#![cfg(feature = "sqlite")]
//! Executable VM PerformAsync -> durable coordinator -> file-backed libSQL.
//!
//! The fixture installs a trusted host callback explicitly. It does NOT mean
//! arbitrary actor-runtime callbacks are durable or that the bytecode artifact
//! has been independently authenticated. Provider state is an in-process
//! stand-in for a remote idempotency service; reopening the database is not
//! the same as a separate process being killed.

use nulang::authority::{AuthorityGrant, AuthorityManifest};
use nulang::bytecode::{CodeModule, Constant};
use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
use nulang::durable_effect_runtime::{
    DurableEffectCoordinator, DurableVmAsyncInvocation,
};
use nulang::hir_lower;
use nulang::lexer::Lexer;
use nulang::mir_codegen;
use nulang::mir_lower;
use nulang::parser::Parser;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::heap::{ActorHeap, TypeTag as HeapTypeTag};
use nulang::runtime::{Actor, LibsqlStore, OrcaGc, PersistenceStore};
use nulang::typechecker::TypeChecker;
use nulang::types::{NuError, NuResult, Span};
use nulang::vm::{ActorVmCallbacks, PerformAsyncResult, Value, VM};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const ACTOR_ID: u64 = 42;
const EXECUTION_KEY: &str = "checkout/order-42/turn-1";
const REQUEST: &str = "order=42&cents=100";
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    dir: PathBuf,
    path: PathBuf,
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
        Self { dir, path }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn compile_module() -> CodeModule {
    let source =
        "actor Payments { behavior charge(order: String) { perform Inference.ask(order) } }";
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut types = TypeChecker::new();
    types.check_module(&ast).unwrap();
    let hir = hir_lower::lower_module(&ast, &types.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).unwrap();
    let compiled = mir_codegen::compile_mir(&mut mir, "durable-vm-bridge").unwrap();
    let bytes = compiled.to_nbc(None).unwrap();
    let module = CodeModule::from_nbc(&bytes).unwrap().module;
    assert_eq!(module.effect_sites.len(), 1);
    assert_eq!(module.behaviors.len(), 1);
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
    fn new(db: &TestDb, provider: Rc<RefCell<Provider>>, observed: Rc<RefCell<Vec<String>>>,
           authorized: bool, epoch: u64) -> Self {
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

    fn spawn_actor(&mut self, _module: &CodeModule, _spawn_pc: usize,
                   _behavior_idx: usize, _init: Vec<(String, Value)>) -> Value {
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
        let request = args.first()
            .and_then(Value::as_string_id)
            .and_then(|id| module.constants.get(id as usize))
            .and_then(|c| match c { Constant::String(s) => Some(s.as_bytes()), _ => None })
            .ok_or_else(|| NuError::VMError {
                msg: "invalid durable provider request".into(),
                span: Span::default(),
            })?;

        let invocation = DurableVmAsyncInvocation {
            actor: &self.actor,
            authority: &grant(),
            execution_key: EXECUTION_KEY,
            occurrence_index: 0,
            request,
        };
        let mut coordinator =
            DurableEffectCoordinator::new(&mut self.store, ACTOR_ID, self.actor.activation_epoch);
        let result = coordinator.execute_vm_async_text_with_deduplication(
            module,
            pc,
            effect_op,
            invocation,
            |key, request| {
                let mut provider = self.provider.borrow_mut();
                provider.attempts += 1;
                let response = if let Some(saved) = provider.committed.get(key) {
                    saved.clone()
                } else {
                    provider.mutations += 1;
                    let response = [b"receipt:".as_slice(), request].concat();
                    provider.committed.insert(key.to_owned(), response.clone());
                    response
                };
                if provider.lose_first_response {
                    provider.lose_first_response = false;
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "provider reply lost"));
                }
                Ok(response)
            },
        )?;
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
    let behavior_pc = module.behaviors[0].code_offset;
    let string_id = module.add_string_constant(request);
    if remove_site {
        module.effect_sites.clear();
    }
    let mut vm = VM::new_without_jit();
    vm.load_module(module);
    vm.set_actor_callbacks(Box::new(Callback::new(db, provider, observed, authorized, epoch)));
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

    assert!(execute_vm(&db, provider.clone(), observed.clone(), true, 1, REQUEST, false).is_err());
    assert_eq!(LibsqlStore::new(&db.path).unwrap().latest_sequence(ACTOR_ID), 1);
    assert_eq!(provider.borrow().attempts, 1);
    assert_eq!(provider.borrow().mutations, 1);

    execute_vm(&db, provider.clone(), observed.clone(), true, 1, REQUEST, false).unwrap();
    assert_eq!(LibsqlStore::new(&db.path).unwrap().latest_sequence(ACTOR_ID), 2);
    assert_eq!(provider.borrow().attempts, 2);
    assert_eq!(provider.borrow().mutations, 1);
    assert_eq!(observed.borrow().as_slice(), &["receipt:order=42&cents=100"]);

    // A completed receipt is returned without any external provider call,
    // even after both VM and persistence store are recreated again.
    execute_vm(&db, provider.clone(), observed.clone(), true, 1, REQUEST, false).unwrap();
    assert_eq!(LibsqlStore::new(&db.path).unwrap().latest_sequence(ACTOR_ID), 2);
    assert_eq!(provider.borrow().attempts, 2);
    assert_eq!(observed.borrow().len(), 2);
}

#[test]
fn vm_replay_rejects_revoked_grant_request_drift_and_missing_site() {
    let db = TestDb::new();
    let provider = Rc::new(RefCell::new(Provider::default()));
    let observed = Rc::new(RefCell::new(Vec::new()));
    execute_vm(&db, provider.clone(), observed.clone(), true, 1, REQUEST, false).unwrap();

    assert!(execute_vm(&db, provider.clone(), observed.clone(), false, 1, REQUEST, false).is_err());
    assert!(execute_vm(&db, provider.clone(), observed.clone(), true, 1, "tampered", false).is_err());
    assert!(execute_vm(&db, provider.clone(), observed.clone(), true, 1, REQUEST, true).is_err());
    assert_eq!(provider.borrow().attempts, 1);
    assert_eq!(LibsqlStore::new(&db.path).unwrap().latest_sequence(ACTOR_ID), 2);
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
    assert!(execute_vm(&db, provider.clone(), observed.clone(), true, 1, REQUEST, false).is_err());

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
    assert_eq!(LibsqlStore::new(&db.path).unwrap().latest_sequence(ACTOR_ID), 2);
    assert!(execute_vm(&db, provider.clone(), observed.clone(), true, 1, REQUEST, false).is_err());
    assert_eq!(provider.borrow().attempts, 1);
    assert_eq!(provider.borrow().mutations, 1);
    assert!(observed.borrow().is_empty());
    assert_eq!(LibsqlStore::new(&db.path).unwrap().latest_sequence(ACTOR_ID), 2);
}
