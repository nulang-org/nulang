//! Atomic in-memory migration of legacy actor recovery records.
//! Exercises the public PersistenceStore contract without a running supervisor.
use std::io::ErrorKind;

use nulang::runtime::{
    ActorSnapshot, DurableTransition, EventEntry, JournalEntry, MemoryStore, PersistedValue,
    PersistenceStore, WorkflowEvent, DURABLE_TRANSITION_VERSION,
};

fn snapshot(actor_id: u64) -> ActorSnapshot {
    ActorSnapshot {
        actor_id,
        sequence: 3,
        state: [("counter".to_string(), PersistedValue::Int(42))]
            .into_iter()
            .collect(),
        ..ActorSnapshot::default()
    }
}

fn seed_legacy_history(store: &mut impl PersistenceStore, actor_id: u64) {
    store.save_snapshot(snapshot(actor_id)).unwrap();
    store
        .append_journal(
            actor_id,
            JournalEntry {
                sequence: 3,
                behavior_id: 5,
                payload: vec![PersistedValue::Int(7)],
            },
        )
        .unwrap();
    store
        .append_workflow_event(
            actor_id,
            WorkflowEvent::TimerFired {
                sequence: 3,
                name: "wake".to_string(),
            },
        )
        .unwrap();
    store
        .append_event(
            actor_id,
            EventEntry {
                sequence: 3,
                field_name: "counter".to_string(),
                event_name: "Increased".to_string(),
                args: vec![PersistedValue::Int(7)],
                value: PersistedValue::Int(42),
            },
        )
        .unwrap();
}

#[test]
fn memory_store_rekeys_legacy_snapshot_and_all_journals_together() {
    let mut store = MemoryStore::new();
    seed_legacy_history(&mut store, 100);

    store.rekey_actor_history(100, 101).unwrap();

    assert!(store.load_snapshot(100).is_none());
    assert!(store.read_journal(100).is_empty());
    assert!(store.read_workflow_events(100).is_empty());
    assert!(store.read_events(100).is_empty());

    let restored = store.load_snapshot(101).expect("re-keyed snapshot");
    assert_eq!(restored.actor_id, 101);
    assert_eq!(restored.sequence, 3);
    assert_eq!(restored.state["counter"], PersistedValue::Int(42));
    assert_eq!(store.read_journal(101)[0].behavior_id, 5);
    assert!(matches!(
        &store.read_workflow_events(101)[0],
        WorkflowEvent::TimerFired { name, .. } if name == "wake"
    ));
    assert_eq!(store.read_events(101)[0].value, PersistedValue::Int(42));
    assert_eq!(store.latest_sequence(101), 3);
}

#[test]
fn memory_store_rekey_rejects_destination_history_without_mutation() {
    let mut store = MemoryStore::new();
    seed_legacy_history(&mut store, 100);
    // Even a destination journal without a snapshot must be treated as occupied.
    store
        .append_journal(
            101,
            JournalEntry {
                sequence: 9,
                behavior_id: 8,
                payload: vec![],
            },
        )
        .unwrap();

    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(
        store.load_snapshot(100).unwrap().state["counter"],
        PersistedValue::Int(42)
    );
    assert_eq!(store.read_journal(100).len(), 1);
    assert_eq!(store.read_workflow_events(100).len(), 1);
    assert_eq!(store.read_events(100).len(), 1);
    assert_eq!(store.read_journal(101)[0].behavior_id, 8);
}

#[test]
fn memory_store_rekey_refuses_atomic_transition_identity_rewrite() {
    let mut store = MemoryStore::new();
    store
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: 100,
            activation_epoch: 1,
            sequence: 1,
            expected_previous_sequence: 0,
            command: None,
            snapshot: Some(ActorSnapshot {
                actor_id: 100,
                sequence: 1,
                ..ActorSnapshot::default()
            }),
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .unwrap();

    let old_digest = store.durable_tail(100).expect("committed tail").digest;
    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(store.durable_tail(100).unwrap().digest, old_digest);
    assert!(store.durable_tail(101).is_none());
    assert_eq!(store.committed_transitions(100).len(), 1);
    assert!(store.load_snapshot(101).is_none());
}

#[test]
fn memory_store_rekey_rejects_identity_bound_workflow_events() {
    let mut store = MemoryStore::new();
    seed_legacy_history(&mut store, 100);
    store
        .append_workflow_event(
            100,
            WorkflowEvent::StepCompleted {
                sequence: 4,
                activation: Some(nulang::runtime::WorkflowActivationId::new(100, 3)),
                step_name: "done".to_string(),
            },
        )
        .unwrap();

    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(store.load_snapshot(100).is_some());
    assert_eq!(store.read_workflow_events(100).len(), 2);
    assert!(store.load_snapshot(101).is_none());
}

#[test]
fn memory_store_rekey_rejects_replay_site_identity() {
    let mut store = MemoryStore::new();
    seed_legacy_history(&mut store, 100);
    store
        .append_workflow_event(
            100,
            WorkflowEvent::TimerSet {
                sequence: 4,
                replay_id: Some(nulang::runtime::WorkflowReplayEventId {
                    activation: nulang::runtime::WorkflowActivationId::new(100, 3),
                    ordinal: 0,
                }),
                name: "timeout".to_string(),
                duration_ms: 100,
            },
        )
        .unwrap();

    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(store.read_workflow_events(100).len(), 2);
    assert!(store.read_workflow_events(101).is_empty());
}

#[test]
fn memory_store_rekey_missing_source_or_same_identity_does_not_mutate() {
    let mut store = MemoryStore::new();
    assert_eq!(
        store.rekey_actor_history(100, 101).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    seed_legacy_history(&mut store, 100);
    assert_eq!(
        store.rekey_actor_history(100, 100).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert!(store.load_snapshot(100).is_some());
    assert_eq!(store.read_journal(100).len(), 1);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_rekeys_legacy_recovery_history_in_one_transaction() {
    let mut store = nulang::runtime::LibsqlStore::in_memory().unwrap();
    seed_legacy_history(&mut store, 100);
    store.rekey_actor_history(100, 101).unwrap();

    assert!(store.load_snapshot(100).is_none());
    assert!(store.read_journal(100).is_empty());
    assert!(store.read_workflow_events(100).is_empty());
    assert!(store.read_events(100).is_empty());
    assert_eq!(store.load_snapshot(101).unwrap().actor_id, 101);
    assert_eq!(
        store.load_snapshot(101).unwrap().state["counter"],
        PersistedValue::Int(42)
    );
    assert_eq!(store.read_journal(101)[0].behavior_id, 5);
    assert_eq!(store.read_workflow_events(101).len(), 1);
    assert_eq!(store.read_events(101)[0].value, PersistedValue::Int(42));
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_rekey_rejects_destination_collision_without_partial_move() {
    let mut store = nulang::runtime::LibsqlStore::in_memory().unwrap();
    seed_legacy_history(&mut store, 100);
    store.save_snapshot(snapshot(101)).unwrap();

    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(
        store.load_snapshot(100).unwrap().state["counter"],
        PersistedValue::Int(42)
    );
    assert_eq!(store.read_workflow_events(100).len(), 1);
    assert_eq!(store.read_events(100).len(), 1);
    assert_eq!(store.load_snapshot(101).unwrap().actor_id, 101);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_rekey_refuses_identity_bound_workflow_records() {
    let mut store = nulang::runtime::LibsqlStore::in_memory().unwrap();
    seed_legacy_history(&mut store, 100);
    store
        .append_workflow_event(
            100,
            WorkflowEvent::StepCompleted {
                sequence: 4,
                activation: Some(nulang::runtime::WorkflowActivationId::new(100, 3)),
                step_name: "step".to_string(),
            },
        )
        .unwrap();

    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(store.load_snapshot(100).is_some());
    assert!(store.load_snapshot(101).is_none());
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_rekey_refuses_committed_atomic_transition_records() {
    let mut store = nulang::runtime::LibsqlStore::in_memory().unwrap();
    store
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: 100,
            activation_epoch: 1,
            sequence: 1,
            expected_previous_sequence: 0,
            command: None,
            snapshot: Some(ActorSnapshot {
                actor_id: 100,
                sequence: 1,
                ..ActorSnapshot::default()
            }),
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .unwrap();

    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(store.load_snapshot(100).is_some());
    assert!(store.load_snapshot(101).is_none());
}

#[test]
fn memory_store_rekey_refuses_self_reference_in_snapshot() {
    let mut store = MemoryStore::new();
    seed_legacy_history(&mut store, 100);
    let mut source = store.load_snapshot(100).unwrap();
    source
        .state
        .insert("self_ref".to_string(), PersistedValue::Actor(100));
    store.save_snapshot(source).unwrap();

    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(
        store.load_snapshot(100).unwrap().state["self_ref"],
        PersistedValue::Actor(100)
    );
    assert!(store.load_snapshot(101).is_none());
}

#[test]
fn memory_store_rekey_refuses_self_reference_in_message_journal() {
    let mut store = MemoryStore::new();
    seed_legacy_history(&mut store, 100);
    store
        .append_journal(
            100,
            JournalEntry {
                sequence: 4,
                behavior_id: 6,
                payload: vec![PersistedValue::Actor(100)],
            },
        )
        .unwrap();

    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(store.read_journal(100).len(), 2);
    assert!(store.read_journal(101).is_empty());
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_rekey_refuses_self_reference_in_snapshot() {
    let mut store = nulang::runtime::LibsqlStore::in_memory().unwrap();
    seed_legacy_history(&mut store, 100);
    let mut source = store.load_snapshot(100).unwrap();
    source
        .state
        .insert("self_ref".to_string(), PersistedValue::Actor(100));
    store.save_snapshot(source).unwrap();

    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(
        store.load_snapshot(100).unwrap().state["self_ref"],
        PersistedValue::Actor(100)
    );
    assert!(store.load_snapshot(101).is_none());
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_rekey_refuses_self_reference_in_message_journal() {
    let mut store = nulang::runtime::LibsqlStore::in_memory().unwrap();
    seed_legacy_history(&mut store, 100);
    store
        .append_journal(
            100,
            JournalEntry {
                sequence: 4,
                behavior_id: 6,
                payload: vec![PersistedValue::Actor(100)],
            },
        )
        .unwrap();

    let err = store.rekey_actor_history(100, 101).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert_eq!(store.read_journal(100).len(), 2);
    assert!(store.read_journal(101).is_empty());
}
