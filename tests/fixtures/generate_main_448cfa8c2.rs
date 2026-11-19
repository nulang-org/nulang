#![cfg(feature = "sqlite")]
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, EventEntry, JournalEntry, LibsqlStore,
    PersistedValue, PersistenceStore, WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use std::collections::HashMap;

#[test]
fn create_real_legacy_libsql_fixture() {
    let path =
        std::env::var("NULANG_LEGACY_SQLITE_FIXTURE_PATH").expect("fixture path must be explicit");
    let mut store = LibsqlStore::new(&path).expect("legacy engine must open file");
    let mut state = HashMap::new();
    state.insert("count".to_string(), PersistedValue::Int(5));
    state.insert(
        "label".to_string(),
        PersistedValue::String("legacy-actor".to_string()),
    );
    let before = ActorSnapshot {
        actor_id: 42,
        sequence: 5,
        activation_epoch: 1,
        state: state.clone(),
        ..ActorSnapshot::default()
    };
    store.save_snapshot(before).unwrap();
    store
        .append_journal(
            42,
            JournalEntry {
                sequence: 5,
                behavior_id: 3,
                payload: vec![PersistedValue::String("first".into())],
            },
        )
        .unwrap();
    store
        .append_workflow_event(
            42,
            WorkflowEvent::Custom {
                sequence: 5,
                replay_id: None,
                name: "legacy-flow".into(),
                args: vec![PersistedValue::Int(5)],
            },
        )
        .unwrap();
    store
        .append_event(
            42,
            EventEntry {
                sequence: 5,
                field_name: "count".into(),
                event_name: "Changed".into(),
                args: vec![],
                value: PersistedValue::Int(5),
            },
        )
        .unwrap();
    state.insert("count".to_string(), PersistedValue::Int(6));
    let transition = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 42,
        activation_epoch: 1,
        sequence: 6,
        expected_previous_sequence: 5,
        command: Some(JournalEntry {
            sequence: 6,
            behavior_id: 4,
            payload: vec![PersistedValue::Int(6)],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: 42,
            sequence: 6,
            activation_epoch: 1,
            state,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::Custom {
            sequence: 6,
            replay_id: None,
            name: "new-flow".into(),
            args: vec![PersistedValue::Int(6)],
        }],
        domain_events: vec![EventEntry {
            sequence: 6,
            field_name: "count".into(),
            event_name: "Changed".into(),
            args: vec![],
            value: PersistedValue::Int(6),
        }],
        durable_effects: vec![],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: 43,
            ordinal: 0,
            behavior_id: 7,
            payload: vec![PersistedValue::Int(6)],
        }],
    };
    let receipt = store
        .commit_transition(transition)
        .expect("legacy transaction");
    assert_eq!(receipt.sequence, 6);
    assert_eq!(store.read_journal(42).len(), 2);
    assert_eq!(store.read_workflow_events(42).len(), 2);
    assert_eq!(store.read_events(42).len(), 2);
    assert_eq!(store.latest_sequence(42), 6);
    assert_eq!(
        store
            .load_durable_tail_position(42)
            .unwrap()
            .unwrap()
            .sequence,
        6
    );
    drop(store);
    eprintln!("legacy fixture written by libsql engine: {path}");
}
