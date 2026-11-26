#![cfg(feature = "sqlite")]
//! Executable VM PerformAsync -> durable coordinator -> file-backed libSQL.
//!
//! The fixture installs a trusted host callback explicitly. It does NOT mean
//! arbitrary actor-runtime callbacks are durable or that the bytecode artifact
//! has been independently authenticated. Provider state is an in-process
//! stand-in for a remote idempotency service. The subprocess case separately
//! kills a real VM process after its provider ledger is durably written.

use nulang::authority::{AuthorityGrant, AuthorityManifest};
use nulang::bytecode::{CodeModule, Constant};
use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
use nulang::durable_effect_runtime::{DurableEffectCoordinator, DurableVmAsyncInvocation};
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::heap::{ActorHeap, TypeTag as HeapTypeTag};
use nulang::runtime::{
    Actor, ActorSnapshot, DurableTransition, JournalEntry, LibsqlStore, OrcaGc, PersistenceStore,
    WorkflowActivationId, WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use nulang::types::{NuError, NuResult, Span};
use nulang::vm::{ActorVmCallbacks, PerformAsyncResult, Value, VM};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ACTOR_ID: u64 = 42;
const EXECUTION_KEY: &str = "checkout/order-42/turn-1";
const REQUEST: &str = "order=42&cents=100";
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
const CRASH_TIMEOUT: Duration = Duration::from_secs(20);

struct TestDb {
    dir: PathBuf,
    path: PathBuf,
    owns_directory: bool,
}

impl TestDb {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nulang-vm-durable-bridge-{}-{nonce}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&dir).expect("isolated test directory");
        let path = dir.join("effects.db");
        Self {
            dir,
            path,
            owns_directory: true,
        }
    }

    fn from_existing(path: PathBuf) -> Self {
        Self {
            dir: path.parent().expect("parent directory").to_path_buf(),
            path,
            owns_directory: false,
        }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if self.owns_directory {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

fn compile_module() -> CodeModule {
    let source =
        "actor Payments { behavior charge(order: String) { perform Inference.ask(order) } }";
    compile_source_module(source)
}

fn compile_source_module(source: &str) -> CodeModule {
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut types = TypeChecker::new();
    types.check_module(&ast).unwrap();
    let hir = hir_lower::lower_module(&ast, &types.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).unwrap();
    let compiled = mir_codegen::compile_mir(&mut mir, "durable-vm-bridge").unwrap();
    let bytes = compiled.to_nbc(None).unwrap();
    let module = CodeModule::from_nbc(&bytes).unwrap().module;
    module
}

fn grant() -> AuthorityGrant {
    AuthorityGrant::Other {
        namespace: "Inference".into(),
        operation: "ask".into(),
        argument: None,
    }
}

fn actor(authorized: bool, epoch: u64) -> Actor {
    let mut actor = Actor::new(ACTOR_ID, "Payments", 16);
    actor.persistent = true;
    actor.activation_epoch = epoch;
    let grants = if authorized { vec![grant()] } else { vec![] };
    actor.install_authority_manifest(&AuthorityManifest::from_grants(grants));
    actor
}

#[derive(Default)]
struct Provider {
    attempts: usize,
    mutations: usize,
    committed: BTreeMap<String, Vec<u8>>,
    lose_first_response: bool,
    /// Optional file-backed fake for subprocess crash tests.
    disk_path: Option<PathBuf>,
    /// Pause after the provider's durable commit until the parent kills us.
    pause_after_commit: bool,
    /// Test the adapter's text-only receipt contract.
    invalid_utf8_response: bool,
}

struct Callback {
    store: LibsqlStore,
    actor: Actor,
    provider: Rc<RefCell<Provider>>,
    observed: Rc<RefCell<Vec<String>>>,
    heap: ActorHeap,
    gc: OrcaGc,
}

impl fmt::Debug for Callback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DurableVmFixtureCallback")
            .field("actor_id", &self.actor.id)
            .finish_non_exhaustive()
    }
}

impl Callback {
    fn new(
        db: &TestDb,
        provider: Rc<RefCell<Provider>>,
        observed: Rc<RefCell<Vec<String>>>,
        authorized: bool,
        epoch: u64,
    ) -> Self {
        let mut heap = ActorHeap::new(1024 * 1024);
        heap.set_actor_id(ACTOR_ID);
        Self {
            store: LibsqlStore::new(&db.path).expect("open file-backed store"),
            actor: actor(authorized, epoch),
            provider,
            observed,
            heap,
            gc: OrcaGc::new(ACTOR_ID),
        }
    }
}

impl ActorVmCallbacks for Callback {
    fn current_actor_id(&self) -> Option<u64> {
        Some(ACTOR_ID)
    }

    fn alloc(&mut self, size: usize, type_tag: HeapTypeTag) -> Option<*mut u8> {
        self.heap.alloc(size, type_tag)
    }

    fn drop_ref(&mut self, ptr: *mut u8) {
        unsafe { self.gc.drop_local_ref(&mut self.heap, ptr) };
    }

    fn retain_ref(&mut self, ptr: *mut u8) {
        unsafe { self.gc.local_ref(&self.heap, ptr) };
    }

    fn array_len(&self, ptr: *mut u8) -> Option<usize> {
        unsafe {
            let header = &*ActorHeap::header_of(ptr);
            if header.type_tag == HeapTypeTag::Array {
                Some((header.size - ActorHeap::HEADER_SIZE) / std::mem::size_of::<Value>())
            } else {
                None
            }
        }
    }

    fn spawn_actor(
        &mut self,
        _module: &CodeModule,
        _spawn_pc: usize,
        _behavior_idx: usize,
        _init: Vec<(String, Value)>,
    ) -> Value {
        Value::actor_ref(0)
    }

    fn send_message(&mut self, _target: Value, _behavior_id: u16, _args: &[Value]) {}

    fn perform_async_at_site(
        &mut self,
        module: &CodeModule,
        pc: usize,
        effect_op: &str,
        args: &[Value],
    ) -> NuResult<PerformAsyncResult> {
        // The trusted host defines a canonical byte-encoding of the provider
        // request. Never derive a durable key from transient VM register bits.
        let request = args
            .first()
            .and_then(Value::as_string_id)
            .and_then(|id| module.constants.get(id as usize))
            .and_then(|c| match c {
                Constant::String(s) => Some(s.as_bytes()),
                _ => None,
            })
            .ok_or_else(|| NuError::VMError {
                msg: "invalid durable provider request".into(),
                span: Span::default(),
            })?;

        let mut coordinator =
            DurableEffectCoordinator::new(&mut self.store, ACTOR_ID, self.actor.activation_epoch);
        let dispatch = |key: &str, request: &[u8]| {
            let mut provider = self.provider.borrow_mut();
            provider.attempts += 1;
            let response = if let Some(path) = provider.disk_path.clone() {
                // The subprocess fake models a provider-side idempotency
                // ledger independent of the Nulang effect journal.
                if path.try_exists()? {
                    let saved = fs::read(&path)?;
                    let split = saved
                        .iter()
                        .position(|byte| *byte == b'\n')
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "invalid provider receipt record",
                            )
                        })?;
                    if &saved[..split] != key.as_bytes() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "provider idempotency key changed on retry",
                        ));
                    }
                    saved[split + 1..].to_vec()
                } else {
                    let response = [b"receipt:".as_slice(), request].concat();
                    let mut file = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)?;
                    file.write_all(key.as_bytes())?;
                    file.write_all(b"\n")?;
                    file.write_all(&response)?;
                    file.sync_all()?;
                    provider.mutations += 1;
                    if provider.pause_after_commit {
                        // Publish the checkpoint only after the provider
                        // ledger has been synced. The parent sends SIGKILL
                        // while the VM is still inside external dispatch.
                        drop(file);
                        let pending = path.with_file_name("provider-ready.pending");
                        let ready = path.with_file_name("provider-ready");
                        let mut marker = fs::File::create(&pending)?;
                        marker.write_all(b"provider-committed")?;
                        marker.sync_all()?;
                        drop(marker);
                        fs::rename(pending, ready)?;
                        loop {
                            std::thread::park();
                        }
                    }
                    response
                }
            } else if let Some(saved) = provider.committed.get(key) {
                saved.clone()
            } else {
                provider.mutations += 1;
                let response = if provider.invalid_utf8_response {
                    vec![0xff]
                } else {
                    [b"receipt:".as_slice(), request].concat()
                };
                provider.committed.insert(key.to_owned(), response.clone());
                response
            };
            if provider.lose_first_response {
                provider.lose_first_response = false;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "provider reply lost",
                ));
            }
            Ok(response)
        };
        let authority = grant();
        let result = if self.actor.is_workflow {
            coordinator.execute_workflow_vm_async_text_with_deduplication(
                module,
                pc,
                effect_op,
                &mut self.actor,
                &authority,
                request,
                dispatch,
            )
        } else {
            coordinator.execute_vm_async_text_with_deduplication(
                module,
                pc,
                effect_op,
                DurableVmAsyncInvocation {
                    actor: &self.actor,
                    authority: &authority,
                    execution_key: EXECUTION_KEY,
                    occurrence_index: 0,
                    request,
                },
                dispatch,
            )
        }?;
        if let PerformAsyncResult::Ready(Some(content)) = &result {
            self.observed.borrow_mut().push(content.clone());
        }
        Ok(result)
    }
}

fn execute_vm(
    db: &TestDb,
    provider: Rc<RefCell<Provider>>,
    observed: Rc<RefCell<Vec<String>>>,
    authorized: bool,
    epoch: u64,
    request: &str,
    remove_site: bool,
) -> NuResult<()> {
    let mut module = compile_module();
    assert_eq!(module.effect_sites.len(), 1);
    assert_eq!(module.behaviors.len(), 1);
    let behavior_pc = module.behaviors[0].code_offset;
    let string_id = module.add_string_constant(request);
    if remove_site {
        module.effect_sites.clear();
    }
    let mut vm = VM::new_without_jit();
    vm.load_module(module);
    vm.set_actor_callbacks(Box::new(Callback::new(
        db, provider, observed, authorized, epoch,
    )));
    vm.call_function(0, behavior_pc, &[Value::string(string_id as u32)])?;
    Ok(())
}

#[test]
fn real_vm_retries_prepared_effect_after_store_reopen_with_same_provider_key() {
    let db = TestDb::new();
    let provider = Rc::new(RefCell::new(Provider {
        lose_first_response: true,
        ..Provider::default()
    }));
    let observed = Rc::new(RefCell::new(Vec::new()));

    assert!(execute_vm(
        &db,
        provider.clone(),
        observed.clone(),
        true,
        1,
        REQUEST,
        false
    )
    .is_err());
    assert_eq!(
        LibsqlStore::new(&db.path)
            .unwrap()
            .latest_sequence(ACTOR_ID),
        1
    );
    assert_eq!(provider.borrow().attempts, 1);
    assert_eq!(provider.borrow().mutations, 1);

    execute_vm(
        &db,
        provider.clone(),
        observed.clone(),
        true,
        1,
        REQUEST,
        false,
    )
    .unwrap();
    assert_eq!(
        LibsqlStore::new(&db.path)
            .unwrap()
            .latest_sequence(ACTOR_ID),
        2
    );
    assert_eq!(provider.borrow().attempts, 2);
    assert_eq!(provider.borrow().mutations, 1);
    assert_eq!(
        observed.borrow().as_slice(),
        &["receipt:order=42&cents=100"]
    );

    // A completed receipt is returned without any external provider call,
    // even after both VM and persistence store are recreated again.
    execute_vm(
        &db,
        provider.clone(),
        observed.clone(),
        true,
        1,
        REQUEST,
        false,
    )
    .unwrap();
    assert_eq!(
        LibsqlStore::new(&db.path)
            .unwrap()
            .latest_sequence(ACTOR_ID),
        2
    );
    assert_eq!(provider.borrow().attempts, 2);
    assert_eq!(observed.borrow().len(), 2);
}

#[test]
fn vm_replay_rejects_revoked_grant_request_drift_and_missing_site() {
    let db = TestDb::new();
    let provider = Rc::new(RefCell::new(Provider::default()));
    let observed = Rc::new(RefCell::new(Vec::new()));
    execute_vm(
        &db,
        provider.clone(),
        observed.clone(),
        true,
        1,
        REQUEST,
        false,
    )
    .unwrap();

    assert!(execute_vm(
        &db,
        provider.clone(),
        observed.clone(),
        false,
        1,
        REQUEST,
        false
    )
    .is_err());
    assert!(execute_vm(
        &db,
        provider.clone(),
        observed.clone(),
        true,
        1,
        "tampered",
        false
    )
    .is_err());
    assert!(execute_vm(
        &db,
        provider.clone(),
        observed.clone(),
        true,
        1,
        REQUEST,
        true
    )
    .is_err());
    assert_eq!(provider.borrow().attempts, 1);
    assert_eq!(
        LibsqlStore::new(&db.path)
            .unwrap()
            .latest_sequence(ACTOR_ID),
        2
    );
    assert_eq!(observed.borrow().len(), 1);
}

#[test]
fn vm_prepared_retry_rejects_stale_activation_after_new_owner_claims() {
    let db = TestDb::new();
    let provider = Rc::new(RefCell::new(Provider {
        lose_first_response: true,
        ..Provider::default()
    }));
    let observed = Rc::new(RefCell::new(Vec::new()));
    assert!(execute_vm(
        &db,
        provider.clone(),
        observed.clone(),
        true,
        1,
        REQUEST,
        false
    )
    .is_err());

    {
        let mut store = LibsqlStore::new(&db.path).unwrap();
        let spec = DurableEffectSpec::new(
            DurableEffectId::derive(ACTOR_ID, "new-owner-epoch-claim", 0, "Test.claim"),
            "Test.claim",
            EffectBoundary::External,
            DeliverySemantics::AtLeastOnce,
        );
        let mut current = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 2);
        current.begin(spec, b"claim").unwrap();
    }
    assert_eq!(
        LibsqlStore::new(&db.path)
            .unwrap()
            .latest_sequence(ACTOR_ID),
        2
    );
    assert!(execute_vm(
        &db,
        provider.clone(),
        observed.clone(),
        true,
        1,
        REQUEST,
        false
    )
    .is_err());
    assert_eq!(provider.borrow().attempts, 1);
    assert_eq!(provider.borrow().mutations, 1);
    assert!(observed.borrow().is_empty());
    assert_eq!(
        LibsqlStore::new(&db.path)
            .unwrap()
            .latest_sequence(ACTOR_ID),
        2
    );
}

#[test]
fn vm_invalid_text_receipt_fails_closed_and_never_redispatches() {
    let db = TestDb::new();
    let provider = Rc::new(RefCell::new(Provider {
        invalid_utf8_response: true,
        ..Provider::default()
    }));
    let observed = Rc::new(RefCell::new(Vec::new()));

    // The provider mutation and receipt are already durable by the time the
    // text-only adapter detects an invalid UTF-8 result. Replay must keep
    // failing closed and never call the provider again.
    for _ in 0..2 {
        let error = execute_vm(
            &db,
            provider.clone(),
            observed.clone(),
            true,
            1,
            REQUEST,
            false,
        )
        .unwrap_err();
        assert!(
            matches!(
                &error,
                NuError::VMError { msg, .. }
                    if msg.lines().next() == Some("durable VM effect receipt is not valid UTF-8")
            ),
            "unexpected VM error: {error:?}"
        );
        assert_eq!(provider.borrow().attempts, 1);
        assert_eq!(provider.borrow().mutations, 1);
        assert!(observed.borrow().is_empty());
        assert_eq!(
            LibsqlStore::new(&db.path)
                .unwrap()
                .latest_sequence(ACTOR_ID),
            2,
            "terminal invalid receipt must remain durable across recovery"
        );
    }
}

fn admit_command(store: &mut LibsqlStore) -> WorkflowActivationId {
    let previous = store.latest_sequence(ACTOR_ID);
    let sequence = previous + 1;
    let committed = store
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: ACTOR_ID,
            activation_epoch: 1,
            sequence,
            expected_previous_sequence: previous,
            command: Some(JournalEntry {
                sequence,
                behavior_id: 0,
                payload: vec![],
            }),
            snapshot: None,
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .unwrap();
    WorkflowActivationId::new(committed.actor_id, committed.sequence)
}

fn workflow_actor(activation: WorkflowActivationId) -> Actor {
    let mut actor = actor(true, 1);
    actor.is_workflow = true;
    actor.current_workflow_activation = Some(activation);
    actor.sequence = activation.command_sequence;
    actor
}

fn workflow_module() -> CodeModule {
    let module = compile_source_module(
        "workflow Payments { step charge { perform Inference.ask(\"order=42&cents=100\") } }",
    );
    assert_eq!(module.effect_sites.len(), 1);
    module
}

fn workflow_fixture_vm(
    db: &TestDb,
    provider: Rc<RefCell<Provider>>,
    observed: Rc<RefCell<Vec<String>>>,
    activation: WorkflowActivationId,
) -> (VM, usize) {
    let module = workflow_module();
    let behavior_pc = module
        .behaviors
        .iter()
        .find(|behavior| behavior.name == "charge" || behavior.name.ends_with(".charge"))
        .expect("compiled workflow step")
        .code_offset;
    let mut callback = Callback::new(db, provider, observed, true, 1);
    callback.actor = workflow_actor(activation);
    let mut vm = VM::new_without_jit();
    vm.load_module(module);
    vm.set_actor_callbacks(Box::new(callback));
    (vm, behavior_pc)
}

#[test]
fn workflow_vm_occurrences_reconstruct_after_reopen_and_retry() {
    let db = TestDb::new();
    let activation = admit_command(&mut LibsqlStore::new(&db.path).unwrap());
    let provider = Rc::new(RefCell::new(Provider {
        lose_first_response: true,
        ..Provider::default()
    }));
    let observed = Rc::new(RefCell::new(Vec::new()));
    {
        let (mut vm, pc) = workflow_fixture_vm(&db, provider.clone(), observed.clone(), activation);
        // First occurrence has durable intent and an indeterminate response.
        assert!(vm.call_function(0, pc, &[]).is_err());
        assert_eq!(provider.borrow().committed.len(), 1);
        assert_eq!(
            LibsqlStore::new(&db.path)
                .unwrap()
                .latest_sequence(ACTOR_ID),
            2
        );
        // Same actor/VM retry consumes the first occurrence, not a new one.
        vm.call_function(0, pc, &[]).unwrap();
        assert_eq!(provider.borrow().mutations, 1);
        // The same static site reached again within this activation is distinct.
        vm.call_function(0, pc, &[]).unwrap();
        assert_eq!(provider.borrow().attempts, 3);
        assert_eq!(provider.borrow().mutations, 2);
        assert_eq!(provider.borrow().committed.len(), 2);
        assert_eq!(
            LibsqlStore::new(&db.path)
                .unwrap()
                .latest_sequence(ACTOR_ID),
            5
        );
    }

    // The admitted command is persisted; recover its identity from the journal
    // rather than supplying the old free-form execution key or occurrence.
    let reopened = LibsqlStore::new(&db.path).unwrap();
    let commands = reopened.read_journal(ACTOR_ID);
    assert_eq!(commands.len(), 1);
    let recovered = WorkflowActivationId::new(ACTOR_ID, commands[0].sequence);
    assert_eq!(recovered, activation);
    drop(reopened);
    let (mut vm, pc) = workflow_fixture_vm(&db, provider.clone(), observed.clone(), recovered);
    vm.call_function(0, pc, &[]).unwrap();
    vm.call_function(0, pc, &[]).unwrap();
    assert_eq!(provider.borrow().attempts, 3);
    assert_eq!(provider.borrow().mutations, 2);
    assert_eq!(observed.borrow().len(), 4);
    assert_eq!(
        LibsqlStore::new(&db.path)
            .unwrap()
            .latest_sequence(ACTOR_ID),
        5
    );
}

#[test]
fn workflow_cursor_rejects_unadmitted_wrong_actor_and_stale_owners() {
    let db = TestDb::new();
    let mut store = LibsqlStore::new(&db.path).unwrap();
    let module = workflow_module();
    let pc = module.effect_sites[0].pc;
    let mut actor = workflow_actor(WorkflowActivationId::new(ACTOR_ID, 1));
    let denied = |_: &str, _: &[u8]| -> io::Result<Vec<u8>> {
        panic!("rejected workflow invocation must not dispatch")
    };
    // A fabricated activation with no atomic tail is not accepted.
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert!(coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut actor,
            &grant(),
            REQUEST.as_bytes(),
            denied
        )
        .is_err());
    assert_eq!(store.latest_sequence(ACTOR_ID), 0);

    let activation = admit_command(&mut store);
    actor.current_workflow_activation = Some(WorkflowActivationId::new(
        ACTOR_ID + 1,
        activation.command_sequence,
    ));
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert!(coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut actor,
            &grant(),
            REQUEST.as_bytes(),
            denied
        )
        .is_err());
    actor.current_workflow_activation = None;
    assert!(coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut actor,
            &grant(),
            REQUEST.as_bytes(),
            denied
        )
        .is_err());
    actor.current_workflow_activation = Some(activation);
    actor.activation_epoch = 2;
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 2);
    assert!(coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut actor,
            &grant(),
            REQUEST.as_bytes(),
            denied
        )
        .is_err());
    assert_eq!(store.latest_sequence(ACTOR_ID), 1);
}

#[test]
fn workflow_cursor_pins_pending_request_and_separates_new_commands() {
    let db = TestDb::new();
    let mut store = LibsqlStore::new(&db.path).unwrap();
    let module = workflow_module();
    let pc = module.effect_sites[0].pc;
    let activation = admit_command(&mut store);
    let mut actor = workflow_actor(activation);
    let mut keys = Vec::new();
    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        assert!(coordinator
            .execute_workflow_vm_async_text_with_deduplication(
                &module,
                pc,
                "Inference.ask",
                &mut actor,
                &grant(),
                REQUEST.as_bytes(),
                |key, _| {
                    keys.push(key.to_owned());
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "indeterminate provider reply",
                    ))
                }
            )
            .is_err());
        assert!(coordinator
            .execute_workflow_vm_async_text_with_deduplication(
                &module,
                pc,
                "Inference.ask",
                &mut actor,
                &grant(),
                b"changed",
                |_, _| panic!("request drift must not dispatch")
            )
            .is_err());
        // A matching retry preserves the key and completes the occurrence.
        coordinator
            .execute_workflow_vm_async_text_with_deduplication(
                &module,
                pc,
                "Inference.ask",
                &mut actor,
                &grant(),
                REQUEST.as_bytes(),
                |key, _| {
                    keys.push(key.to_owned());
                    Ok(b"receipt".to_vec())
                },
            )
            .unwrap();
    }
    assert_eq!(keys[0], keys[1]);
    let terminal_seq = store.latest_sequence(ACTOR_ID) + 1;
    store
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: ACTOR_ID,
            activation_epoch: 1,
            sequence: terminal_seq,
            expected_previous_sequence: terminal_seq - 1,
            command: None,
            snapshot: Some(ActorSnapshot {
                actor_id: ACTOR_ID,
                sequence: terminal_seq,
                activation_epoch: 1,
                ..ActorSnapshot::default()
            }),
            workflow_events: vec![WorkflowEvent::StepCompleted {
                sequence: terminal_seq,
                activation: Some(activation),
                step_name: "charge".into(),
            }],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .unwrap();
    let next = admit_command(&mut store);
    actor.current_workflow_activation = Some(next);
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut actor,
            &grant(),
            REQUEST.as_bytes(),
            |key, _| {
                keys.push(key.to_owned());
                Ok(b"another receipt".to_vec())
            },
        )
        .unwrap();
    assert_ne!(keys[1], keys[2]);
    actor.current_workflow_activation = Some(activation);
    assert!(coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut actor,
            &grant(),
            REQUEST.as_bytes(),
            |_, _| panic!("old command must not dispatch")
        )
        .is_err());
    assert_eq!(store.latest_sequence(ACTOR_ID), 7);
}

/// Guarantee the test reaps its worker even if a checkpoint assertion panics.
struct CrashWorker(Child);

impl Drop for CrashWorker {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn process_kill_after_provider_commit_restarts_vm_with_one_external_mutation() {
    let db = TestDb::new();
    let receipt_path = db.dir.join("provider-receipt");
    let ready_path = db.dir.join("provider-ready");
    let mut child = CrashWorker(
        Command::new(std::env::current_exe().expect("integration test binary"))
            .arg("--exact")
            .arg("durable_vm_crash_fixture_child")
            .arg("--ignored")
            .arg("--nocapture")
            .env("NULANG_VM_BRIDGE_DB_PATH", &db.path)
            .env("NULANG_VM_BRIDGE_PROVIDER_PATH", &receipt_path)
            .spawn()
            .expect("launch isolated VM subprocess"),
    );
    let deadline = Instant::now() + CRASH_TIMEOUT;
    loop {
        if ready_path.try_exists().expect("inspect crash checkpoint") {
            assert_eq!(
                fs::read(&ready_path).expect("read checkpoint"),
                b"provider-committed"
            );
            break;
        }
        if let Some(status) = child.0.try_wait().expect("poll child") {
            panic!("VM worker exited before provider commit: {status}");
        }
        assert!(Instant::now() < deadline, "VM crash worker timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    child
        .0
        .kill()
        .expect("kill VM while provider dispatch is paused");
    let status = child.0.wait().expect("reap terminated VM");
    assert!(!status.success(), "child must not exit normally");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }
    assert!(
        receipt_path.is_file(),
        "provider must durably commit before exit"
    );
    assert_eq!(
        LibsqlStore::new(&db.path)
            .unwrap()
            .latest_sequence(ACTOR_ID),
        1,
        "prepared intent must precede provider commit and survive SIGKILL"
    );

    // The parent is a separate process from the killed child. Recover with a
    // fresh VM, callback and database connection. The external
    // provider's idempotency ledger survives independently on disk.
    let provider = Rc::new(RefCell::new(Provider {
        disk_path: Some(receipt_path),
        ..Provider::default()
    }));
    let observed = Rc::new(RefCell::new(Vec::new()));
    execute_vm(
        &db,
        provider.clone(),
        observed.clone(),
        true,
        1,
        REQUEST,
        false,
    )
    .unwrap();
    assert_eq!(provider.borrow().attempts, 1);
    assert_eq!(
        provider.borrow().mutations,
        0,
        "the persisted provider result must be reused"
    );
    assert_eq!(
        observed.borrow().as_slice(),
        &["receipt:order=42&cents=100"]
    );
    assert_eq!(
        LibsqlStore::new(&db.path)
            .unwrap()
            .latest_sequence(ACTOR_ID),
        2
    );

    execute_vm(&db, provider.clone(), observed, true, 1, REQUEST, false).unwrap();
    assert_eq!(
        provider.borrow().attempts,
        1,
        "completed VM replay must not call provider"
    );
    assert_eq!(
        LibsqlStore::new(&db.path)
            .unwrap()
            .latest_sequence(ACTOR_ID),
        2
    );
}

#[test]
#[ignore = "subprocess crash fixture; invoked only by the parent test"]
fn durable_vm_crash_fixture_child() {
    let db = TestDb::from_existing(PathBuf::from(
        std::env::var_os("NULANG_VM_BRIDGE_DB_PATH").expect("parent db path"),
    ));
    let provider_path = PathBuf::from(
        std::env::var_os("NULANG_VM_BRIDGE_PROVIDER_PATH").expect("parent provider path"),
    );
    let provider = Rc::new(RefCell::new(Provider {
        disk_path: Some(provider_path),
        pause_after_commit: true,
        ..Provider::default()
    }));
    let observed = Rc::new(RefCell::new(Vec::new()));
    let result = execute_vm(&db, provider, observed, true, 1, REQUEST, false);
    panic!("fixture must be killed while provider receipt is not yet acknowledged: {result:?}");
}

#[test]
fn workflow_later_occurrence_recovers_after_new_owner_claim() {
    let db = TestDb::new();
    let mut store = LibsqlStore::new(&db.path).unwrap();
    let module = workflow_module();
    let pc = module.effect_sites[0].pc;
    let activation = admit_command(&mut store);
    let mut old = workflow_actor(activation);
    let mut keys = Vec::new();
    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        coordinator
            .execute_workflow_vm_async_text_with_deduplication(
                &module,
                pc,
                "Inference.ask",
                &mut old,
                &grant(),
                REQUEST.as_bytes(),
                |key, _| {
                    keys.push(key.to_owned());
                    Ok(b"first".to_vec())
                },
            )
            .unwrap();
        assert!(coordinator
            .execute_workflow_vm_async_text_with_deduplication(
                &module,
                pc,
                "Inference.ask",
                &mut old,
                &grant(),
                REQUEST.as_bytes(),
                |key, _| {
                    keys.push(key.to_owned());
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "second provider reply lost",
                    ))
                }
            )
            .is_err());
        // A different semantic site must not take over the pending local slot.
        let alternate = compile_source_module(
            r#"workflow Other { step charge { perform Inference.ask("order=42&cents=100") } }"#,
        );
        assert_ne!(module.effect_sites[0].id, alternate.effect_sites[0].id);
        assert!(coordinator
            .execute_workflow_vm_async_text_with_deduplication(
                &alternate,
                alternate.effect_sites[0].pc,
                "Inference.ask",
                &mut old,
                &grant(),
                REQUEST.as_bytes(),
                |_, _| panic!("changed pending site must not dispatch")
            )
            .is_err());
    }
    assert_ne!(keys[0], keys[1]);
    assert_eq!(store.latest_sequence(ACTOR_ID), 4);
    // An empty atomic transition claims a newer epoch in this controlled
    // fixture. The accepted command sequence is unchanged.
    store
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: ACTOR_ID,
            activation_epoch: 2,
            sequence: 5,
            expected_previous_sequence: 4,
            command: None,
            snapshot: None,
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .unwrap();
    let mut stale = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert!(stale
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut old,
            &grant(),
            REQUEST.as_bytes(),
            |_, _| panic!("stale owner must not dispatch")
        )
        .is_err());
    drop(store);

    let mut store = LibsqlStore::new(&db.path).unwrap();
    let commands = store.read_journal(ACTOR_ID);
    assert_eq!(commands.len(), 1);
    let recovered = WorkflowActivationId::new(ACTOR_ID, commands[0].sequence);
    let mut current = workflow_actor(recovered);
    current.activation_epoch = 2;
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 2);
    let first = coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut current,
            &grant(),
            REQUEST.as_bytes(),
            |_, _| panic!("first completed occurrence must replay, not dispatch"),
        )
        .unwrap();
    assert!(matches!(first, PerformAsyncResult::Ready(Some(ref text)) if text == "first"));
    coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut current,
            &grant(),
            REQUEST.as_bytes(),
            |key, _| {
                keys.push(key.to_owned());
                Ok(b"second".to_vec())
            },
        )
        .unwrap();
    assert_eq!(
        keys[1], keys[2],
        "epoch is a fence, not logical effect identity"
    );
    let canonical = |ordinal| {
        DurableEffectSpec::from_compiled_async_site(
            &module,
            pc,
            ACTOR_ID,
            &format!("workflow-command:v1:{}", recovered.command_sequence),
            ordinal,
            EffectBoundary::External,
            DeliverySemantics::EffectivelyOnceWithDeduplication,
        )
        .unwrap()
        .id
        .idempotency_key()
    };
    assert_eq!(keys[0], canonical(0));
    assert_eq!(keys[1], canonical(1));
    assert_eq!(store.latest_sequence(ACTOR_ID), 6);
}

#[test]
fn workflow_restart_rejects_changed_site_before_provider_and_replays_original_slot() {
    let db = TestDb::new();
    let mut store = LibsqlStore::new(&db.path).unwrap();
    let module = workflow_module();
    let pc = module.effect_sites[0].pc;
    let alternate = compile_source_module(
        r#"workflow Other { step charge { perform Inference.ask("order=42&cents=100") } }"#,
    );
    assert_ne!(module.effect_sites[0].id, alternate.effect_sites[0].id);
    let activation = admit_command(&mut store);
    let mut actor = workflow_actor(activation);
    let mut calls = Vec::new();
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert!(coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut actor,
            &grant(),
            REQUEST.as_bytes(),
            |key, _| {
                calls.push(key.to_owned());
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "provider reply lost",
                ))
            }
        )
        .is_err());
    assert_eq!(store.latest_sequence(ACTOR_ID), 2);
    drop(store);

    // A fresh actor has no pending in-memory cursor. Only persisted binding
    // prevents the changed site from being treated as a new logical effect.
    let mut store = LibsqlStore::new(&db.path).unwrap();
    let prepared = store
        .load_workflow_effect_occurrence(activation, 0)
        .unwrap()
        .unwrap();
    let binding = prepared.workflow_binding().unwrap();
    assert_eq!(binding.site_id, module.effect_sites[0].id);
    assert_eq!(binding.effect_id().idempotency_key(), calls[0]);
    let mut fresh = workflow_actor(activation);
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert!(coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &alternate,
            alternate.effect_sites[0].pc,
            "Inference.ask",
            &mut fresh,
            &grant(),
            REQUEST.as_bytes(),
            |_, _| panic!("a different site after process restart must not dispatch")
        )
        .is_err());
    assert_eq!(store.latest_sequence(ACTOR_ID), 2);

    // Generic callers may not strip workflow metadata or replay a bound
    // slot through the older free-form key adapter.
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert!(coordinator
        .begin(prepared.effect().spec().clone(), REQUEST.as_bytes())
        .is_err());
    assert!(coordinator
        .complete(binding.effect_id(), REQUEST.as_bytes(), b"forged".to_vec())
        .is_err());
    let mut correct = workflow_actor(activation);
    assert!(coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut correct,
            &grant(),
            b"changed",
            |_, _| panic!("a changed request after restart must not dispatch")
        )
        .is_err());
    let mut correct = workflow_actor(activation);
    coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut correct,
            &grant(),
            REQUEST.as_bytes(),
            |key, _| {
                calls.push(key.to_owned());
                Ok(b"receipt".to_vec())
            },
        )
        .unwrap();
    assert_eq!(calls[0], calls[1]);
    assert_eq!(store.latest_sequence(ACTOR_ID), 3);
    drop(store);

    // Terminal history retains the binding as well as the receipt.
    let mut store = LibsqlStore::new(&db.path).unwrap();
    let completed = store
        .load_workflow_effect_occurrence(activation, 0)
        .unwrap()
        .unwrap();
    assert_eq!(completed.workflow_binding(), Some(binding));
    let mut wrong = workflow_actor(activation);
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert!(coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &alternate,
            alternate.effect_sites[0].pc,
            "Inference.ask",
            &mut wrong,
            &grant(),
            REQUEST.as_bytes(),
            |_, _| panic!("site drift after terminal receipt must fail closed")
        )
        .is_err());
    let mut replayed = workflow_actor(activation);
    let result = coordinator
        .execute_workflow_vm_async_text_with_deduplication(
            &module,
            pc,
            "Inference.ask",
            &mut replayed,
            &grant(),
            REQUEST.as_bytes(),
            |_, _| panic!("recorded terminal workflow receipt must replay"),
        )
        .unwrap();
    assert!(matches!(result, PerformAsyncResult::Ready(Some(ref value)) if value == "receipt"));
    assert_eq!(store.latest_sequence(ACTOR_ID), 3);
}
