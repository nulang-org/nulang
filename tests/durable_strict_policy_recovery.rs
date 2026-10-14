//! Versioned strict-admission policy must survive cold restarts, and failed
//! policy commits must never turn on the in-memory strict flag.
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{ActorSnapshot, PersistedValue, Runtime, PersistenceStore};
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

fn module() -> nulang::bytecode::CodeModule {
    let source = r#"
        persistent actor Counter {
            state durable total: Int = 0
            behavior add(by: Int) { self.total = self.total + by }
        }
    "#;
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR");
    nulang::mir_codegen::compile_mir(&mut mir, "durable_admission_policy").expect("bytecode")
}

fn typed_actor(rt: &mut Runtime, module: &nulang::bytecode::CodeModule) -> u64 {
    rt.spawn_from_module(module, module.actor_metadata[0].behavior_indices[0], vec![])
        .as_actor_id()
        .expect("persistent Counter actor")
}

fn register(rt: &mut Runtime, id: u64, module: &nulang::bytecode::CodeModule) {
    let offsets = module.behaviors.iter().map(|entry| entry.code_offset).collect();
    let compensations = module.behaviors.iter().map(|entry| entry.compensate_offset).collect();
    rt.register_recovery_module(id, module.clone(), offsets, compensations);
}

#[test]
fn strict_policy_is_atomically_persisted_and_restored_before_actor_publication() {
    let module = module();
    let mut first = Runtime::new();
    let id = typed_actor(&mut first, &module);
    first.enable_strict_typed_actor_admission(id).expect("durable policy transition");
    let snapshot = first.persistence.load_snapshot(id).expect("durable snapshot");
    assert_eq!(snapshot.strict_admission_policy_version, 1);
    assert!(first.actors[&id].mailbox.requires_strict_durable_admission());
    let sequence = first.persistence.latest_sequence(id);
    assert!(sequence > 0);

    let mut second = Runtime::new();
    second.persistence = std::mem::replace(
        &mut first.persistence,
        Box::new(nulang::runtime::MemoryStore::new()),
    );
    register(&mut second, id, &module);
    assert_eq!(second.recover_actor(id), Some(id));
    assert_eq!(second.persistence.latest_sequence(id), sequence);
    assert!(second.actors[&id].mailbox.requires_strict_durable_admission());

    let behavior = module.actor_metadata[0].behavior_indices[0] as u16;
    second.send_message_by_id(id, behavior, &[Value::int(7)]);
    second.run_scheduler();
    assert_eq!(
        second.actors[&id].get_state_field("total").and_then(|v| v.as_int()),
        Some(0)
    );
    assert_eq!(second.actors[&id].mailbox.len(), 1);
}

#[test]
fn legacy_snapshot_without_policy_is_backward_compatible() {
    let encoded = serde_json::json!({
        "actor_id": 77,
        "sequence": 3,
        "activation_epoch": 1,
        "state": {"total": {"Int": 5}}
    });
    let snapshot: ActorSnapshot = serde_json::from_value(encoded).expect("legacy snapshot");
    assert_eq!(snapshot.strict_admission_policy_version, 0);
    let reencoded = serde_json::to_value(&snapshot).expect("reserialize legacy");
    assert!(
        reencoded.get("strict_admission_policy_version").is_none(),
        "legacy policy version 0 must not change historical snapshot digests"
    );
}

#[test]
fn unsupported_policy_version_fails_closed_on_recovery() {
    let module = module();
    let mut rt = Runtime::new();
    let id = 91_234;
    register(&mut rt, id, &module);
    rt.persistence.save_snapshot(ActorSnapshot {
        actor_id: id,
        schema_name: Some("Counter".to_string()),
        state: std::collections::HashMap::from([(
            "total".to_string(),
            PersistedValue::Int(0),
        )]),
        strict_admission_policy_version: 99,
        ..ActorSnapshot::default()
    }).expect("snapshot");
    assert_eq!(rt.recover_actor(id), None);
    assert!(!rt.actors.contains_key(&id));
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_policy_survives_new_database_connection() {
    use nulang::runtime::LibsqlStore;
    let module = module();
    let dir = std::env::temp_dir().join(format!(
        "nulang-strict-policy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("strict.db");
    let id;
    {
        let mut rt = Runtime::new();
        rt.persistence = Box::new(LibsqlStore::new(&path).expect("database"));
        id = typed_actor(&mut rt, &module);
        rt.enable_strict_typed_actor_admission(id).expect("durable policy");
    }
    {
        let mut rt = Runtime::new();
        rt.persistence = Box::new(LibsqlStore::new(&path).expect("reopen database"));
        register(&mut rt, id, &module);
        assert_eq!(rt.recover_actor(id), Some(id));
        assert_eq!(
            rt.persistence.load_snapshot(id).unwrap().strict_admission_policy_version,
            1
        );
        assert!(rt.actors[&id].mailbox.requires_strict_durable_admission());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[derive(Default)]
struct FailPolicyStore {
    inner: nulang::runtime::MemoryStore,
    reject: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PersistenceStore for FailPolicyStore {
    fn commit_transition(
        &mut self,
        transition: nulang::runtime::DurableTransition,
    ) -> std::io::Result<nulang::runtime::DurableCommit> {
        if self.reject.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::other("injected policy commit failure"));
        }
        self.inner.commit_transition(transition)
    }

    fn load_durable_tail_position(
        &self,
        actor_id: u64,
    ) -> std::io::Result<Option<nulang::runtime::DurableTailPosition>> {
        self.inner.load_durable_tail_position(actor_id)
    }

    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> std::io::Result<()> {
        self.inner.save_snapshot(snapshot)
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        self.inner.load_snapshot(actor_id)
    }

    fn append_journal(
        &mut self,
        actor_id: u64,
        entry: nulang::runtime::JournalEntry,
    ) -> std::io::Result<()> {
        self.inner.append_journal(actor_id, entry)
    }

    fn read_journal(&self, actor_id: u64) -> Vec<nulang::runtime::JournalEntry> {
        self.inner.read_journal(actor_id)
    }

    fn append_workflow_event(
        &mut self,
        actor_id: u64,
        event: nulang::runtime::WorkflowEvent,
    ) -> std::io::Result<()> {
        self.inner.append_workflow_event(actor_id, event)
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<nulang::runtime::WorkflowEvent> {
        self.inner.read_workflow_events(actor_id)
    }

    fn append_event(
        &mut self,
        actor_id: u64,
        event: nulang::runtime::EventEntry,
    ) -> std::io::Result<()> {
        self.inner.append_event(actor_id, event)
    }

    fn read_events(&self, actor_id: u64) -> Vec<nulang::runtime::EventEntry> {
        self.inner.read_events(actor_id)
    }

    fn latest_sequence(&self, actor_id: u64) -> u64 {
        self.inner.latest_sequence(actor_id)
    }

    fn clear(&mut self, actor_id: u64) -> std::io::Result<()> {
        self.inner.clear(actor_id)
    }
}

#[test]
fn rejected_policy_commit_never_enables_legacy_bypass_gate() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let module = module();
    let mut rt = Runtime::new();
    let id = typed_actor(&mut rt, &module);
    let reject = Arc::new(AtomicBool::new(true));
    rt.persistence = Box::new(FailPolicyStore {
        inner: nulang::runtime::MemoryStore::new(),
        reject: Arc::clone(&reject),
    });
    assert!(rt.enable_strict_typed_actor_admission(id).is_err());
    assert!(!rt.actors[&id].mailbox.requires_strict_durable_admission());
    assert_eq!(rt.persistence.latest_sequence(id), 0);
    assert!(rt.persistence.load_snapshot(id).is_none());

    reject.store(false, Ordering::SeqCst);
    rt.enable_strict_typed_actor_admission(id)
        .expect("same host may safely enable strict mode once storage succeeds");
    assert!(rt.actors[&id].mailbox.requires_strict_durable_admission());
    assert_eq!(rt.persistence.latest_sequence(id), 1);
}
