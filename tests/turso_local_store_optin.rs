//! Explicit opt-in, disposable local SQLite backend parity.
//! This test does not use remote Turso or production data.
#![cfg(feature = "turso-local-probe")]

use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, JournalEntry, LibsqlStore,
    PersistedValue, PersistenceStore, TursoLocalProbeStore, DURABLE_TRANSITION_VERSION,
};
use std::collections::{BTreeSet, HashMap};

#[test]
fn turso_local_probe_commits_atomically_and_preserves_authority_for_legacy_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("disposable.db");
    assert!(
        TursoLocalProbeStore::open_existing(&path).is_err(),
        "must not initialize an unrecognized file"
    );
    let legacy = LibsqlStore::new(&path).unwrap();
    drop(legacy);

    let mut store = TursoLocalProbeStore::open_existing(&path).unwrap();
    let tokens = BTreeSet::from(["synthetic-test-only-authority".to_string()]);
    let transition = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 19,
        activation_epoch: 4,
        sequence: 1,
        expected_previous_sequence: 0,
        command: Some(JournalEntry {
            sequence: 1,
            behavior_id: 5,
            payload: vec![PersistedValue::Int(12)],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: 19,
            sequence: 1,
            activation_epoch: 4,
            state: HashMap::from([("count".to_string(), PersistedValue::Int(12))]),
            schema_name: Some("ExperimentalCounter".to_string()),
            authority_tokens: tokens.clone(),
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: 20,
            ordinal: 0,
            behavior_id: 7,
            payload: vec![PersistedValue::Int(12)],
        }],
    };
    let receipt = store.commit_transition(transition.clone()).unwrap();
    assert_eq!(receipt.digest, transition.digest().unwrap());
    assert_eq!(
        store.commit_transition(transition.clone()).unwrap().digest,
        receipt.digest
    );
    let mut conflict = transition.clone();
    conflict.command.as_mut().unwrap().payload = vec![PersistedValue::Int(99)];
    assert!(
        store.commit_transition(conflict).is_err(),
        "conflicting logical replay must fail"
    );
    let mut stale = transition.clone();
    stale.activation_epoch = 3;
    stale.sequence = 2;
    stale.expected_previous_sequence = 1;
    stale.command.as_mut().unwrap().sequence = 2;
    stale.snapshot.as_mut().unwrap().sequence = 2;
    stale.snapshot.as_mut().unwrap().activation_epoch = 3;
    assert!(
        store.commit_transition(stale).is_err(),
        "stale epoch must fail"
    );
    assert_eq!(
        store
            .load_durable_tail_position(19)
            .unwrap()
            .unwrap()
            .sequence,
        1
    );
    assert_eq!(store.read_journal(19).len(), 1);
    drop(store);

    let reopened = LibsqlStore::new(&path).unwrap();
    let snapshot = reopened.load_snapshot(19).unwrap();
    assert_eq!(snapshot.schema_name.as_deref(), Some("ExperimentalCounter"));
    assert_eq!(snapshot.authority_tokens, tokens);
    assert_eq!(reopened.read_journal(19).len(), 1);
    assert_eq!(
        reopened
            .query("SELECT COUNT(*) FROM durable_outbox WHERE actor_id=19", &[])
            .unwrap(),
        vec!["[1]"]
    );
    assert_eq!(
        reopened
            .query(
                "SELECT COUNT(*) FROM durable_transitions WHERE actor_id=19",
                &[]
            )
            .unwrap(),
        vec!["[1]"]
    );
    assert_eq!(
        reopened
            .load_durable_tail_position(19)
            .unwrap()
            .unwrap()
            .sequence,
        1
    );
}

#[test]
fn turso_probe_does_not_admit_memory_or_malformed_files() {
    let temp = tempfile::tempdir().unwrap();
    let garbage = temp.path().join("not-sqlite.db");
    std::fs::write(&garbage, b"invalid database data").unwrap();
    assert!(TursoLocalProbeStore::open_existing(":memory:").is_err());
    assert!(TursoLocalProbeStore::open_existing(&garbage).is_err());
    assert_eq!(std::fs::read(&garbage).unwrap(), b"invalid database data");
    #[cfg(unix)]
    {
        let link = temp.path().join("database-symlink.db");
        std::os::unix::fs::symlink(&garbage, &link).unwrap();
        assert!(TursoLocalProbeStore::open_existing(&link).is_err());
    }
}
