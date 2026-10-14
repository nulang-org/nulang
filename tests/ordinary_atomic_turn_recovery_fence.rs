//! Crash-cutpoint contract for ordinary actor command admission and result
//! completion. The host-only atomic admission path must not fall back to
//! executing partially committed bytecode on a fresh recovery activation.
use std::collections::HashMap;

use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{
    ActorSnapshot, DurableTransition, JournalEntry, PersistedValue, Runtime,
    DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;

fn compiled_counter() -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(
        r#"
        persistent actor Counter {
            state durable total: Int = 0
            behavior add(by: Int) { self.total = self.total + by }
        }
        "#,
    )
    .lex()
    .expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR");
    nulang::mir_codegen::compile_mir(&mut mir, "atomic_recovery_fence").expect("bytecode")
}

fn recovery_fixture() -> (Runtime, u64, u16) {
    let module = compiled_counter();
    let behavior = u16::try_from(module.actor_metadata[0].behavior_indices[0])
        .expect("behavior index");
    let offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.code_offset as usize)
        .collect();
    let compensations = module
        .behaviors
        .iter()
        .map(|behavior| behavior.compensate_offset)
        .collect();
    let actor_id = 71_220;
    let mut runtime = Runtime::new();
    runtime.register_recovery_module(actor_id, module, offsets, compensations);
    runtime
        .persistence
        .save_snapshot(ActorSnapshot {
            actor_id,
            schema_name: Some("Counter".to_string()),
            state: HashMap::from([("total".to_string(), PersistedValue::Int(0))]),
            ..ActorSnapshot::default()
        })
        .expect("durable baseline");
    (runtime, actor_id, behavior)
}

fn transition(
    actor_id: u64,
    sequence: u64,
    command: Option<JournalEntry>,
    snapshot: Option<ActorSnapshot>,
) -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: 1,
        sequence,
        expected_previous_sequence: sequence - 1,
        command,
        snapshot,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

#[test]
fn crash_after_command_admission_before_result_must_not_reexecute_legacy_handler() {
    let (mut runtime, actor_id, behavior) = recovery_fixture();
    runtime
        .persistence
        .commit_transition(transition(
            actor_id,
            1,
            Some(JournalEntry {
                sequence: 1,
                behavior_id: behavior,
                payload: vec![PersistedValue::Int(5)],
            }),
            None,
        ))
        .expect("command admitted before crash");

    assert_eq!(runtime.persistence.latest_sequence(actor_id), 1);
    assert_eq!(
        runtime.recover_actor(actor_id),
        None,
        "incomplete atomic ordinary-actor command must not run via legacy replay"
    );
    assert!(!runtime.actors.contains_key(&actor_id));
    let safe_snapshot = runtime.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(safe_snapshot.sequence, 0);
    assert_eq!(
        safe_snapshot.state.get("total"),
        Some(&PersistedValue::Int(0))
    );
}

#[test]
fn recovery_after_atomic_result_restores_committed_state_without_reexecuting_command() {
    let (mut runtime, actor_id, behavior) = recovery_fixture();
    runtime
        .persistence
        .commit_transition(transition(
            actor_id,
            1,
            Some(JournalEntry {
                sequence: 1,
                behavior_id: behavior,
                payload: vec![PersistedValue::Int(5)],
            }),
            None,
        ))
        .expect("command");
    assert_eq!(runtime.recover_actor(actor_id), None);

    runtime
        .persistence
        .commit_transition(transition(
            actor_id,
            2,
            None,
            Some(ActorSnapshot {
                actor_id,
                sequence: 2,
                schema_name: Some("Counter".to_string()),
                state: HashMap::from([("total".to_string(), PersistedValue::Int(5))]),
                ..ActorSnapshot::default()
            }),
        ))
        .expect("atomic completed result");

    assert_eq!(runtime.recover_actor(actor_id), Some(actor_id));
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(5),
        "completed command must not execute twice during recovery"
    );
}

#[test]
fn legacy_non_atomic_journal_remains_recoverable() {
    let (mut runtime, actor_id, behavior) = recovery_fixture();
    runtime
        .persistence
        .append_journal(
            actor_id,
            JournalEntry {
                sequence: 1,
                behavior_id: behavior,
                payload: vec![PersistedValue::Int(5)],
            },
        )
        .expect("legacy journal");
    assert_eq!(
        runtime.recover_actor(actor_id),
        Some(actor_id),
        "a backend with no atomic tail must preserve legacy recovery"
    );
}
