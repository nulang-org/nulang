//! A source-compiled actor's incoming command can be admitted to the canonical
//! atomic journal without dropping the exact mailbox envelope on failure.
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{
    DurableTransition, JournalEntry, PersistedValue, PersistenceStore, Runtime,
    DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

fn compiled_counter() -> (nulang::bytecode::CodeModule, u16, u16) {
    let source = r#"
        persistent actor Counter {
            state durable total: Int = 0
            behavior add(by: Int) { self.total = self.total + by }
        }
        actor Unrelated {
            behavior add(by: Int) { nil }
        }
    "#;
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lower");
    let module = nulang::mir_codegen::compile_mir(&mut mir, "atomic_admission").expect("compile");
    let counter = module.actor_metadata.iter().find(|m| m.name == "Counter").unwrap();
    let unrelated = module.actor_metadata.iter().find(|m| m.name == "Unrelated").unwrap();
    (
        module.clone(),
        counter.behavior_indices[0] as u16,
        unrelated.behavior_indices[0] as u16,
    )
}

fn spawned() -> (Runtime, u64, u16, u16) {
    let (module, behavior, unrelated) = compiled_counter();
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, behavior as usize, vec![])
        .as_actor_id()
        .expect("spawn persistent Counter");
    (runtime, actor_id, behavior, unrelated)
}

fn command(
    runtime: &Runtime,
    actor_id: u64,
    behavior: u16,
    predecessor: u64,
    value: i64,
) -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: runtime.actors[&actor_id].activation_epoch,
        sequence: predecessor + 1,
        expected_previous_sequence: predecessor,
        command: Some(JournalEntry {
            sequence: predecessor + 1,
            behavior_id: behavior,
            payload: vec![PersistedValue::Int(value)],
        }),
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

#[test]
fn successful_atomic_admission_returns_exact_message_without_running_handler() {
    let (mut runtime, actor_id, behavior, _) = spawned();
    let predecessor = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let queued = runtime.actors[&actor_id].mailbox.len();
    assert_eq!(queued, 1);

    let proposed = command(&runtime, actor_id, behavior, predecessor, 5);
    let (message, committed) = runtime
        .admit_next_typed_actor_command(actor_id, proposed)
        .expect("atomic command accepted")
        .expect("one message admitted");
    assert_eq!(message.behavior_id, behavior);
    assert_eq!(message.payload[0].as_int(), Some(5));
    assert_eq!(committed.sequence, predecessor + 1);
    assert!(runtime.actors[&actor_id].mailbox.is_empty());
    assert_eq!(runtime.persistence.latest_sequence(actor_id), predecessor + 1);
    assert_eq!(
        runtime.persistence.read_journal(actor_id).last().unwrap().behavior_id,
        behavior
    );
    assert_eq!(
        runtime.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(0),
        "the adapter must not run user bytecode before durable state staging"
    );
}

#[test]
fn failed_commit_keeps_original_message_and_retry_preserves_fifo() {
    let (mut runtime, actor_id, behavior, _) = spawned();
    let predecessor = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);

    let invalid = command(&runtime, actor_id, behavior, predecessor + 1, 5);
    assert!(runtime
        .admit_next_typed_actor_command(actor_id, invalid)
        .is_err());
    assert_eq!(runtime.persistence.latest_sequence(actor_id), predecessor);
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);

    runtime.send_message_by_id(actor_id, behavior, &[Value::int(6)]);
    let retry = command(&runtime, actor_id, behavior, predecessor, 5);
    let (message, _) = runtime
        .admit_next_typed_actor_command(actor_id, retry)
        .expect("retry")
        .expect("original message");
    assert_eq!(message.payload[0].as_int(), Some(5));
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.pop().unwrap().payload[0].as_int(),
        Some(6)
    );
}

#[test]
fn untrusted_transition_payload_or_foreign_behavior_never_commits() {
    let (mut runtime, actor_id, behavior, unrelated) = spawned();
    let predecessor = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);

    let spoofed = command(&runtime, actor_id, behavior, predecessor, 99);
    let error = runtime
        .admit_next_typed_actor_command(actor_id, spoofed)
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);

    let foreign = command(&runtime, actor_id, unrelated, predecessor, 5);
    assert_eq!(
        runtime
            .admit_next_typed_actor_command(actor_id, foreign)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(runtime.persistence.latest_sequence(actor_id), predecessor);
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
}

#[test]
fn admission_rejects_transient_actors_without_touching_durable_store() {
    let (module, _, unrelated) = compiled_counter();
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, unrelated as usize, vec![])
        .as_actor_id()
        .expect("spawn transient Unrelated");
    let predecessor = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, unrelated, &[Value::int(3)]);

    let proposed = command(&runtime, actor_id, unrelated, predecessor, 3);
    assert_eq!(
        runtime
            .admit_next_typed_actor_command(actor_id, proposed)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(runtime.persistence.latest_sequence(actor_id), predecessor);
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
}
