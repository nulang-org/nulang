use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::{DurableEffectPersistenceRecord, WorkflowEffectBinding};
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableTransition, JsonFileStore, MemoryStore, PersistenceStore,
    WorkflowActivationId, DURABLE_TRANSITION_VERSION,
};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};

fn spec() -> DurableEffectSpec {
    let site = effect_site_id(
        "durable-effect-recovery-store-tests",
        EffectSiteOwnerKind::Behavior,
        "TestActor.run",
        "Provider.ask",
        0,
    );
    DurableEffectSpec::new(
        DurableEffectId::derive_from_site(42, "turn:7", site, 0),
        "Provider.ask",
        EffectBoundary::External,
        DeliverySemantics::AtLeastOnce,
    )
}

fn transition(
    store: &dyn PersistenceStore,
    actor_id: u64,
    epoch: u64,
    record: DurableEffectPersistenceRecord,
) -> DurableTransition {
    let previous = store.latest_sequence(actor_id);
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: epoch,
        sequence: previous + 1,
        expected_previous_sequence: previous,
        command: None,
        workflow_admission: false,
        workflow_effects_consumed: None,
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![record],
        outbox: vec![],
    }
}

#[test]
fn memory_store_loads_newest_record_for_logical_effect() {
    let mut store = MemoryStore::new();
    let prepared = DurableEffectRecord::prepare(spec(), b"request");
    let id = prepared.spec().id;

    let first = transition(
        &store,
        42,
        1,
        DurableEffectPersistenceRecord::from_effect(prepared.clone()),
    );
    store.commit_transition(first).unwrap();

    let completed = prepared.complete(b"result".to_vec());
    let second = transition(
        &store,
        42,
        1,
        DurableEffectPersistenceRecord::from_effect(completed.clone()),
    );
    store.commit_transition(second).unwrap();

    let loaded = store.load_durable_effect(42, id).unwrap().unwrap();
    assert_eq!(loaded.effect(), &completed);
}

#[test]
fn unsupported_store_fails_closed_instead_of_reporting_missing_effect() {
    let base = std::env::temp_dir().join(format!(
        "nulang_durable_effect_recovery_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let store = JsonFileStore::new(&base).unwrap();

    let error = store
        .load_durable_effect(42, spec().id)
        .expect_err("legacy non-atomic backend must not pretend recovery history is empty");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);

    assert_eq!(
        store
            .load_workflow_effect_occurrence(WorkflowActivationId::new(42, 1), 0)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(
        store.load_open_workflow_command(42).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_store_loads_newest_record_for_logical_effect() {
    use nulang::runtime::LibsqlStore;

    let mut store = LibsqlStore::in_memory().unwrap();
    let prepared = DurableEffectRecord::prepare(spec(), b"request");
    let id = prepared.spec().id;

    let first = transition(
        &store,
        42,
        1,
        DurableEffectPersistenceRecord::from_effect(prepared.clone()),
    );
    store.commit_transition(first).unwrap();

    let completed = prepared.complete(b"result".to_vec());
    let second = transition(
        &store,
        42,
        1,
        DurableEffectPersistenceRecord::from_effect(completed.clone()),
    );
    store.commit_transition(second).unwrap();

    let loaded = store.load_durable_effect(42, id).unwrap().unwrap();
    assert_eq!(loaded.effect(), &completed);
}

fn slot(occurrence: u32, site: u8) -> WorkflowEffectBinding {
    WorkflowEffectBinding {
        activation: WorkflowActivationId::new(42, 1),
        occurrence,
        site_id: [site; 32],
    }
}

fn bound_prepared(
    binding: WorkflowEffectBinding,
    request: &[u8],
) -> DurableEffectPersistenceRecord {
    let spec = DurableEffectSpec::new(
        binding.effect_id(),
        "Provider.ask",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    DurableEffectPersistenceRecord::from_workflow_occurrence(
        binding,
        DurableEffectRecord::prepare(spec, request),
    )
    .unwrap()
}

fn bound_completed(
    record: &DurableEffectPersistenceRecord,
    result: &[u8],
) -> DurableEffectPersistenceRecord {
    DurableEffectPersistenceRecord::from_workflow_occurrence(
        record.workflow_binding().unwrap(),
        record.effect().clone().complete(result.to_vec()),
    )
    .unwrap()
}

fn admit(store: &mut dyn PersistenceStore) {
    store
        .save_snapshot(ActorSnapshot {
            actor_id: 42,
            activation_epoch: 1,
            ..ActorSnapshot::default()
        })
        .unwrap();
    store
        .commit_transition(DurableTransition::workflow_command(42, 1, 0, 0, vec![]).unwrap())
        .unwrap();
}

fn assert_slot_contract(store: &mut dyn PersistenceStore) {
    admit(store);
    let binding = slot(0, 1);
    let prepared = bound_prepared(binding, b"request");
    let completed = bound_completed(&prepared, b"result");
    assert!(store
        .load_workflow_effect_occurrence(binding.activation, 0)
        .unwrap()
        .is_none());

    // No completion can invent an intent.
    let missing_intent = transition(store, 42, 1, completed.clone());
    assert!(store.commit_transition(missing_intent).is_err());
    assert_eq!(store.latest_sequence(42), 1);

    let skipped = transition(store, 42, 1, bound_prepared(slot(1, 1), b"request"));
    assert!(store.commit_transition(skipped).is_err());
    assert_eq!(store.latest_sequence(42), 1);
    let first = transition(store, 42, 1, prepared.clone());
    let initial = store.commit_transition(first.clone()).unwrap();
    assert_eq!(
        store.commit_transition(first).unwrap(),
        initial,
        "exact-tail retry remains idempotent"
    );
    assert_eq!(
        store
            .load_workflow_effect_occurrence(binding.activation, 0)
            .unwrap(),
        Some(prepared.clone())
    );
    // Even a caller that bypasses the adapter and picks the current
    // predecessor cannot bind a second semantic site to the same slot.
    let other = bound_prepared(slot(0, 2), b"request");
    let mut conflict = transition(store, 42, 1, other.clone());
    conflict.snapshot = Some(ActorSnapshot {
        actor_id: 42,
        sequence: 3,
        activation_epoch: 1,
        ..ActorSnapshot::default()
    });
    assert!(store.commit_transition(conflict).is_err());
    assert_eq!(store.load_snapshot(42).unwrap().sequence, 0);
    assert!(store
        .load_durable_effect(42, other.effect().spec().id)
        .unwrap()
        .is_none());
    let skipped_pending = transition(store, 42, 1, bound_prepared(slot(1, 1), b"request"));
    assert!(store.commit_transition(skipped_pending).is_err());
    assert!(store
        .load_workflow_effect_occurrence(binding.activation, 1)
        .is_err());
    let changed_request = bound_completed(&bound_prepared(binding, b"changed"), b"result");
    let drift = transition(store, 42, 1, changed_request);
    assert!(store.commit_transition(drift).is_err());
    let duplicate = transition(store, 42, 1, prepared.clone());
    assert!(store.commit_transition(duplicate).is_err());
    assert_eq!(store.latest_sequence(42), 2);

    let finish = transition(store, 42, 1, completed.clone());
    store.commit_transition(finish).unwrap();
    assert_eq!(
        store
            .load_workflow_effect_occurrence(binding.activation, 0)
            .unwrap(),
        Some(completed.clone())
    );
    let changed_result = transition(store, 42, 1, bound_completed(&prepared, b"changed result"));
    assert!(store.commit_transition(changed_result).is_err());
    let later = bound_prepared(slot(1, 1), b"request");
    let later_transition = transition(store, 42, 1, later.clone());
    store.commit_transition(later_transition).unwrap();
    assert_eq!(
        store
            .load_workflow_effect_occurrence(binding.activation, 1)
            .unwrap(),
        Some(later)
    );
    assert_eq!(
        store
            .load_workflow_effect_occurrence(binding.activation, 0)
            .unwrap(),
        Some(completed)
    );
    assert_eq!(store.latest_sequence(42), 4);
}

#[test]
fn memory_workflow_slot_commit_is_immutable_and_atomic() {
    assert_slot_contract(&mut MemoryStore::new());
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_workflow_slot_commit_is_immutable_and_atomic() {
    assert_slot_contract(&mut nulang::runtime::LibsqlStore::in_memory().unwrap());
}

fn assert_unbound_history_rejected(store: &mut dyn PersistenceStore) {
    let mut legacy = DurableTransition::workflow_command(42, 1, 0, 0, vec![]).unwrap();
    legacy.workflow_admission = false;
    store.commit_transition(legacy).unwrap();
    assert!(store.load_open_workflow_command(42).unwrap().is_none());
    let old = transition(
        store,
        42,
        1,
        DurableEffectPersistenceRecord::from_effect(
            bound_prepared(slot(0, 1), b"request").effect().clone(),
        ),
    );
    store.commit_transition(old).unwrap();
    let binding = slot(0, 2);
    assert_eq!(
        store
            .load_workflow_effect_occurrence(binding.activation, 0)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    let new = transition(store, 42, 1, bound_prepared(binding, b"request"));
    assert!(store.commit_transition(new).is_err());
    assert_eq!(store.latest_sequence(42), 2);
}

#[test]
fn memory_old_unbound_workflow_history_fails_closed() {
    assert_unbound_history_rejected(&mut MemoryStore::new());
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_old_unbound_workflow_history_fails_closed() {
    assert_unbound_history_rejected(&mut nulang::runtime::LibsqlStore::in_memory().unwrap());
}

#[test]
fn workflow_binding_codec_keeps_v1_unchanged_and_validates_v2_identity() {
    let binding = slot(0, 1);
    let prepared = bound_prepared(binding, b"request");
    let completed = bound_completed(&prepared, b"result");
    for record in [prepared, completed] {
        let data = record.to_json().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(value["version"], 2);
        assert_eq!(
            DurableEffectPersistenceRecord::from_json(&data).unwrap(),
            record
        );
        value["record"]["value"]["occurrence"] = 1.into();
        assert!(
            DurableEffectPersistenceRecord::from_json(&serde_json::to_vec(&value).unwrap())
                .is_err()
        );
        let mut value: serde_json::Value = serde_json::from_slice(&data).unwrap();
        value["version"] = 1.into();
        assert!(
            DurableEffectPersistenceRecord::from_json(&serde_json::to_vec(&value).unwrap())
                .is_err()
        );
        let legacy = DurableEffectPersistenceRecord::from_effect(record.effect().clone());
        let legacy_bytes = legacy.to_json().unwrap();
        let mut legacy_value: serde_json::Value = serde_json::from_slice(&legacy_bytes).unwrap();
        assert_eq!(legacy_value["version"], 1);
        assert_eq!(
            DurableEffectPersistenceRecord::from_json(&legacy_bytes).unwrap(),
            legacy
        );
        legacy_value["version"] = 2.into();
        assert!(DurableEffectPersistenceRecord::from_json(
            &serde_json::to_vec(&legacy_value).unwrap()
        )
        .is_err());
    }
}

fn assert_workflow_admission_contract(store: &mut dyn PersistenceStore) {
    use nulang::runtime::{EventEntry, JournalEntry, PersistedValue, WorkflowEvent};
    assert!(store.load_open_workflow_command(42).unwrap().is_none());
    let admission =
        DurableTransition::workflow_command(42, 1, 0, 7, vec![PersistedValue::Int(8)]).unwrap();
    assert!(
        store.commit_transition(admission.clone()).is_err(),
        "no safe snapshot"
    );
    assert_eq!(store.latest_sequence(42), 0);
    store
        .save_snapshot(ActorSnapshot {
            actor_id: 42,
            activation_epoch: 1,
            ..ActorSnapshot::default()
        })
        .unwrap();
    let mut invalid = admission.clone();
    invalid.snapshot = store.load_snapshot(42);
    assert!(store.commit_transition(invalid).is_err());
    let admitted = store.commit_transition(admission.clone()).unwrap();
    assert_eq!(
        store.commit_transition(admission.clone()).unwrap(),
        admitted
    );
    let mut changed = admission.clone();
    changed.command.as_mut().unwrap().payload = vec![PersistedValue::Int(9)];
    assert!(store.commit_transition(changed).is_err());
    let open = store.load_open_workflow_command(42).unwrap().unwrap();
    assert_eq!(open.activation, WorkflowActivationId::new(42, 1));
    assert_eq!(open.tail.sequence, 1);
    assert_eq!(open.tail.activation_epoch, 1);
    assert_eq!(
        store.read_journal(42)[0].payload,
        vec![PersistedValue::Int(8)]
    );
    assert!(store
        .commit_transition(DurableTransition::workflow_command(42, 1, 1, 7, vec![]).unwrap())
        .is_err());
    assert!(store
        .append_journal(
            42,
            JournalEntry {
                sequence: 2,
                behavior_id: 7,
                payload: vec![]
            }
        )
        .is_err());
    assert!(store
        .save_snapshot(ActorSnapshot {
            actor_id: 42,
            sequence: 0,
            activation_epoch: 1,
            ..ActorSnapshot::default()
        })
        .is_err());
    assert!(store
        .append_workflow_event(
            42,
            WorkflowEvent::StepCompleted {
                sequence: 2,
                activation: Some(open.activation),
                step_name: "step".into()
            }
        )
        .is_err());
    assert!(store
        .append_event(
            42,
            EventEntry {
                sequence: 2,
                field_name: "count".into(),
                event_name: "change".into(),
                args: vec![],
                value: PersistedValue::Int(1)
            }
        )
        .is_err());
    let record = bound_prepared(slot(0, 1), b"request");
    let unbound = DurableEffectPersistenceRecord::from_effect(record.effect().clone());
    assert!(store
        .commit_transition(transition(store, 42, 1, unbound))
        .is_err());
    assert!(
        store
            .commit_transition(transition(store, 42, 2, record.clone()))
            .is_err(),
        "effect cannot itself claim a newer epoch"
    );
    store
        .commit_transition(transition(store, 42, 1, record.clone()))
        .unwrap();

    let expected = store.latest_sequence(42);
    let mut terminal = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 42,
        activation_epoch: 1,
        sequence: expected + 1,
        expected_previous_sequence: expected,
        command: None,
        workflow_admission: false,
        workflow_effects_consumed: Some(1),
        snapshot: Some(ActorSnapshot {
            actor_id: 42,
            activation_epoch: 1,
            sequence: expected + 1,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::StepCompleted {
            sequence: expected + 1,
            activation: Some(open.activation),
            step_name: "step".into(),
        }],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    };
    assert!(
        store.commit_transition(terminal.clone()).is_err(),
        "cannot close while an effect is Prepared"
    );
    assert_eq!(store.load_snapshot(42).unwrap().sequence, 0);
    assert_eq!(store.latest_sequence(42), expected);
    store
        .commit_transition(transition(
            store,
            42,
            1,
            bound_completed(&record, b"receipt"),
        ))
        .unwrap();
    terminal.sequence += 1;
    terminal.expected_previous_sequence += 1;
    terminal.snapshot.as_mut().unwrap().sequence = terminal.sequence;
    terminal.workflow_events = vec![WorkflowEvent::StepCompleted {
        sequence: terminal.sequence,
        activation: Some(open.activation),
        step_name: "step".into(),
    }];
    let mut unqualified = terminal.clone();
    unqualified.snapshot = None;
    assert!(store.commit_transition(unqualified).is_err());
    let closed = store.commit_transition(terminal.clone()).unwrap();
    assert_eq!(store.commit_transition(terminal.clone()).unwrap(), closed);
    assert!(store.load_open_workflow_command(42).unwrap().is_none());
    assert!(store
        .commit_transition(transition(
            store,
            42,
            1,
            bound_prepared(slot(1, 1), b"request")
        ))
        .is_err());
    let next = DurableTransition::workflow_command(42, 1, closed.sequence, 8, vec![]).unwrap();
    let sequence = next.sequence;
    store.commit_transition(next).unwrap();
    assert_eq!(
        store
            .load_open_workflow_command(42)
            .unwrap()
            .unwrap()
            .activation
            .command_sequence,
        sequence
    );
}

#[test]
fn memory_atomic_admission_lifecycle() {
    assert_workflow_admission_contract(&mut MemoryStore::new());
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_atomic_admission_lifecycle() {
    assert_workflow_admission_contract(&mut nulang::runtime::LibsqlStore::in_memory().unwrap());
}

#[test]
fn workflow_admission_digest_distinguishes_explicit_marker() {
    let admission = DurableTransition::workflow_command(42, 1, 0, 0, vec![]).unwrap();
    let mut generic = admission.clone();
    generic.workflow_admission = false;
    assert_ne!(admission.digest().unwrap(), generic.digest().unwrap());
    // Exact canonical digest for the pre-marker v1 envelope. Not merely
    // comparing two digests generated by the new implementation.
    let old = r#"{"activation_epoch":1,"actor_id":42,"command":{"behavior_id":0,"payload":[],"sequence":1},"domain_events":[],"durable_effects":[],"expected_previous_sequence":0,"outbox":[],"sequence":1,"snapshot":null,"version":1,"workflow_events":[]}"#;
    assert_eq!(
        generic.digest().unwrap(),
        *blake3::hash(old.as_bytes()).as_bytes()
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_admission_and_terminal_marker_rollback_and_reopen() {
    use nulang::runtime::{LibsqlStore, WorkflowEvent};
    use std::time::{SystemTime, UNIX_EPOCH};
    let path = std::env::temp_dir().join(format!(
        "nulang-workflow-admission-{}-{}.db",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut store = LibsqlStore::new(&path).unwrap();
    store
        .save_snapshot(ActorSnapshot {
            actor_id: 42,
            activation_epoch: 1,
            ..ActorSnapshot::default()
        })
        .unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    // Force a database error *after* journal and durable transition inserts.
    raw.execute_batch("CREATE TRIGGER fail_admit BEFORE INSERT ON durable_workflow_admissions BEGIN SELECT RAISE(ABORT, 'admission fault'); END;").unwrap();
    let admission = DurableTransition::workflow_command(42, 1, 0, 7, vec![]).unwrap();
    assert!(store.commit_transition(admission.clone()).is_err());
    assert!(store.read_journal(42).is_empty());
    assert!(store.load_durable_tail_position(42).unwrap().is_none());
    assert!(store.load_open_workflow_command(42).unwrap().is_none());
    raw.execute_batch("DROP TRIGGER fail_admit;").unwrap();
    store.commit_transition(admission).unwrap();
    drop(store);
    let mut store = LibsqlStore::new(&path).unwrap();
    let activation = store
        .load_open_workflow_command(42)
        .unwrap()
        .unwrap()
        .activation;
    assert_eq!(activation, WorkflowActivationId::new(42, 1));
    let mut competing = LibsqlStore::new(&path).unwrap();
    assert!(competing
        .commit_transition(DurableTransition::workflow_command(42, 1, 1, 8, vec![]).unwrap())
        .is_err());
    assert!(competing
        .save_snapshot(ActorSnapshot {
            actor_id: 42,
            sequence: 0,
            activation_epoch: 1,
            ..ActorSnapshot::default()
        })
        .is_err());
    drop(competing);

    let close = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 42,
        activation_epoch: 1,
        sequence: 2,
        expected_previous_sequence: 1,
        command: None,
        workflow_admission: false,
        workflow_effects_consumed: Some(0),
        snapshot: Some(ActorSnapshot {
            actor_id: 42,
            activation_epoch: 1,
            sequence: 2,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::StepCompleted {
            sequence: 2,
            activation: Some(activation),
            step_name: "step".into(),
        }],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    };
    raw.execute_batch("CREATE TRIGGER fail_close BEFORE UPDATE ON durable_workflow_admissions BEGIN SELECT RAISE(ABORT, 'closure fault'); END;").unwrap();
    assert!(store.commit_transition(close.clone()).is_err());
    assert_eq!(store.load_snapshot(42).unwrap().sequence, 0);
    assert!(store.read_workflow_events(42).is_empty());
    assert_eq!(
        store
            .load_open_workflow_command(42)
            .unwrap()
            .unwrap()
            .tail
            .sequence,
        1
    );
    raw.execute_batch("DROP TRIGGER fail_close;").unwrap();
    store.commit_transition(close).unwrap();
    drop(store);
    drop(raw);
    let store = LibsqlStore::new(&path).unwrap();
    assert!(store.load_open_workflow_command(42).unwrap().is_none());
    assert_eq!(store.load_snapshot(42).unwrap().sequence, 2);
    assert_eq!(store.read_journal(42).len(), 1);
    assert_eq!(store.read_workflow_events(42).len(), 1);
    drop(store);
    std::fs::remove_file(path).unwrap();
}

fn terminal_after_two_effects(store: &mut dyn PersistenceStore) -> DurableTransition {
    use nulang::runtime::WorkflowEvent;
    admit(store);
    for occurrence in 0..2 {
        let record = bound_prepared(slot(occurrence, 1), b"request");
        store
            .commit_transition(transition(store, 42, 1, record.clone()))
            .unwrap();
        store
            .commit_transition(transition(
                store,
                42,
                1,
                bound_completed(&record, b"receipt"),
            ))
            .unwrap();
    }
    let activation = WorkflowActivationId::new(42, 1);
    let sequence = store.latest_sequence(42) + 1;
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 42,
        activation_epoch: 1,
        sequence,
        expected_previous_sequence: sequence - 1,
        command: None,
        workflow_admission: false,
        workflow_effects_consumed: Some(2),
        snapshot: Some(ActorSnapshot {
            actor_id: 42,
            activation_epoch: 1,
            sequence,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::StepCompleted {
            sequence,
            activation: Some(activation),
            step_name: "step".into(),
        }],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

fn assert_terminal_cursor_contract(store: &mut dyn PersistenceStore) {
    let terminal = terminal_after_two_effects(store);
    let sequence = terminal.sequence;
    let activation = WorkflowActivationId::new(42, 1);
    for evidence in [None, Some(0), Some(1), Some(3)] {
        let mut invalid = terminal.clone();
        invalid.workflow_effects_consumed = evidence;
        assert!(
            store.commit_transition(invalid).is_err(),
            "must reject missing, short or over-counted cursor evidence: {evidence:?}"
        );
        assert_eq!(store.load_snapshot(42).unwrap().sequence, 0);
        assert_eq!(store.latest_sequence(42), sequence - 1);
        assert_eq!(
            store
                .load_open_workflow_command(42)
                .unwrap()
                .unwrap()
                .activation,
            activation
        );
        assert!(store.read_workflow_events(42).is_empty());
    }
    let mut changed = terminal.clone();
    changed.workflow_effects_consumed = Some(1);
    assert_ne!(changed.digest().unwrap(), terminal.digest().unwrap());
    let committed = store.commit_transition(terminal.clone()).unwrap();
    assert_eq!(store.commit_transition(terminal).unwrap(), committed);
    assert!(store.commit_transition(changed).is_err());
    assert!(store.load_open_workflow_command(42).unwrap().is_none());
    assert_eq!(store.load_snapshot(42).unwrap().sequence, sequence);
}

#[test]
fn memory_terminal_cursor_reconciles_all_effect_occurrences() {
    assert_terminal_cursor_contract(&mut MemoryStore::new());
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_terminal_cursor_reconciles_all_effect_occurrences() {
    assert_terminal_cursor_contract(&mut nulang::runtime::LibsqlStore::in_memory().unwrap());
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_terminal_rejects_missing_earlier_intent_even_with_valid_last_receipt() {
    use nulang::runtime::LibsqlStore;
    use std::time::{SystemTime, UNIX_EPOCH};
    let path = std::env::temp_dir().join(format!(
        "nulang-workflow-terminal-corrupt-{}-{}.db",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut store = LibsqlStore::new(&path).unwrap();
    let terminal = terminal_after_two_effects(&mut store);
    // Simulate corruption of the experimental effect table: keep the last
    // terminal receipt intact but remove the first occurrence's intent.
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        raw.execute(
            "DELETE FROM durable_effect_records WHERE actor_id = 42 AND sequence = 2",
            []
        )
        .unwrap(),
        1
    );
    assert!(store.commit_transition(terminal).is_err());
    assert_eq!(store.latest_sequence(42), 5);
    assert_eq!(store.load_snapshot(42).unwrap().sequence, 0);
    assert!(store.read_workflow_events(42).is_empty());
    assert!(store.load_open_workflow_command(42).unwrap().is_some());
    drop(raw);
    drop(store);
    std::fs::remove_file(path).unwrap();
}
