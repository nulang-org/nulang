//! Trusted host atomic completion of an already-admitted typed actor command.
//! No bytecode, external provider, or outbox dispatcher executes in this test.

use std::collections::HashMap;
use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    DurableOutboxMessage, DurableTransition, JournalEntry, PersistedValue, PersistenceStore,
    Runtime, DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

fn spawned_counter() -> (Runtime, u64, u16) {
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
    let module = nulang::mir_codegen::compile_mir(&mut mir, "typed_result").expect("bytecode");
    let behavior = module.actor_metadata[0].behavior_indices[0] as u16;
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, behavior as usize, vec![])
        .as_actor_id()
        .expect("spawn persistent counter");
    (runtime, actor_id, behavior)
}

fn admit(runtime: &mut Runtime, actor_id: u64, behavior: u16, amount: i64) -> u64 {
    let predecessor = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(amount)]);
    let commit = runtime
        .admit_next_typed_actor_command(
            actor_id,
            DurableTransition {
                version: DURABLE_TRANSITION_VERSION,
                actor_id,
                activation_epoch: runtime.actors[&actor_id].activation_epoch,
                sequence: predecessor + 1,
                expected_previous_sequence: predecessor,
                command: Some(JournalEntry {
                    sequence: predecessor + 1,
                    behavior_id: behavior,
                    payload: vec![PersistedValue::Int(amount)],
                }),
                snapshot: None,
                workflow_events: vec![],
                domain_events: vec![],
                durable_effects: vec![],
                outbox: vec![],
            },
        )
        .unwrap()
        .expect("admit command");
    assert_eq!(commit.0.payload[0].as_int(), Some(amount));
    commit.1.sequence
}

fn prepared_effect(actor_id: u64) -> DurableEffectPersistenceRecord {
    let spec = DurableEffectSpec::new(
        DurableEffectId::derive(actor_id, "cmd:1", 0, "Audit::Write"),
        "Audit::Write",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    DurableEffectPersistenceRecord::from_effect(DurableEffectRecord::prepare(spec, b"amount=5"))
}

#[test]
fn result_state_and_outbox_commit_before_live_state_or_next_command() {
    let (mut runtime, actor_id, behavior) = spawned_counter();
    let admitted = admit(&mut runtime, actor_id, behavior, 5);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(6)]);

    let updates = HashMap::from([("total".to_string(), PersistedValue::Int(5))]);
    let result = runtime
        .commit_admitted_typed_actor_result(
            actor_id,
            admitted,
            updates,
            vec![],
            vec![DurableOutboxMessage {
                destination_actor_id: actor_id + 1,
                ordinal: 0,
                behavior_id: 0,
                payload: vec![PersistedValue::Int(5)],
            }],
        )
        .expect("commit staged result");
    assert_eq!(result.sequence, admitted + 1);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), admitted + 1);
    assert_eq!(runtime.persistence.read_journal(actor_id).len(), 1);
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(5)
    );
    assert_eq!(
        runtime.actors[&actor_id]
            .mailbox
            .pop()
            .unwrap()
            .payload[0]
            .as_int(),
        Some(6),
        "next message becomes runnable only after durable completion"
    );
}

#[test]
fn invalid_typed_result_never_commits_or_releases_admission_fence() {
    let (mut runtime, actor_id, behavior) = spawned_counter();
    let admitted = admit(&mut runtime, actor_id, behavior, 5);

    let error = runtime
        .commit_admitted_typed_actor_result(
            actor_id,
            admitted,
            HashMap::from([(
                "total".to_string(),
                PersistedValue::String("wrong".into()),
            )]),
            vec![],
            vec![],
        )
        .expect_err("reject invalid typed result before persistence");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), admitted);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(admitted)
    );
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0)
    );
}

#[test]
fn prepared_external_effect_is_denied_without_live_capability_grant() {
    let (mut runtime, actor_id, behavior) = spawned_counter();
    let admitted = admit(&mut runtime, actor_id, behavior, 5);
    let record = prepared_effect(actor_id);
    let operation_id = record.effect().spec().id;
    let error = runtime
        .commit_admitted_typed_actor_result(
            actor_id,
            admitted,
            HashMap::from([("total".to_string(), PersistedValue::Int(5))]),
            vec![record],
            vec![],
        )
        .expect_err("external effect requires live authority");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), admitted);
    assert!(
        runtime
            .persistence
            .load_durable_effect(actor_id, operation_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn result_commit_requires_an_actual_admitted_command() {
    let (mut runtime, actor_id, _behavior) = spawned_counter();
    let error = runtime
        .commit_admitted_typed_actor_result(
            actor_id,
            1,
            HashMap::new(),
            vec![],
            vec![],
        )
        .expect_err("no admitted execution");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), 0);
}
