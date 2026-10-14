//! Regression gates for opt-in strict typed durable command admission.
//!
//! The strict gate prevents legacy handler dispatch. It does not itself
//! provide crash-safe inbox delivery or automatically execute staged handlers.

use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{
    DurableTransition, JournalEntry, Message, MessagePayload, MessagePriority,
    PersistedValue, PersistenceStore, Runtime, DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

fn actors() -> (Runtime, u64, u64, u16) {
    let source = r#"
        persistent actor Counter {
            state durable total: Int = 0
            behavior add(amount: Int) { self.total = self.total + amount }
        }
        actor Transient {
            state total: Int = 0
            behavior add(amount: Int) { self.total = self.total + amount }
        }
    "#;
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lower");
    let module = nulang::mir_codegen::compile_mir(&mut mir, "strict_durable_gate")
        .expect("compile");

    let counter = module.actor_metadata.iter().find(|m| m.name == "Counter").unwrap();
    let persistent_behavior = counter.behavior_indices[0] as u16;
    let transient_behavior = module.actor_metadata.iter()
        .find(|m| m.name == "Transient")
        .unwrap().behavior_indices[0];

    let mut runtime = Runtime::new();
    let persistent_id = runtime
        .spawn_from_module(&module, persistent_behavior as usize, vec![])
        .as_actor_id().unwrap();
    let transient_id = runtime
        .spawn_from_module(&module, transient_behavior, vec![])
        .as_actor_id().unwrap();
    (runtime, persistent_id, transient_id, persistent_behavior)
}

fn command(runtime: &Runtime, actor_id: u64, behavior: u16, predecessor: u64, value: i64)
    -> DurableTransition
{
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
fn strict_mode_blocks_legacy_bytecode_and_sync_ask_without_dropping_messages() {
    let (mut rt, actor_id, _, behavior) = actors();
    rt.enable_strict_typed_actor_admission(actor_id).expect("enable strict gate");
    let previous = rt.persistence.latest_sequence(actor_id);

    // A normal actor turn must not silently bypass durable admission.
    rt.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    rt.step_actor(actor_id);
    assert_eq!(rt.actors[&actor_id].mailbox.len(), 1);
    assert_eq!(rt.persistence.latest_sequence(actor_id), previous);
    assert_eq!(rt.actors[&actor_id].get_state_field("total").unwrap().as_int(), Some(0));

    // Synchronous Ask bypasses the mailbox, so it must also be fenced,
    // including before any command has been admitted.
    let error = rt.ask_actor_sync(actor_id, behavior, &[Value::int(19)])
        .expect_err("synchronous ask cannot bypass strict gate");
    assert!(error.to_string().contains("strict durable admission"));
    assert_eq!(rt.actors[&actor_id].get_state_field("total").unwrap().as_int(), Some(0));
    assert_eq!(rt.actors[&actor_id].mailbox.len(), 1);
}

#[test]
fn strict_gate_allows_explicit_atomic_host_admission_only() {
    let (mut rt, actor_id, _, behavior) = actors();
    rt.enable_strict_typed_actor_admission(actor_id).unwrap();
    let predecessor = rt.persistence.latest_sequence(actor_id);
    rt.send_message_by_id(actor_id, behavior, &[Value::int(5)]);

    let proposal = command(&rt, actor_id, behavior, predecessor, 5);
    let (admitted, receipt) = rt.admit_next_typed_actor_command(actor_id, proposal)
        .expect("atomic commit").expect("original message");
    assert_eq!(admitted.payload[0].as_int(), Some(5));
    assert_eq!(receipt.sequence, predecessor + 1);
    assert_eq!(rt.persistence.read_journal(actor_id).len(), 1);
    assert_eq!(rt.actors[&actor_id].get_state_field("total").unwrap().as_int(), Some(0));

    // Completion is still a separate, trusted-host persistence operation.
    // Strict mode must not resume normal legacy dispatch after admission.
    rt.send_message_by_id(actor_id, behavior, &[Value::int(6)]);
    rt.step_actor(actor_id);
    assert_eq!(rt.actors[&actor_id].mailbox.len(), 1);
    assert_eq!(rt.persistence.latest_sequence(actor_id), receipt.sequence);
}

#[test]
fn strict_gate_preserves_system_priority_messages_and_original_queue() {
    let (mut rt, actor_id, _, behavior) = actors();
    rt.enable_strict_typed_actor_admission(actor_id).unwrap();
    rt.send_message_by_id(actor_id, behavior, &[Value::int(5)]);

    // System notifications are still eligible, but user messages remain
    // queued and cannot execute through the legacy actor.receive() path.
    rt.actors.get_mut(&actor_id).unwrap().mailbox.push_local(Message {
        behavior_id: 65534,
        payload: MessagePayload::from_slice(&[]),
        sender: 0,
        priority: MessagePriority::System,
        trace_id: None,
    }).unwrap();
    let system = rt.actors.get_mut(&actor_id).unwrap().mailbox.pop()
        .expect("system notice may bypass strict user admission");
    assert_eq!(system.priority, MessagePriority::System);
    assert_eq!(rt.actors[&actor_id].mailbox.len(), 1);
    assert!(rt.actors.get_mut(&actor_id).unwrap().mailbox.pop().is_none());
}

#[test]
fn strict_gate_refuses_transient_and_missing_actors() {
    let (mut rt, _, transient_id, _) = actors();
    assert_eq!(
        rt.enable_strict_typed_actor_admission(transient_id).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(
        rt.enable_strict_typed_actor_admission(u64::MAX).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn enabling_strict_gate_invalidates_an_already_selected_receive() {
    let (mut rt, actor_id, _, behavior) = actors();
    rt.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    assert!(
        rt.actors.get_mut(&actor_id).unwrap().mailbox
            .receive_match(&[behavior]).is_some(),
        "before the opt-in, the legacy selective receive can find a candidate"
    );
    rt.enable_strict_typed_actor_admission(actor_id).unwrap();

    let mailbox = &mut rt.actors.get_mut(&actor_id).unwrap().mailbox;
    assert!(
        mailbox.commit_receive_match().is_none(),
        "an active receive selected before opt-in must not commit afterward"
    );
    assert!(mailbox.receive_match(&[behavior]).is_none());
    assert_eq!(mailbox.len(), 1, "no user message may be lost");
}

#[test]
fn default_legacy_execution_is_unchanged_until_host_opts_in() {
    let (mut rt, actor_id, _, behavior) = actors();
    assert!(!rt.actors[&actor_id].mailbox.requires_strict_durable_admission());
    rt.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    rt.step_actor(actor_id);
    assert_eq!(
        rt.actors[&actor_id].get_state_field("total").unwrap().as_int(),
        Some(5),
        "the opt-in must not silently change existing execution semantics"
    );
}
