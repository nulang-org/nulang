//! Integration between host-enforced strict admission and isolated pure-bytecode
//! execution. Neither stage may allow the legacy scheduler to run a user turn.
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{
    DurableTransition, JournalEntry, PersistedValue, PersistenceStore, Runtime,
    DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

fn strict_counter() -> (Runtime, u64, u16) {
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
    let module = nulang::mir_codegen::compile_mir(&mut mir, "strict_pure_bridge")
        .expect("bytecode");
    let behavior = module.actor_metadata[0].behavior_indices[0] as u16;
    let mut rt = Runtime::new();
    let actor_id = rt
        .spawn_from_module(&module, behavior as usize, vec![])
        .as_actor_id()
        .expect("spawn");
    (rt, actor_id, behavior)
}

fn admit(rt: &mut Runtime, actor_id: u64, behavior: u16, amount: i64) -> u64 {
    let predecessor = rt.persistence.latest_sequence(actor_id);
    let epoch = rt.actors[&actor_id].activation_epoch;
    let proposed = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: epoch,
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
    };
    rt.admit_next_typed_actor_command(actor_id, proposed)
        .expect("commit admission")
        .expect("pending command")
        .1
        .sequence
}

#[test]
fn strict_gate_only_releases_commands_through_persisted_pure_completion() {
    let (mut rt, actor_id, behavior) = strict_counter();
    rt.enable_strict_typed_actor_admission(actor_id)
        .expect("opt into strict admission");
    assert!(rt.actors[&actor_id]
        .mailbox
        .requires_strict_durable_admission());

    rt.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    rt.send_message_by_id(actor_id, behavior, &[Value::int(7)]);
    rt.run_scheduler();
    assert_eq!(rt.actors[&actor_id].mailbox.len(), 2);
    assert_eq!(rt.persistence.latest_sequence(actor_id), 0);
    assert_eq!(
        rt.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(0)
    );
    assert!(rt.ask_actor_sync(actor_id, behavior, &[Value::int(19)]).is_err());

    let first = admit(&mut rt, actor_id, behavior, 5);
    rt.run_scheduler();
    assert_eq!(rt.persistence.latest_sequence(actor_id), first);
    assert_eq!(rt.actors[&actor_id].mailbox.len(), 1);
    assert!(rt.actors[&actor_id].mailbox.requires_strict_durable_admission());
    assert_eq!(
        rt.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(0)
    );

    let first_result = rt
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, first)
        .expect("complete first pure turn");
    assert_eq!(first_result.sequence, first + 1);
    assert_eq!(
        rt.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(5)
    );
    // The next message must still be gated even though the previous
    // admitted-command fence was successfully released.
    rt.run_scheduler();
    assert_eq!(rt.actors[&actor_id].mailbox.len(), 1);
    assert_eq!(rt.persistence.latest_sequence(actor_id), first + 1);

    let second = admit(&mut rt, actor_id, behavior, 7);
    assert_eq!(second, first + 2);
    let second_result = rt
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, second)
        .expect("complete second pure turn");
    assert_eq!(second_result.sequence, second + 1);
    assert_eq!(
        rt.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(12)
    );
    assert_eq!(rt.persistence.read_journal(actor_id).len(), 2);
    assert!(rt.actors[&actor_id].mailbox.is_empty());
    assert!(rt.actors[&actor_id].mailbox.requires_strict_durable_admission());
}

#[test]
fn strict_gate_rejects_sync_bypass_after_successful_atomic_result() {
    let (mut rt, actor_id, behavior) = strict_counter();
    rt.enable_strict_typed_actor_admission(actor_id).unwrap();
    rt.send_message_by_id(actor_id, behavior, &[Value::int(3)]);
    let command = admit(&mut rt, actor_id, behavior, 3);
    rt.execute_and_commit_admitted_pure_typed_actor_turn(actor_id, command)
        .unwrap();

    assert!(rt.ask_actor_sync(actor_id, behavior, &[Value::int(50)]).is_err());
    assert_eq!(
        rt.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(3)
    );
    assert_eq!(rt.persistence.latest_sequence(actor_id), command + 1);
}

#[test]
fn isolated_staged_execution_rejects_unprotected_actor_until_strict_opt_in() {
    let (mut rt, actor_id, behavior) = strict_counter();
    rt.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let sequence = admit(&mut rt, actor_id, behavior, 5);
    let error = rt
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, sequence)
        .expect_err("staged execution must require a strict admission policy");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(rt.persistence.latest_sequence(actor_id), sequence);
    assert_eq!(
        rt.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(0)
    );
    assert_eq!(
        rt.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(sequence)
    );

    rt.enable_strict_typed_actor_admission(actor_id).unwrap();
    let committed = rt
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, sequence)
        .expect("retry after strict opt-in");
    assert_eq!(committed.sequence, sequence + 1);
    assert_eq!(
        rt.actors[&actor_id].get_state_field("total").and_then(|v| v.as_int()),
        Some(5)
    );
}
