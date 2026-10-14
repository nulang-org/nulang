#![cfg(feature = "sqlite")]
//! Durable inbox receipts must commit atomically with actor command history.
//! Tests exercise the actual file-backed libSQL store, including reopen.

use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::durable_inbox::DurableInboxCommand;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, EventEntry, JournalEntry, LibsqlStore,
    PersistedValue, PersistenceStore, WorkflowEvent, DURABLE_TRANSITION_VERSION,
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
            let store = LibsqlStore::new(path_a);
            barrier_a.wait();
            let mut store = store.expect("first libSQL connection");
            store.commit_inbox_transition(identity(51, "first", 7), command(51, 0, 7))
        });
        let right = scope.spawn(move || {
            let store = LibsqlStore::new(path_b);
            barrier_b.wait();
            let mut store = store.expect("second libSQL connection");
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

#[test]
fn concurrent_retries_of_same_identity_share_one_persisted_receipt() {
    let file = TestDatabase::new();
    // Establish the schema before racing independent connections; this test
    // exercises receipt contention, not concurrent schema initialization.
    drop(file.store());
    let path_a = file.0.clone();
    let path_b = file.0.clone();
    let barrier = Arc::new(Barrier::new(2));
    let (left, right) = std::thread::scope(|scope| {
        let barrier_a = Arc::clone(&barrier);
        let barrier_b = Arc::clone(&barrier);
        let left = scope.spawn(move || {
            let store = LibsqlStore::new(path_a);
            barrier_a.wait();
            let mut store = store.expect("first libSQL connection");
            store.commit_inbox_transition(identity(51, "same-message", 7), command(51, 0, 7))
        });
        let right = scope.spawn(move || {
            let store = LibsqlStore::new(path_b);
            barrier_b.wait();
            let mut store = store.expect("second libSQL connection");
            store.commit_inbox_transition(identity(51, "same-message", 7), command(51, 0, 7))
        });
        (left.join().unwrap(), right.join().unwrap())
    });

    // A contender can encounter SQLITE_BUSY instead of waiting for the
    // winner. Either outcome must recover the same committed receipt.
    assert!(
        left.is_ok() || right.is_ok(),
        "at least one concurrent admission must succeed: left={left:?}, right={right:?}"
    );
    let first = match (&left, &right) {
        (Ok(first), Ok(replay)) => {
            assert_eq!(first, replay, "duplicate admission returned a different commit");
            *first
        }
        (Ok(first), Err(_)) | (Err(_), Ok(first)) => *first,
        (Err(_), Err(_)) => unreachable!("checked that at least one commit succeeded"),
    };

    let request = identity(51, "same-message", 7);
    let mut reopened = file.store();
    let receipt = reopened
        .load_durable_inbox_receipt(request.id())
        .unwrap()
        .expect("one durable receipt must survive both contenders");
    assert_eq!(receipt.commit, first);
    assert_eq!(receipt.request_digest, request.request_digest());
    assert_eq!(reopened.read_journal(51).len(), 1);
    assert_eq!(
        reopened.load_durable_tail_position(51).unwrap().unwrap().sequence,
        1
    );
    assert_eq!(
        reopened.commit_inbox_transition(request, command(51, 0, 7)).unwrap(),
        first,
        "a retry after contention must return the original acknowledgment"
    );
    assert_eq!(reopened.read_journal(51).len(), 1);
}

#[test]
fn receipt_insert_failure_rolls_back_previously_written_transition_records() {
    let file = TestDatabase::new();
    let mut store = file.store();
    // Force a failure at the last write, after snapshot, journal, events,
    // outbox and tail have been staged, rather than at admission validation.
    let path = file.0.to_string_lossy().into_owned();
    let rt = tokio::runtime::Runtime::new().expect("create trigger runtime");
    rt.block_on(async {
        let db = libsql::Builder::new_local(path)
            .build()
            .await
            .expect("open the same file-backed database");
        let conn = db.connect().expect("connect to failure-injection database");
        conn.execute(
            "CREATE TRIGGER reject_inbox_receipt BEFORE INSERT ON durable_inbox_receipts
             BEGIN SELECT RAISE(ABORT, 'injected durable inbox receipt write failure'); END",
            (),
        )
        .await
        .expect("install receipt insert failure");
    });
    drop(rt);

    let request = identity(51, "failure-at-receipt", 7);
    let mut transition = command(51, 0, 7);
    transition.snapshot = Some(ActorSnapshot {
        actor_id: 51,
        activation_epoch: 3,
        sequence: 1,
        ..ActorSnapshot::default()
    });
    transition.workflow_events.push(WorkflowEvent::WorkflowStarted {
        sequence: 1,
        name: "rollback-test".to_string(),
        state: vec![],
    });
    transition.domain_events.push(EventEntry {
        sequence: 1,
        field_name: "counter".to_string(),
        event_name: "Changed".to_string(),
        args: vec![],
        value: PersistedValue::Int(7),
    });
    transition.outbox.push(DurableOutboxMessage {
        destination_actor_id: 52,
        ordinal: 0,
        behavior_id: 12,
        payload: vec![PersistedValue::Int(9)],
    });
    let effect_spec = DurableEffectSpec::new(
        DurableEffectId::derive(51, "failure-at-receipt", 0, "Provider.ask"),
        "Provider.ask",
        EffectBoundary::External,
        DeliverySemantics::AtLeastOnce,
    );
    transition.durable_effects.push(DurableEffectPersistenceRecord::from_effect(
        DurableEffectRecord::prepare(effect_spec, b"request"),
    ));

    assert!(
        store.commit_inbox_transition(request.clone(), transition).is_err(),
        "the injected receipt failure must abort the transaction"
    );
    for table in [
        "snapshots",
        "journal",
        "durable_workflow_events",
        "durable_domain_events",
        "durable_effect_records",
        "durable_outbox",
        "durable_transitions",
        "durable_tails",
        "durable_inbox_receipts",
    ] {
        let rows = store
            .query(&format!("SELECT actor_id FROM {table} WHERE actor_id = 51"), &[])
            .unwrap();
        assert!(rows.is_empty(), "{table} must roll back with the receipt");
    }
    assert!(store.read_journal(51).is_empty());
    assert!(store.load_durable_tail_position(51).unwrap().is_none());
    assert!(store.load_durable_inbox_receipt(request.id()).unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn process_kill_after_commit_before_ack_replays_durable_receipt() {
    use std::os::unix::process::ExitStatusExt;

    const CHILD_DB_ENV: &str = "NULANG_TEST_INBOX_KILL_AFTER_COMMIT_DB";
    if let Some(db_path) = std::env::var_os(CHILD_DB_ENV) {
        // The child commits but never reports an acknowledgment to the
        // parent; SIGKILL bypasses normal Rust destructors and DB close.
        let mut store = LibsqlStore::new(PathBuf::from(db_path))
            .expect("open file-backed libSQL in subprocess");
        store
            .commit_inbox_transition(
                identity(51, "killed-after-commit", 7),
                command(51, 0, 7),
            )
            .expect("durable command and receipt committed before process kill");
        unsafe {
            libc::raise(libc::SIGKILL);
        }
        panic!("the subprocess should have been killed with SIGKILL");
    }

    let file = TestDatabase::new();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("process_kill_after_commit_before_ack_replays_durable_receipt")
        .arg("--nocapture")
        .env(CHILD_DB_ENV, &file.0)
        .output()
        .expect("spawn the integration-test subprocess");
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGKILL),
        "child did not reach the intentional kill window: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let request = identity(51, "killed-after-commit", 7);
    let mut reopened = file.store();
    let committed = reopened
        .load_durable_inbox_receipt(request.id())
        .unwrap()
        .expect("receipt must survive process death after transaction commit");
    assert_eq!(committed.request_digest, request.request_digest());
    assert_eq!(committed.commit.sequence, 1);
    assert_eq!(
        reopened
            .commit_inbox_transition(request, command(51, 0, 7))
            .expect("retry after lost acknowledgment"),
        committed.commit
    );
    assert_eq!(reopened.read_journal(51).len(), 1);
    assert_eq!(
        reopened.load_durable_tail_position(51).unwrap().unwrap().sequence,
        1
    );
}
