#![cfg(feature = "sqlite")]

//! Cross-engine on-disk upgrade check: fixture produced by unchanged main's
//! libsql 0.10.0-pre.4 implementation; reopened by the patched SQLite provider.
//! The fixture contains legacy journal/workflow/event rows and an atomically
//! committed transition, not merely a fresh database created by this code.

use flate2::read::GzDecoder;
use std::io::Read;

use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTailPosition, DurableTransition, EventEntry,
    JournalEntry, LibsqlStore, PersistedValue, PersistenceStore, WorkflowEvent,
    DURABLE_TRANSITION_VERSION,
};

#[test]
fn legacy_libsql_database_can_recover_and_commit_with_patched_sqlite() {
    let compressed = include_bytes!("fixtures/legacy_libsql_actor_main_448cfa8c2.db.gz");
    let mut bytes = Vec::new();
    GzDecoder::new(compressed.as_slice())
        .read_to_end(&mut bytes)
        .expect("decode historical libsql database fixture");
    assert!(bytes.starts_with(b"SQLite format 3\0"));
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "nulang_legacy_libsql_{}_{}.db",
        std::process::id(),
        unique
    ));
    std::fs::write(&path, bytes).unwrap();

    {
        let mut store = LibsqlStore::new(&path).expect("new provider opens old libsql file");
        let original = store.load_snapshot(42).expect("original actor snapshot");
        assert_eq!(original.sequence, 6);
        assert_eq!(original.activation_epoch, 1);
        assert_eq!(original.state.get("count"), Some(&PersistedValue::Int(6)));
        assert_eq!(
            original.state.get("label"),
            Some(&PersistedValue::String("legacy-actor".to_string()))
        );
        assert_eq!(store.latest_sequence(42), 6);
        assert_eq!(store.read_journal(42).len(), 2);
        assert_eq!(store.read_workflow_events(42).len(), 2);
        assert_eq!(store.read_events(42).len(), 2);
        assert_eq!(
            store.load_durable_tail_position(42).unwrap(),
            Some(DurableTailPosition {
                activation_epoch: 1,
                sequence: 6
            })
        );
        assert_eq!(
            store
                .query(
                    "SELECT COUNT(*) FROM durable_outbox WHERE actor_id = 42",
                    &[]
                )
                .unwrap(),
            vec!["[1]".to_string()]
        );

        let mut state = original.state;
        state.insert("count".to_string(), PersistedValue::Int(7));
        let next = DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: 42,
            activation_epoch: 1,
            sequence: 7,
            expected_previous_sequence: 6,
            command: Some(JournalEntry {
                sequence: 7,
                behavior_id: 4,
                payload: vec![PersistedValue::Int(7)],
            }),
            snapshot: Some(ActorSnapshot {
                actor_id: 42,
                sequence: 7,
                activation_epoch: 1,
                state,
                ..ActorSnapshot::default()
            }),
            workflow_events: vec![WorkflowEvent::Custom {
                sequence: 7,
                replay_id: None,
                name: "upgraded-flow".into(),
                args: vec![PersistedValue::Int(7)],
            }],
            domain_events: vec![EventEntry {
                sequence: 7,
                field_name: "count".into(),
                event_name: "Changed".into(),
                args: vec![],
                value: PersistedValue::Int(7),
            }],
            durable_effects: vec![],
            outbox: vec![DurableOutboxMessage {
                destination_actor_id: 44,
                ordinal: 0,
                behavior_id: 7,
                payload: vec![PersistedValue::Int(7)],
            }],
        };
        let receipt = store
            .commit_transition(next.clone())
            .expect("commit after upgrade");
        assert_eq!(receipt.sequence, 7);
        assert_eq!(store.commit_transition(next).unwrap(), receipt);
    }

    {
        let store = LibsqlStore::new(&path).expect("upgraded database reopens");
        assert_eq!(store.latest_sequence(42), 7);
        assert_eq!(store.read_journal(42).len(), 3);
        assert_eq!(store.read_workflow_events(42).len(), 3);
        assert_eq!(store.read_events(42).len(), 3);
        assert_eq!(
            store.load_durable_tail_position(42).unwrap(),
            Some(DurableTailPosition {
                activation_epoch: 1,
                sequence: 7
            })
        );
        assert_eq!(
            store.load_snapshot(42).unwrap().state.get("count"),
            Some(&PersistedValue::Int(7))
        );
    }

    std::fs::remove_file(&path).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}
