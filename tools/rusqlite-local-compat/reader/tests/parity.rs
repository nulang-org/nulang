use nulang::runtime::PersistedValue;
use nulang_rusqlite_recovery_reader::RusqliteRecoveryReader;
use std::{
    fs,
    io::ErrorKind,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn fixture() -> PathBuf {
    PathBuf::from(
        std::env::var_os("NULANG_LEGACY_FIXTURE_DB").expect(
            "run the exact-head two-process verifier to generate a genuine libSQL database",
        ),
    )
}
fn copy_fixture(label: &str) -> (PathBuf, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-rusqlite-parity-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("fixture.db");
    fs::copy(fixture(), &path).unwrap();
    (dir, path)
}

#[test]
fn reads_actual_libsql_file_snapshot_journal_workflows_and_tail_without_modification() {
    let f = fixture();
    let original = fs::read(&f).unwrap();
    let reader = RusqliteRecoveryReader::open(&f).unwrap();
    let snapshot = reader.snapshot(42).unwrap().unwrap();
    assert_eq!(snapshot.sequence, 2);
    assert_eq!(snapshot.activation_epoch, 1);
    assert_eq!(snapshot.state["count"], PersistedValue::Int(2));
    let tail = reader.tail(42).unwrap().unwrap();
    assert_eq!(tail.sequence, 2);
    assert_eq!(tail.activation_epoch, 1);
    let entries = reader.journal(42).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].behavior_id, 3);
    assert_eq!(reader.workflow_events(42).unwrap().len(), 2);
    assert_eq!(fs::read(&f).unwrap(), original);
}

#[test]
fn missing_and_empty_database_files_are_rejected() {
    let (dir, path) = copy_fixture("missing");
    fs::remove_file(&path).unwrap();
    assert_eq!(
        RusqliteRecoveryReader::open(&path).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    fs::write(&path, []).unwrap();
    assert!(RusqliteRecoveryReader::open(&path).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn malformed_snapshot_json_causes_hard_error_not_empty_recovery() {
    let (dir, path) = copy_fixture("corrupt");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE snapshots SET state=?1 WHERE actor_id=42",
        ["{bad-json"],
    )
    .unwrap();
    drop(conn);
    let reader = RusqliteRecoveryReader::open(&path).unwrap();
    assert_eq!(
        reader.snapshot(42).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn malicious_or_malformed_tail_digest_is_rejected() {
    let (dir, path) = copy_fixture("tail");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE durable_tails SET digest=?1 WHERE actor_id=42",
        ["not-a-valid-digest"],
    )
    .unwrap();
    drop(conn);
    let reader = RusqliteRecoveryReader::open(&path).unwrap();
    assert_eq!(reader.tail(42).unwrap_err().kind(), ErrorKind::InvalidData);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn empty_actor_is_distinct_from_missing_schema() {
    let reader = RusqliteRecoveryReader::open(fixture()).unwrap();
    assert!(reader.snapshot(999).unwrap().is_none());
    assert!(reader.tail(999).unwrap().is_none());
    assert!(reader.journal(999).unwrap().is_empty());
    assert!(reader.workflow_events(999).unwrap().is_empty());
}

// A regression guard: read-only handle must refuse SQL writes.
#[test]
fn native_sqlite_readonly_flag_rejects_mutations() {
    let connection = rusqlite::Connection::open_with_flags(
        fixture(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert!(connection
        .execute("UPDATE durable_tails SET sequence=99 WHERE actor_id=42", [])
        .is_err());
}
