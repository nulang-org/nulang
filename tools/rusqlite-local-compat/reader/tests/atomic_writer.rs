//! Contract-first tests: these must run in a different OS process from
//! the legacy libSQL fixture writer to avoid native SQLite runtime conflicts.
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, EventEntry, JournalEntry,
    PersistedValue, WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use nulang_rusqlite_recovery_reader::{RusqliteAtomicWriter, RusqliteRecoveryReader};
use rusqlite::Connection;
use std::{
    fs,
    io::ErrorKind,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn copied_fixture(label: &str) -> (PathBuf, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-rusqlite-atomic-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    let db = dir.join("durable.db");
    fs::copy(std::env::var_os("NULANG_LEGACY_FIXTURE_DB").unwrap(), &db).unwrap();
    (dir, db)
}

fn transition(
    actor: u64,
    epoch: u64,
    sequence: u64,
    predecessor: u64,
    tag: &str,
) -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: actor,
        activation_epoch: epoch,
        sequence,
        expected_previous_sequence: predecessor,
        command: Some(JournalEntry {
            sequence,
            behavior_id: 7,
            payload: vec![PersistedValue::String(tag.to_string())],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: actor,
            sequence,
            activation_epoch: epoch,
            state: [("tag".into(), PersistedValue::String(tag.to_string()))].into(),
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::Custom {
            sequence,
            replay_id: None,
            name: tag.to_string(),
            args: vec![],
        }],
        domain_events: vec![EventEntry {
            sequence,
            field_name: "tag".to_string(),
            event_name: "Changed".to_string(),
            args: vec![],
            value: PersistedValue::String(tag.to_string()),
        }],
        durable_effects: vec![],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: 89,
            ordinal: 0,
            behavior_id: 9,
            payload: vec![PersistedValue::String(tag.to_string())],
        }],
    }
}

fn count(conn: &Connection, table: &str, sequence: u64) -> i64 {
    const TABLES: &[&str] = &[
        "snapshots",
        "journal",
        "events",
        "durable_tails",
        "durable_transitions",
        "durable_workflow_events",
        "durable_domain_events",
        "durable_effect_records",
        "durable_outbox",
    ];
    assert!(TABLES.contains(&table));
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE actor_id=42 AND sequence=?1"),
        [sequence as i64],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn commit_recovery_and_all_durable_rows_match_existing_file_schema() {
    let (dir, db) = copied_fixture("commit");
    let t = transition(42, 1, 3, 2, "v3");
    let expected_digest = t.digest().unwrap();
    let mut writer = RusqliteAtomicWriter::open_existing(&db).unwrap();
    let commit = writer.commit(&t).unwrap();
    assert_eq!(commit.digest, expected_digest);
    drop(writer);
    let reader = RusqliteRecoveryReader::open(&db).unwrap();
    assert_eq!(reader.tail(42).unwrap().unwrap().sequence, 3);
    assert_eq!(
        reader.snapshot(42).unwrap().unwrap().state["tag"],
        PersistedValue::String("v3".into())
    );
    assert_eq!(reader.journal(42).unwrap().len(), 3);
    assert_eq!(reader.workflow_events(42).unwrap().len(), 3);
    drop(reader);
    let conn = Connection::open(&db).unwrap();
    assert_eq!(count(&conn, "snapshots", 3), 1);
    for table in [
        "journal",
        "durable_transitions",
        "durable_tails",
        "durable_workflow_events",
        "durable_domain_events",
        "durable_outbox",
    ] {
        assert_eq!(count(&conn, table, 3), 1, "missing atomic row in {table}");
    }
    assert_eq!(count(&conn, "durable_effect_records", 3), 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn exact_digest_replay_is_idempotent_but_conflict_stale_epoch_and_gap_are_rejected() {
    let (dir, db) = copied_fixture("fencing");
    let mut writer = RusqliteAtomicWriter::open_existing(&db).unwrap();
    let t = transition(42, 1, 3, 2, "first");
    let initial = writer.commit(&t).unwrap();
    assert_eq!(writer.commit(&t).unwrap().digest, initial.digest);
    assert_eq!(
        writer
            .commit(&transition(42, 1, 3, 2, "conflict"))
            .unwrap_err()
            .kind(),
        ErrorKind::AlreadyExists
    );
    assert_eq!(
        writer
            .commit(&transition(42, 0, 4, 3, "invalid-epoch"))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        writer
            .commit(&transition(42, 1, 5, 4, "gap"))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
    writer
        .commit(&transition(42, 2, 4, 3, "new-epoch"))
        .unwrap();
    assert_eq!(
        writer
            .commit(&transition(42, 1, 5, 4, "stale"))
            .unwrap_err()
            .kind(),
        ErrorKind::PermissionDenied
    );
    drop(writer);
    let c = Connection::open(&db).unwrap();
    assert_eq!(count(&c, "durable_transitions", 3), 1);
    assert_eq!(count(&c, "durable_transitions", 4), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn failure_in_late_outbox_stage_rolls_back_all_prior_rows() {
    let (dir, db) = copied_fixture("rollback");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_outbox BEFORE INSERT ON durable_outbox
         BEGIN SELECT RAISE(ABORT,'injected late outbox failure'); END",
    )
    .unwrap();
    drop(conn);
    let mut writer = RusqliteAtomicWriter::open_existing(&db).unwrap();
    assert!(writer
        .commit(&transition(42, 1, 3, 2, "should-rollback"))
        .is_err());
    drop(writer);
    let c = Connection::open(&db).unwrap();
    for table in [
        "snapshots",
        "journal",
        "durable_transitions",
        "durable_tails",
        "durable_workflow_events",
        "durable_domain_events",
        "durable_effect_records",
        "durable_outbox",
    ] {
        assert_eq!(count(&c, table, 3), 0, "partial commit in {table}");
    }
    assert_eq!(
        c.query_row(
            "SELECT sequence FROM durable_tails WHERE actor_id=42",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn independent_processes_contend_and_only_one_digest_commits() {
    let (dir, db) = copied_fixture("races");
    let binary = env!("CARGO_BIN_EXE_rusqlite_atomic_fixture");
    let children = (0..8)
        .map(|i| {
            Command::new(binary)
                .arg(&db)
                .arg("42")
                .arg("1")
                .arg("3")
                .arg("2")
                .arg(format!("contender-{i}"))
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let results = children
        .into_iter()
        .map(|mut p| p.wait().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results.iter().filter(|s| s.success()).count(),
        1,
        "exactly one distinct digest must win"
    );
    let c = Connection::open(&db).unwrap();
    assert_eq!(count(&c, "durable_transitions", 3), 1);
    assert_eq!(count(&c, "durable_outbox", 3), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn corrupted_committed_tail_digest_blocks_all_further_writes() {
    let (dir, db) = copied_fixture("corrupt-tail");
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE durable_tails SET digest='forged' WHERE actor_id=42",
        [],
    )
    .unwrap();
    drop(conn);
    let mut writer = RusqliteAtomicWriter::open_existing(&db).unwrap();
    let result = writer.commit(&transition(42, 1, 3, 2, "must-not-commit"));
    assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData);
    drop(writer);
    let conn = Connection::open(&db).unwrap();
    assert_eq!(count(&conn, "durable_transitions", 3), 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn identical_replay_must_verify_transition_history_not_only_tail_digest() {
    let (dir, db) = copied_fixture("corrupt-history");
    let original = transition(42, 1, 3, 2, "accepted");
    let mut writer = RusqliteAtomicWriter::open_existing(&db).unwrap();
    writer.commit(&original).unwrap();
    drop(writer);
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE durable_transitions SET digest='forged' WHERE actor_id=42 AND sequence=3",
        [],
    )
    .unwrap();
    drop(conn);
    let mut writer = RusqliteAtomicWriter::open_existing(&db).unwrap();
    assert_eq!(
        writer.commit(&original).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn separate_actors_can_commit_in_independent_processes() {
    let (dir, db) = copied_fixture("independent-actors");
    let binary = env!("CARGO_BIN_EXE_rusqlite_atomic_fixture");
    let mut a = Command::new(binary)
        .arg(&db)
        .args(["90", "1", "1", "0", "actor90"])
        .spawn()
        .unwrap();
    let mut b = Command::new(binary)
        .arg(&db)
        .args(["91", "1", "1", "0", "actor91"])
        .spawn()
        .unwrap();
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    let c = Connection::open(&db).unwrap();
    let n: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM durable_tails WHERE actor_id IN (90,91) AND sequence=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn killed_uncommitted_process_leaves_no_durable_rows_and_allows_retry() {
    use std::io::BufRead;
    use std::process::Stdio;

    let (dir, db) = copied_fixture("sigkill");
    let binary = env!("CARGO_BIN_EXE_rusqlite_atomic_fixture");
    let mut child = Command::new(binary)
        .arg("--precommit-block")
        .arg(&db)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(
        ready.trim(),
        "READY_UNCOMMITTED",
        "child must stage the write transaction"
    );
    child.kill().unwrap(); // SIGKILL on Linux, no SQLite destructor runs
    assert!(!child.wait().unwrap().success());
    let conn = Connection::open(&db).unwrap();
    for table in [
        "snapshots",
        "journal",
        "durable_transitions",
        "durable_tails",
        "durable_workflow_events",
        "durable_domain_events",
        "durable_effect_records",
        "durable_outbox",
    ] {
        assert_eq!(count(&conn, table, 3), 0, "crash leaked a row in {table}");
    }
    drop(conn);
    let mut writer = RusqliteAtomicWriter::open_existing(&db).unwrap();
    writer
        .commit(&transition(42, 1, 3, 2, "recovered"))
        .unwrap();
    drop(writer);
    let reader = RusqliteRecoveryReader::open(&db).unwrap();
    assert_eq!(reader.tail(42).unwrap().unwrap().sequence, 3);
    fs::remove_dir_all(dir).unwrap();
}
