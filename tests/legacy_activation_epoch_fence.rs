//! Migration boundary: an existing legacy snapshot is an ownership-fencing
//! authority until the first atomic transition establishes a durable tail.
//! Both persistence backends must reject stale activations without mutation.

use nulang::runtime::{
    ActorSnapshot, DurableTailPosition, DurableTransition, MemoryStore, PersistenceStore,
    DURABLE_TRANSITION_VERSION,
};
use std::io;

#[cfg(feature = "sqlite")]
use nulang::runtime::LibsqlStore;

fn transition(actor_id: u64, activation_epoch: u64, sequence: u64) -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch,
        sequence,
        expected_previous_sequence: sequence - 1,
        command: None,
        snapshot: Some(ActorSnapshot {
            actor_id,
            sequence,
            activation_epoch,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

fn assert_legacy_epoch_fences_first_atomic_commit(store: &mut dyn PersistenceStore, actor_id: u64) {
    store
        .save_snapshot(ActorSnapshot {
            actor_id,
            sequence: 5,
            activation_epoch: 9,
            ..ActorSnapshot::default()
        })
        .expect("legacy snapshot should persist");

    let stale = store
        .commit_transition(transition(actor_id, 8, 6))
        .expect_err("an older owner must not promote a legacy history");
    assert_eq!(stale.kind(), io::ErrorKind::PermissionDenied);

    let untouched = store
        .load_snapshot(actor_id)
        .expect("legacy snapshot remains");
    assert_eq!(untouched.sequence, 5, "rejection must not advance state");
    assert_eq!(
        untouched.activation_epoch, 9,
        "rejection must not replace the owner"
    );
    assert_eq!(
        store.load_durable_tail_position(actor_id).unwrap(),
        None,
        "rejection must not establish an atomic tail"
    );

    let committed = store
        .commit_transition(transition(actor_id, 9, 6))
        .expect("current owner may extend the legacy tail");
    assert_eq!(committed.activation_epoch, 9);
    assert_eq!(committed.sequence, 6);
    assert_eq!(
        store.load_durable_tail_position(actor_id).unwrap(),
        Some(DurableTailPosition {
            activation_epoch: 9,
            sequence: 6,
        })
    );
}

#[test]
fn memory_store_legacy_snapshot_epoch_fences_first_atomic_commit() {
    let mut store = MemoryStore::new();
    assert_legacy_epoch_fences_first_atomic_commit(&mut store, 742_001);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_legacy_snapshot_epoch_fences_first_atomic_commit() {
    let mut store = LibsqlStore::in_memory().expect("in-memory libsql");
    assert_legacy_epoch_fences_first_atomic_commit(&mut store, 742_002);
}

fn assert_zero_epoch_legacy_snapshot_cannot_be_promoted(
    store: &mut dyn PersistenceStore,
    actor_id: u64,
) {
    store
        .save_snapshot(ActorSnapshot {
            actor_id,
            sequence: 3,
            activation_epoch: 0,
            ..ActorSnapshot::default()
        })
        .expect("store malformed legacy fixture");

    let error = store
        .commit_transition(transition(actor_id, 1, 4))
        .expect_err("legacy snapshot with invalid epoch zero must fail closed");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    let snapshot = store.load_snapshot(actor_id).expect("snapshot remains");
    assert_eq!(snapshot.sequence, 3);
    assert_eq!(snapshot.activation_epoch, 0);
    assert_eq!(store.load_durable_tail_position(actor_id).unwrap(), None);
}

#[test]
fn memory_store_rejects_invalid_zero_epoch_legacy_promotion() {
    let mut store = MemoryStore::new();
    assert_zero_epoch_legacy_snapshot_cannot_be_promoted(&mut store, 742_003);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_rejects_invalid_zero_epoch_legacy_promotion() {
    let mut store = LibsqlStore::in_memory().expect("in-memory libsql");
    assert_zero_epoch_legacy_snapshot_cannot_be_promoted(&mut store, 742_004);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_reopen_preserves_legacy_owner_fence_before_atomic_promotion() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after Unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-legacy-epoch-fence-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create isolated test directory");
    let db_path = dir.join("durable.db");
    let actor_id = 742_005;

    {
        let mut store = LibsqlStore::new(&db_path).expect("open file-backed libsql");
        store
            .save_snapshot(ActorSnapshot {
                actor_id,
                sequence: 5,
                activation_epoch: 9,
                ..ActorSnapshot::default()
            })
            .expect("persist legacy owner");
    }

    // A new store handle must still observe the old ownership authority;
    // stale promotion cannot be used to replace it after a restart.
    {
        let mut store = LibsqlStore::new(&db_path).expect("reopen legacy history");
        let error = store
            .commit_transition(transition(actor_id, 8, 6))
            .expect_err("stale owner must not establish the first atomic tail");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    // Reopening after the failed transaction must reveal no partial writes.
    {
        let mut store = LibsqlStore::new(&db_path).expect("reopen rejected history");
        let snapshot = store.load_snapshot(actor_id).expect("legacy snapshot survives");
        assert_eq!(snapshot.sequence, 5);
        assert_eq!(snapshot.activation_epoch, 9);
        assert_eq!(store.load_durable_tail_position(actor_id).unwrap(), None);

        // A strictly newer activation can legitimately take ownership.
        let committed = store
            .commit_transition(transition(actor_id, 10, 6))
            .expect("new owner can atomically promote legacy history");
        assert_eq!(committed.activation_epoch, 10);
        assert_eq!(committed.sequence, 6);
    }

    {
        let store = LibsqlStore::new(&db_path).expect("reopen committed atomic tail");
        assert_eq!(
            store.load_durable_tail_position(actor_id).unwrap(),
            Some(DurableTailPosition {
                activation_epoch: 10,
                sequence: 6,
            })
        );
        let snapshot = store.load_snapshot(actor_id).expect("promoted snapshot survives");
        assert_eq!(snapshot.sequence, 6);
        assert_eq!(snapshot.activation_epoch, 10);
    }

    std::fs::remove_dir_all(dir).expect("clean isolated test directory");
}
