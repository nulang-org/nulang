#![cfg(feature = "sqlite")]
//! Durable inbox receipts must commit atomically with actor command history.
//! Tests exercise the actual file-backed libSQL store, including reopen.

use nulang::durable_inbox::DurableInboxCommand;
use nulang::runtime::{
    DurableTransition, JournalEntry, LibsqlStore, PersistedValue, PersistenceStore,
    DURABLE_TRANSITION_VERSION,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

static NEXT_FILE: AtomicU64 = AtomicU64::new(1);

struct TestDatabase(PathBuf);

impl TestDatabase {
    fn new() -> Self {
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "nulang-durable-inbox-{}-{}-{id}.db",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        Self(path)
    }

    fn store(&self) -> LibsqlStore {
        LibsqlStore::new(&self.0).expect("open file-backed libSQL")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for extension in ["", "-wal", "-shm"] {
            let suffix = format!("{}{}", self.0.display(), extension);
            let _ = std::fs::remove_file(suffix);
        }
    }
}

fn command(actor: u64, predecessor: u64, value: i64) -> DurableTransition {
    let sequence = predecessor + 1;
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: actor,
        activation_epoch: 3,
        sequence,
        expected_previous_sequence: predecessor,
        command: Some(JournalEntry {
            sequence,
            behavior_id: 11,
            payload: vec![PersistedValue::Int(value)],
        }),
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

fn identity(actor: u64, key: &str, value: i64) -> DurableInboxCommand {
    DurableInboxCommand::new(
        "tenant-a",
        "verified-sender",
        key,
        actor,
        11,
        &[PersistedValue::Int(value)],
    )
    .unwrap()
}

#[test]
fn receipt_and_command_survive_connection_close_and_reopen() {
    let file = TestDatabase::new();
    let request = identity(51, "message-1", 7);
    let first;
    {
        let mut store = file.store();
        first = store
            .commit_inbox_transition(request.clone(), command(51, 0, 7))
            .expect("atomic inbox admission");
        assert_eq!(store.read_journal(51).len(), 1);
        assert_eq!(
            store.load_durable_inbox_receipt(request.id()).unwrap().unwrap().commit,
            first
        );
    }
    {
        let mut reopened = file.store();
        let receipt = reopened
            .load_durable_inbox_receipt(request.id())
            .unwrap()
            .expect("receipt must survive reopen");
        assert_eq!(receipt.commit, first);
        assert_eq!(receipt.request_digest, request.request_digest());
        assert_eq!(
            reopened
                .commit_inbox_transition(request, command(51, 0, 7))
                .expect("ambiguous acknowledgment replay"),
            first
        );
        assert_eq!(reopened.read_journal(51).len(), 1);
    }
}

#[test]
fn old_acknowledgment_replays_after_subsequent_durable_turns() {
    let file = TestDatabase::new();
    let mut store = file.store();
    let original = identity(51, "original", 7);
    let first = store
        .commit_inbox_transition(original.clone(), command(51, 0, 7))
        .unwrap();
    store
        .commit_inbox_transition(identity(51, "next", 8), command(51, 1, 8))
        .unwrap();
    drop(store);

    let mut reopened = file.store();
    assert_eq!(
        reopened
            .commit_inbox_transition(original, command(51, 0, 7))
            .unwrap(),
        first
    );
    assert_eq!(reopened.read_journal(51).len(), 2);
    assert_eq!(
        reopened.load_durable_tail_position(51).unwrap().unwrap().sequence,
        2
    );
}

#[test]
fn duplicate_key_with_different_payload_fails_without_advancing_tail() {
    let file = TestDatabase::new();
    let mut store = file.store();
    store
        .commit_inbox_transition(identity(51, "same", 7), command(51, 0, 7))
        .unwrap();
    let error = store
        .commit_inbox_transition(identity(51, "same", 8), command(51, 1, 8))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(store.read_journal(51).len(), 1);
    assert_eq!(store.load_durable_tail_position(51).unwrap().unwrap().sequence, 1);
}

#[test]
fn invalid_transition_leaves_no_dedup_receipt() {
    let file = TestDatabase::new();
    let mut store = file.store();
    let request = identity(51, "invalid", 7);
    let mut invalid = command(51, 0, 7);
    invalid.command.as_mut().unwrap().behavior_id = 999;
    assert_eq!(
        store
            .commit_inbox_transition(request.clone(), invalid)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert!(store.load_durable_inbox_receipt(request.id()).unwrap().is_none());
    assert!(store.read_journal(51).is_empty());
}

#[test]
fn plain_transition_cannot_later_mint_an_inbox_receipt() {
    let file = TestDatabase::new();
    let mut store = file.store();
    store.commit_transition(command(51, 0, 7)).unwrap();
    let request = identity(51, "plain", 7);
    assert_eq!(
        store
            .commit_inbox_transition(request.clone(), command(51, 0, 7))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert!(store.load_durable_inbox_receipt(request.id()).unwrap().is_none());
    assert_eq!(store.read_journal(51).len(), 1);
}

#[test]
fn mismatched_predecessor_rolls_back_receipt_and_journal() {
    let file = TestDatabase::new();
    let mut store = file.store();
    let request = identity(51, "wrong-tail", 7);
    assert!(store
        .commit_inbox_transition(request.clone(), command(51, 6, 7))
        .is_err());
    assert!(store.load_durable_inbox_receipt(request.id()).unwrap().is_none());
    assert!(store.read_journal(51).is_empty());
}

#[test]
fn two_connections_competing_for_same_sequence_never_both_commit() {
    let file = TestDatabase::new();
    let path_a = file.0.clone();
    let path_b = file.0.clone();
    let barrier = Arc::new(Barrier::new(2));
    let (left, right) = std::thread::scope(|scope| {
        let barrier_a = Arc::clone(&barrier);
        let barrier_b = Arc::clone(&barrier);
        let left = scope.spawn(move || {
            let mut store = LibsqlStore::new(path_a).unwrap();
            barrier_a.wait();
            store.commit_inbox_transition(identity(51, "first", 7), command(51, 0, 7))
        });
        let right = scope.spawn(move || {
            let mut store = LibsqlStore::new(path_b).unwrap();
            barrier_b.wait();
            store.commit_inbox_transition(identity(51, "second", 8), command(51, 0, 8))
        });
        (left.join().unwrap(), right.join().unwrap())
    });
    assert_eq!((left.is_ok() as usize) + (right.is_ok() as usize), 1);

    let store = file.store();
    assert_eq!(store.read_journal(51).len(), 1);
    assert_eq!(
        (store
            .load_durable_inbox_receipt(identity(51, "first", 7).id())
            .unwrap()
            .is_some() as usize)
            + (store
                .load_durable_inbox_receipt(identity(51, "second", 8).id())
                .unwrap()
                .is_some() as usize),
        1
    );
}
