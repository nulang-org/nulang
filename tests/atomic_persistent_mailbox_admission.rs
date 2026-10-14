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
    assert!(
        runtime.actors[&actor_id].mailbox.pop().is_none(),
        "a second user command must stay queued until admitted work is completed"
    );
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
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

#[derive(Default)]
struct AcknowledgmentLossStore {
    inner: nulang::runtime::MemoryStore,
    fail_after_commit: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PersistenceStore for AcknowledgmentLossStore {
    fn commit_transition(
        &mut self,
        transition: DurableTransition,
    ) -> std::io::Result<nulang::runtime::DurableCommit> {
        let commit = self.inner.commit_transition(transition)?;
        if self
            .fail_after_commit
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(std::io::Error::other("injected lost commit acknowledgment"));
        }
        Ok(commit)
    }

    fn save_snapshot(&mut self, snapshot: nulang::runtime::ActorSnapshot) -> std::io::Result<()> {
        self.inner.save_snapshot(snapshot)
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<nulang::runtime::ActorSnapshot> {
        self.inner.load_snapshot(actor_id)
    }

    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> std::io::Result<()> {
        self.inner.append_journal(actor_id, entry)
    }

    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
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
fn lost_commit_acknowledgment_retries_same_transition_without_duplicate_journal() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let (module, behavior, _) = compiled_counter();
    let lose_ack = Arc::new(AtomicBool::new(false));
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(AcknowledgmentLossStore {
        inner: nulang::runtime::MemoryStore::new(),
        fail_after_commit: Arc::clone(&lose_ack),
    });
    let actor_id = runtime
        .spawn_from_module(&module, behavior as usize, vec![])
        .as_actor_id()
        .expect("spawn persistent Counter");
    let predecessor = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);

    let same_turn = command(&runtime, actor_id, behavior, predecessor, 5);
    lose_ack.store(true, Ordering::SeqCst);

    assert!(runtime
        .admit_next_typed_actor_command(actor_id, same_turn.clone())
        .is_err());
    assert_eq!(
        runtime.persistence.latest_sequence(actor_id),
        predecessor + 1,
        "the original transaction did commit despite the lost acknowledgment"
    );
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
    assert_eq!(runtime.persistence.read_journal(actor_id).len(), 1);

    let (message, committed) = runtime
        .admit_next_typed_actor_command(actor_id, same_turn)
        .expect("repeat exact transition is idempotent")
        .expect("original message now acknowledged");
    assert_eq!(message.payload[0].as_int(), Some(5));
    assert_eq!(committed.sequence, predecessor + 1);
    assert_eq!(runtime.persistence.read_journal(actor_id).len(), 1);
    assert!(runtime.actors[&actor_id].mailbox.is_empty());
}

#[test]
fn command_admission_rejects_outbox_and_external_effect_dispatch() {
    let (mut runtime, actor_id, behavior, _) = spawned();
    let predecessor = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let mut unsafe_turn = command(&runtime, actor_id, behavior, predecessor, 5);
    unsafe_turn.outbox.push(nulang::runtime::DurableOutboxMessage {
        destination_actor_id: actor_id + 1,
        ordinal: 0,
        behavior_id: behavior,
        payload: vec![PersistedValue::Int(5)],
    });

    assert_eq!(
        runtime
            .admit_next_typed_actor_command(actor_id, unsafe_turn)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), predecessor);
}

#[test]
fn admitted_command_blocks_next_user_message_until_durable_completion() {
    let (mut runtime, actor_id, behavior, _) = spawned();
    let previous = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let admitted = command(&runtime, actor_id, behavior, previous, 5);
    let (_, receipt) = runtime
        .admit_next_typed_actor_command(actor_id, admitted)
        .unwrap()
        .expect("admitted message");

    runtime.send_message_by_id(actor_id, behavior, &[Value::int(6)]);
    assert!(
        runtime.actors.get_mut(&actor_id).unwrap().mailbox.pop().is_none(),
        "legacy scheduler must not steal the next user command before completion"
    );
    let next = command(&runtime, actor_id, behavior, receipt.sequence, 6);
    assert_eq!(
        runtime
            .admit_next_typed_actor_command(actor_id, next)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        runtime
            .finish_admitted_typed_actor_command(actor_id, receipt.sequence)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock,
        "an admitted journal entry without a durable completion must not unlock dispatch"
    );

    // The host commits resulting state separately. No user bytecode or
    // external provider runs in this test; this is a completion barrier proof.
    let mut completed_snapshot = runtime.persistence.load_snapshot(actor_id).unwrap_or_default();
    completed_snapshot.actor_id = actor_id;
    completed_snapshot.sequence = receipt.sequence + 1;
    completed_snapshot.activation_epoch = receipt.activation_epoch;
    completed_snapshot.schema_name = Some("Counter".to_string());
    completed_snapshot
        .state
        .insert("total".to_string(), PersistedValue::Int(5));
    runtime
        .persistence
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: receipt.activation_epoch,
            sequence: receipt.sequence + 1,
            expected_previous_sequence: receipt.sequence,
            command: None,
            snapshot: Some(completed_snapshot),
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .expect("commit completion snapshot");
    runtime
        .finish_admitted_typed_actor_command(actor_id, receipt.sequence)
        .expect("completion safely releases next user command");
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(5),
        "live actor state must match durable completion before next command starts"
    );
    let next_message = runtime.actors.get_mut(&actor_id).unwrap().mailbox.pop().unwrap();
    assert_eq!(next_message.payload[0].as_int(), Some(6));
}

#[test]
fn supervisor_notices_still_bypass_admitted_inflight_user_command() {
    use nulang::runtime::{Message, MessagePayload, MessagePriority};

    let (mut runtime, actor_id, behavior, _) = spawned();
    let previous = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let first = command(&runtime, actor_id, behavior, previous, 5);
    runtime
        .admit_next_typed_actor_command(actor_id, first)
        .unwrap()
        .expect("admitted");
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(6)]);
    runtime
        .actors
        .get_mut(&actor_id)
        .unwrap()
        .mailbox
        .push_local(Message {
            behavior_id: 0,
            payload: MessagePayload::from_slice(&[Value::int(99)]),
            sender: 99,
            priority: MessagePriority::System,
            trace_id: None,
        })
        .unwrap();

    let mailbox = &mut runtime.actors.get_mut(&actor_id).unwrap().mailbox;
    assert_eq!(mailbox.pop().unwrap().sender, 99);
    assert!(mailbox.pop().is_none());
    assert_eq!(mailbox.len(), 1);
}

#[test]
fn mismatched_completion_schema_cannot_release_admitted_message_fence() {
    let (mut runtime, actor_id, behavior, _) = spawned();
    let previous = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(4)]);
    let first = command(&runtime, actor_id, behavior, previous, 4);
    let (_, receipt) = runtime
        .admit_next_typed_actor_command(actor_id, first)
        .unwrap()
        .unwrap();

    let mut fake_snapshot = runtime.persistence.load_snapshot(actor_id).unwrap_or_default();
    fake_snapshot.actor_id = actor_id;
    fake_snapshot.activation_epoch = receipt.activation_epoch;
    fake_snapshot.sequence = receipt.sequence + 1;
    fake_snapshot.schema_name = Some("WrongSchema".to_string());
    runtime
        .persistence
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: receipt.activation_epoch,
            sequence: receipt.sequence + 1,
            expected_previous_sequence: receipt.sequence,
            command: None,
            snapshot: Some(fake_snapshot),
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .unwrap();

    assert_eq!(
        runtime
            .finish_admitted_typed_actor_command(actor_id, receipt.sequence)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(receipt.sequence)
    );
}

#[test]
fn unsupported_message_values_cannot_be_normalized_to_nil_in_durable_history() {
    use nulang::runtime::{Message, MessagePayload, MessagePriority};

    let (mut runtime, actor_id, behavior, _) = spawned();
    let previous = runtime.persistence.latest_sequence(actor_id);
    // Model a dynamically delivered object-store reference without a stable
    // canonical codec. Legacy conversion would silently journal this as Nil.
    runtime
        .actors
        .get_mut(&actor_id)
        .unwrap()
        .mailbox
        .push_local(Message {
            behavior_id: behavior,
            payload: MessagePayload::from_slice(&[Value::object(999)]),
            sender: 0,
            priority: MessagePriority::Normal,
            trace_id: None,
        })
        .unwrap();

    let mut forged = command(&runtime, actor_id, behavior, previous, 0);
    forged.command.as_mut().unwrap().payload = vec![PersistedValue::Nil];
    assert_eq!(
        runtime
            .admit_next_typed_actor_command(actor_id, forged)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(runtime.persistence.latest_sequence(actor_id), previous);
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
}

#[test]
fn completion_cannot_install_forged_external_authority_from_snapshot() {
    let (mut runtime, actor_id, behavior, _) = spawned();
    let previous = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let first = command(&runtime, actor_id, behavior, previous, 5);
    let (_, accepted) = runtime
        .admit_next_typed_actor_command(actor_id, first)
        .unwrap()
        .expect("atomic admission");

    let mut completion = runtime.persistence.load_snapshot(actor_id).unwrap_or_default();
    completion.actor_id = actor_id;
    completion.sequence = accepted.sequence + 1;
    completion.activation_epoch = accepted.activation_epoch;
    completion.schema_name = Some("Counter".into());
    completion.state.insert("total".into(), PersistedValue::Int(5));
    completion
        .authority_tokens
        .insert("Net::TcpOut(*)".to_string());
    runtime
        .persistence
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: accepted.activation_epoch,
            sequence: accepted.sequence + 1,
            expected_previous_sequence: accepted.sequence,
            command: None,
            snapshot: Some(completion),
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .expect("stored completion, without granting host authority");

    assert_eq!(
        runtime
            .finish_admitted_typed_actor_command(actor_id, accepted.sequence)
            .expect_err("snapshot cannot grant new authority")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(accepted.sequence),
        "untrusted completion must not release the next user command"
    );
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(0),
        "untrusted completion must not publish state"
    );
}

#[test]
fn synchronous_ask_cannot_bypass_committed_but_unfinished_durable_command() {
    let (mut runtime, actor_id, behavior, _) = spawned();
    let previous = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let first = command(&runtime, actor_id, behavior, previous, 5);
    let (_, receipt) = runtime
        .admit_next_typed_actor_command(actor_id, first)
        .unwrap()
        .expect("admitted message");

    let error = runtime
        .ask_actor_sync(actor_id, behavior, &[Value::int(99)])
        .expect_err("sync ask cannot bypass admitted completion fence");
    assert!(error.to_string().contains("durable admission"));
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0),
        "sync ask must not execute while a durable command is unfinished"
    );
    assert_eq!(runtime.persistence.latest_sequence(actor_id), receipt.sequence);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(receipt.sequence)
    );
}

#[test]
fn synchronous_ask_cannot_bypass_failed_durable_admission() {
    let (mut runtime, actor_id, behavior, _) = spawned();
    let previous = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let invalid = command(&runtime, actor_id, behavior, previous + 1, 5);
    assert!(runtime.admit_next_typed_actor_command(actor_id, invalid).is_err());

    let error = runtime
        .ask_actor_sync(actor_id, behavior, &[Value::int(7)])
        .expect_err("sync ask cannot jump ahead of uncommitted mailbox candidate");
    assert!(error.to_string().contains("durable admission"));
    assert_eq!(runtime.persistence.latest_sequence(actor_id), previous);
    assert_eq!(runtime.actors[&actor_id].mailbox.len(), 1);
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0)
    );
}

#[test]
fn completion_rejects_incompatible_compiler_declared_state_type() {
    let (mut runtime, actor_id, behavior, _) = spawned();
    let previous = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let first = command(&runtime, actor_id, behavior, previous, 5);
    let (_, admitted) = runtime
        .admit_next_typed_actor_command(actor_id, first)
        .unwrap()
        .expect("persisted command");

    let mut completion = runtime.persistence.load_snapshot(actor_id).unwrap_or_default();
    completion.actor_id = actor_id;
    completion.sequence = admitted.sequence + 1;
    completion.activation_epoch = admitted.activation_epoch;
    completion.schema_name = Some("Counter".to_string());
    completion
        .state
        .insert("total".to_string(), PersistedValue::String("five".to_string()));
    runtime
        .persistence
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: admitted.activation_epoch,
            sequence: admitted.sequence + 1,
            expected_previous_sequence: admitted.sequence,
            command: None,
            snapshot: Some(completion),
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .expect("backend stores envelope; trusted host must validate schema");

    assert_eq!(
        runtime
            .finish_admitted_typed_actor_command(actor_id, admitted.sequence)
            .expect_err("string state cannot satisfy compiler-declared integer field")
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(admitted.sequence)
    );
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0),
        "incompatible persisted state cannot be published"
    );
}

#[test]
fn completion_snapshot_cannot_change_compiled_integer_field_to_string() {
    let (mut runtime, actor_id, behavior, _) = spawned();
    let previous = runtime.persistence.latest_sequence(actor_id);
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(5)]);
    let first = command(&runtime, actor_id, behavior, previous, 5);
    let (_, accepted) = runtime
        .admit_next_typed_actor_command(actor_id, first)
        .unwrap()
        .expect("command admitted");

    let mut invalid = runtime.persistence.load_snapshot(actor_id).unwrap_or_default();
    invalid.actor_id = actor_id;
    invalid.sequence = accepted.sequence + 1;
    invalid.activation_epoch = accepted.activation_epoch;
    invalid.schema_name = Some("Counter".to_string());
    invalid.state.insert(
        "total".to_string(),
        PersistedValue::String("not an Int".to_string()),
    );
    runtime
        .persistence
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: accepted.activation_epoch,
            sequence: accepted.sequence + 1,
            expected_previous_sequence: accepted.sequence,
            command: None,
            snapshot: Some(invalid),
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .expect("storage accepts an opaque snapshot");

    let error = runtime
        .finish_admitted_typed_actor_command(actor_id, accepted.sequence)
        .expect_err("runtime must reject persisted state type mismatch");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(accepted.sequence),
        "invalid state must not unlock later commands"
    );
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0),
        "invalid completion must not corrupt live state"
    );
}
