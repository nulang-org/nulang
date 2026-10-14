//! Persisted strict admission policy must be restored before actor publication.
//! A committed-but-incomplete strict command must never replay through legacy
//! non-workflow bytecode recovery.

use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{
    DurableTransition, JournalEntry, PersistedValue, PersistenceStore, Runtime,
    DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

fn compiled_counter() -> (nulang::bytecode::CodeModule, usize) {
    let source = r#"
        persistent actor Counter {
            state durable total: Int = 0
            behavior add(by: Int) { self.total = self.total + by }
        }
    "#;
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).unwrap();
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).unwrap();
    let module = nulang::mir_codegen::compile_mir(&mut mir, "strict_recovery").unwrap();
    let behavior = module.actor_metadata[0].behavior_indices[0];
    (module, behavior)
}

fn recovered_fixture() -> (Runtime, u64, u16) {
    let (module, behavior) = compiled_counter();
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, behavior, vec![])
        .as_actor_id()
        .unwrap();
    runtime.checkpoint_actor(actor_id);
    assert!(runtime.persistence.load_snapshot(actor_id).is_some());
    let offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.code_offset as usize)
        .collect();
    let compensation_offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.compensate_offset)
        .collect();
    runtime.register_recovery_module(actor_id, module, offsets, compensation_offsets);
    (runtime, actor_id, behavior as u16)
}

#[test]
fn enabling_strict_admission_persists_policy_before_routing() {
    let (mut runtime, actor_id, _) = recovered_fixture();
    assert!(!runtime
        .persistence
        .load_strict_typed_actor_admission(actor_id)
        .unwrap());
    runtime.enable_strict_typed_actor_admission(actor_id).unwrap();
    assert!(runtime
        .persistence
        .load_strict_typed_actor_admission(actor_id)
        .unwrap());
    assert!(runtime.actors[&actor_id]
        .mailbox
        .requires_strict_durable_admission());
}

#[test]
fn recovered_typed_actor_restores_gate_before_legacy_dispatch() {
    let (mut runtime, actor_id, behavior) = recovered_fixture();
    runtime.enable_strict_typed_actor_admission(actor_id).unwrap();
    runtime.actors.remove(&actor_id);

    assert_eq!(runtime.recover_actor(actor_id), Some(actor_id));
    assert!(runtime.actors[&actor_id]
        .mailbox
        .requires_strict_durable_admission());
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    runtime.step_actor(actor_id);
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0),
        "strict policy must prevent recovered actor from running bytecode"
    );
}

#[test]
fn unfinished_atomic_command_refuses_recovery_instead_of_replaying_handler() {
    let (mut runtime, actor_id, behavior) = recovered_fixture();
    runtime.enable_strict_typed_actor_admission(actor_id).unwrap();
    let snapshot = runtime.persistence.load_snapshot(actor_id).unwrap();
    let command_sequence = snapshot.sequence + 1;
    runtime
        .persistence
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: snapshot.activation_epoch,
            sequence: command_sequence,
            expected_previous_sequence: snapshot.sequence,
            command: Some(JournalEntry {
                sequence: command_sequence,
                behavior_id: behavior,
                payload: vec![PersistedValue::Int(5)],
            }),
            snapshot: None,
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .expect("simulate admitted but unfinished command");
    runtime.actors.remove(&actor_id);

    assert_eq!(
        runtime.recover_actor(actor_id),
        Some(actor_id),
        "one proven unfinished strict command can be recovered only as fenced state"
    );
    let actor = &runtime.actors[&actor_id];
    assert!(actor.mailbox.requires_strict_durable_admission());
    assert_eq!(
        actor.mailbox.admitted_command_sequence(),
        Some(command_sequence),
        "uncompleted command must remain fenced"
    );
    assert_eq!(actor.get_state_field("total").and_then(|v| v.as_int()), Some(0));
    assert_eq!(runtime.persistence.read_journal(actor_id).len(), 1);

    // A queued normal message must not cause the recovered user handler to
    // run while the prior durable command has no completion snapshot.
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(9)]);
    runtime.step_actor(actor_id);
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
    assert_eq!(
        runtime.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(0)
    );
    assert_eq!(
        runtime.finish_admitted_typed_actor_command(actor_id, command_sequence)
            .unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn strict_recovery_rejects_ambiguous_unfinished_history() {
    let (mut runtime, actor_id, behavior) = recovered_fixture();
    runtime.enable_strict_typed_actor_admission(actor_id).unwrap();
    let checkpoint = runtime.persistence.load_snapshot(actor_id).unwrap();
    let first = checkpoint.sequence + 1;
    runtime.persistence.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: checkpoint.activation_epoch,
        sequence: first,
        expected_previous_sequence: checkpoint.sequence,
        command: Some(JournalEntry {
            sequence: first,
            behavior_id: behavior,
            payload: vec![PersistedValue::Int(5)],
        }),
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }).unwrap();
    // A second committed atomic journal command without a completion
    // snapshot is ambiguous and must not be published as recoverable work.
    runtime.persistence.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: checkpoint.activation_epoch,
        sequence: first + 1,
        expected_previous_sequence: first,
        command: Some(JournalEntry {
            sequence: first + 1,
            behavior_id: behavior,
            payload: vec![PersistedValue::Int(6)],
        }),
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }).unwrap();
    runtime.actors.remove(&actor_id);
    assert_eq!(runtime.recover_actor(actor_id), None);
    assert!(!runtime.actors.contains_key(&actor_id));
}

#[test]
fn strict_recovery_rejects_orphaned_post_snapshot_journal() {
    let (mut runtime, actor_id, behavior) = recovered_fixture();
    runtime.enable_strict_typed_actor_admission(actor_id).unwrap();
    let snapshot = runtime.persistence.load_snapshot(actor_id).unwrap();
    runtime.persistence.append_journal(actor_id, JournalEntry {
        sequence: snapshot.sequence + 1,
        behavior_id: behavior,
        payload: vec![PersistedValue::Int(5)],
    }).unwrap();
    runtime.actors.remove(&actor_id);
    assert_eq!(runtime.recover_actor(actor_id), None);
    assert!(!runtime.actors.contains_key(&actor_id));
}

#[test]
fn unknown_schema_cannot_recover_strict_actor_without_compiler_metadata() {
    let (mut runtime, actor_id, _) = recovered_fixture();
    runtime.enable_strict_typed_actor_admission(actor_id).unwrap();
    // Reconstruct a runtime with the persisted store but without the
    // compiler-owned recovery module. Public APIs must fail closed.
    let mut fresh = Runtime::new();
    fresh.persistence = std::mem::replace(
        &mut runtime.persistence,
        Box::new(nulang::runtime::MemoryStore::new()),
    );
    assert_eq!(fresh.recover_actor(actor_id), None);
    assert!(!fresh.actors.contains_key(&actor_id));
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_strict_policy_survives_disk_reopen() {
    use nulang::runtime::LibsqlStore;
    let path = std::env::temp_dir().join(format!(
        "nulang-strict-policy-{}.db",
        std::process::id()
    ));
    {
        let mut store = LibsqlStore::new(&path).unwrap();
        store.persist_strict_typed_actor_admission(701).unwrap();
        assert!(store.load_strict_typed_actor_admission(701).unwrap());
        assert!(!store.load_strict_typed_actor_admission(702).unwrap());
    }
    {
        let reopened = LibsqlStore::new(&path).unwrap();
        assert!(reopened.load_strict_typed_actor_admission(701).unwrap());
        assert!(!reopened.load_strict_typed_actor_admission(702).unwrap());
    }
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}
