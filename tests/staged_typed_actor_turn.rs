//! A trusted host can stage a typed durable turn without running a handler or
//! exposing effects before one atomic persistence commit.
use std::collections::HashMap;
use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{DurableOutboxMessage, PersistedValue, Runtime};
use nulang::typechecker::TypeChecker;

fn compile(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lower");
    nulang::mir_codegen::compile_mir(&mut mir, "staged_typed_turn").expect("compile")
}

fn module() -> nulang::bytecode::CodeModule {
    compile(
        r#"
        persistent actor Counter {
            state durable count: Int = 0
            state local scratch: Int = 1
            behavior add(amount: Int) { self.count = self.count + amount }
        }
        actor Other { behavior add(amount: Int) { nil } }
        "#,
    )
}

fn spawn() -> (Runtime, u64, u16, u16) {
    let module = module();
    let counter = module.actor_metadata.iter().find(|m| m.name == "Counter").unwrap();
    let other = module.actor_metadata.iter().find(|m| m.name == "Other").unwrap();
    let counter_behavior = counter.behavior_indices[0] as u16;
    let other_behavior = other.behavior_indices[0] as u16;
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, counter_behavior as usize, vec![])
        .as_actor_id()
        .expect("spawn persistent Counter");
    (runtime, actor_id, counter_behavior, other_behavior)
}

fn effect(actor_id: u64) -> DurableEffectPersistenceRecord {
    let spec = DurableEffectSpec::new(
        DurableEffectId::derive(actor_id, "turn:1", 0, "Audit.write"),
        "Audit.write",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    DurableEffectPersistenceRecord::from_effect(DurableEffectRecord::prepare(spec, b"count=5"))
}

fn outbox() -> Vec<DurableOutboxMessage> {
    vec![DurableOutboxMessage {
        destination_actor_id: 81,
        ordinal: 0,
        behavior_id: 0,
        payload: vec![PersistedValue::Int(5)],
    }]
}

#[test]
fn staging_commits_command_state_effect_and_outbox_before_publishing_live_state() {
    let (mut runtime, actor_id, behavior, _) = spawn();
    let previous = runtime.persistence.latest_sequence(actor_id);
    let updates = HashMap::from([("count".to_string(), PersistedValue::Int(5))]);
    let record = effect(actor_id);
    let id = record.effect().spec().id;

    let first = runtime
        .commit_staged_typed_actor_turn(
            actor_id, previous, behavior, vec![PersistedValue::Int(5)],
            updates.clone(), vec![record.clone()], outbox(),
        )
        .expect("commit typed state and effects");
    assert_eq!(first.sequence, previous + 1);
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("count")
            .and_then(|v| v.as_int()),
        Some(5)
    );
    assert_eq!(runtime.persistence.latest_sequence(actor_id), previous + 1);
    assert_eq!(runtime.persistence.read_journal(actor_id).last().unwrap().behavior_id, behavior);
    assert_eq!(runtime.persistence.load_snapshot(actor_id).unwrap().schema_name.as_deref(), Some("Counter"));
    assert!(runtime.persistence.load_durable_effect(actor_id, id).unwrap().is_some());

    let repeat = runtime
        .commit_staged_typed_actor_turn(
            actor_id, previous, behavior, vec![PersistedValue::Int(5)],
            updates, vec![record], outbox(),
        )
        .expect("same logical turn may retry after ambiguous acknowledgement");
    assert_eq!(repeat, first);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), previous + 1);
}

#[test]
fn wrong_argument_count_does_not_advance_state_or_persistence() {
    let (mut runtime, actor_id, behavior, _) = spawn();
    let previous = runtime.persistence.latest_sequence(actor_id);
    let error = runtime
        .commit_staged_typed_actor_turn(
            actor_id, previous, behavior, vec![],
            HashMap::from([("count".to_string(), PersistedValue::Int(9))]),
            vec![], vec![],
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), previous);
    assert_eq!(runtime.actors[&actor_id].get_state_field("count").and_then(|v| v.as_int()), Some(0));
}

#[test]
fn foreign_behavior_and_non_durable_state_are_refused_before_commit() {
    let (mut runtime, actor_id, _behavior, foreign_behavior) = spawn();
    let prev = runtime.persistence.latest_sequence(actor_id);
    let error = runtime
        .commit_staged_typed_actor_turn(
            actor_id, prev, foreign_behavior, vec![PersistedValue::Int(1)],
            HashMap::new(), vec![], vec![],
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    let behavior = runtime.behavior_id_for(actor_id, "add").unwrap();
    let error = runtime
        .commit_staged_typed_actor_turn(
            actor_id, prev, behavior, vec![PersistedValue::Int(1)],
            HashMap::from([("scratch".to_string(), PersistedValue::Int(99))]),
            vec![], vec![],
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), prev);
    assert_eq!(runtime.actors[&actor_id].get_state_field("scratch").and_then(|v| v.as_int()), Some(1));
}

#[test]
fn transient_actors_cannot_be_upgraded_by_a_staged_durable_call() {
    let module = compile(
        r#"
        actor Transient {
            state count: Int = 0
            behavior add(amount: Int) { self.count = amount }
        }
        "#,
    );
    let behavior = module.actor_metadata[0].behavior_indices[0] as u16;
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, behavior as usize, vec![])
        .as_actor_id().expect("spawn");
    let error = runtime
        .commit_staged_typed_actor_turn(
            actor_id, 0, behavior, vec![PersistedValue::Int(9)],
            HashMap::from([("count".to_string(), PersistedValue::Int(9))]),
            vec![], vec![],
        )
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.actors[&actor_id].get_state_field("count").and_then(|v| v.as_int()), Some(0));
}
