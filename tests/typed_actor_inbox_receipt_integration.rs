//! Strict typed actor admission must distinguish a new atomic inbox commit
//! from a replayed acknowledgment before returning bytecode work to the host.
use nulang::durable_inbox::DurableInboxCommand;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{
    DurableTransition, JournalEntry, PersistedValue, PersistenceStore, Runtime,
    TypedInboxAdmission, DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

fn actor() -> (Runtime, u64, u16) {
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
    let module = nulang::mir_codegen::compile_mir(&mut mir, "inbox_actor").unwrap();
    let behavior = module
        .actor_metadata
        .iter()
        .find(|a| a.name == "Counter")
        .unwrap()
        .behavior_indices[0] as u16;
    let mut rt = Runtime::new();
    let id = rt
        .spawn_from_module(&module, behavior as usize, vec![])
        .as_actor_id()
        .unwrap();
    (rt, id, behavior)
}

fn transition(rt: &Runtime, id: u64, behavior: u16, value: i64) -> DurableTransition {
    let previous = rt.persistence.latest_sequence(id);
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: id,
        activation_epoch: rt.actors[&id].activation_epoch,
        sequence: previous + 1,
        expected_previous_sequence: previous,
        command: Some(JournalEntry {
            sequence: previous + 1,
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

fn request(id: u64, behavior: u16, key: &str, value: i64) -> DurableInboxCommand {
    DurableInboxCommand::new(
        "tenant-a",
        "authenticated-sender",
        key,
        id,
        behavior,
        &[PersistedValue::Int(value)],
    )
    .unwrap()
}

#[test]
fn strict_inbox_admission_returns_only_newly_committed_work_for_host_execution() {
    let (mut rt, id, behavior) = actor();
    rt.enable_strict_typed_actor_admission(id).unwrap();
    rt.send_message_by_id(id, behavior, &[Value::int(5)]);
    let proposal = transition(&rt, id, behavior, 5);
    let receipt_key = request(id, behavior, "fresh", 5);

    let admitted = rt
        .admit_next_typed_actor_command_with_inbox(id, receipt_key.clone(), proposal)
        .unwrap()
        .expect("queued command");
    let commit = match admitted {
        TypedInboxAdmission::New { message, commit } => {
            assert_eq!(message.payload[0].as_int(), Some(5));
            commit
        }
        TypedInboxAdmission::Duplicate { .. } => panic!("unexpected prior receipt"),
    };
    assert_eq!(
        rt.persistence
            .load_durable_inbox_receipt(receipt_key.id())
            .unwrap()
            .unwrap()
            .commit,
        commit
    );
    assert_eq!(rt.actors[&id].mailbox.admitted_command_sequence(), Some(commit.sequence));
    assert_eq!(
        rt.actors[&id]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(0),
        "admission never runs compiled user code or publishes state"
    );
}

#[test]
fn replayed_receipt_discards_duplicate_mailbox_envelope_without_running_bytecode() {
    let (mut rt, id, behavior) = actor();
    rt.enable_strict_typed_actor_admission(id).unwrap();
    let proposal = transition(&rt, id, behavior, 5);
    let receipt_key = request(id, behavior, "replayed", 5);
    let expected = rt
        .persistence
        .commit_inbox_transition(receipt_key.clone(), proposal.clone())
        .expect("simulated committed admission with lost acknowledgment");

    rt.send_message_by_id(id, behavior, &[Value::int(5)]);
    let outcome = rt
        .admit_next_typed_actor_command_with_inbox(id, receipt_key, proposal)
        .unwrap()
        .expect("duplicate found");
    match outcome {
        TypedInboxAdmission::Duplicate { commit } => assert_eq!(commit, expected),
        TypedInboxAdmission::New { .. } => panic!("duplicate cannot be returned to handler"),
    }
    assert_eq!(rt.actors[&id].mailbox.admitted_command_sequence(), None);
    assert_eq!(rt.actors[&id].mailbox.len(), 0);
    assert_eq!(
        rt.actors[&id]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(0)
    );
}

#[test]
fn failed_or_conflicting_inbox_admission_preserves_mailbox_and_predecessor() {
    let (mut rt, id, behavior) = actor();
    rt.enable_strict_typed_actor_admission(id).unwrap();
    let previous = rt.persistence.latest_sequence(id);
    rt.send_message_by_id(id, behavior, &[Value::int(5)]);
    let mismatched_key = request(id, behavior, "bad", 99);
    let result = rt.admit_next_typed_actor_command_with_inbox(
        id,
        mismatched_key,
        transition(&rt, id, behavior, 5),
    );
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(rt.actors[&id].mailbox.len(), 1);
    assert_eq!(rt.persistence.latest_sequence(id), previous);
}

#[test]
fn receipt_backed_admission_requires_explicit_strict_mode() {
    let (mut rt, id, behavior) = actor();
    rt.send_message_by_id(id, behavior, &[Value::int(5)]);
    let proposal = transition(&rt, id, behavior, 5);
    let err = rt
        .admit_next_typed_actor_command_with_inbox(
            id,
            request(id, behavior, "fresh", 5),
            proposal,
        )
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(rt.actors[&id].mailbox.len(), 1);
}
