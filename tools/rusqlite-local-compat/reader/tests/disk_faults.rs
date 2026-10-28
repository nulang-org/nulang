//! Fault injection in disposable, *real* SQLite files only; no production DBs.
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, JournalEntry, PersistedValue,
    DURABLE_TRANSITION_VERSION,
};
use nulang_rusqlite_recovery_reader::{RusqliteAtomicWriter, RusqliteRecoveryReader};
use rusqlite::Connection;
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn fixture(label: &str) -> (PathBuf, PathBuf) {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-sqlite-fault-{label}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("durable.db");
    fs::copy(std::env::var_os("NULANG_LEGACY_FIXTURE_DB").unwrap(), &path).unwrap();
    (dir, path)
}

fn transition(tag: String) -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 42,
        activation_epoch: 1,
        sequence: 3,
        expected_previous_sequence: 2,
        command: Some(JournalEntry {
            sequence: 3,
            behavior_id: 7,
            payload: vec![PersistedValue::String(tag.clone())],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: 42,
            sequence: 3,
            activation_epoch: 1,
            state: [("value".to_owned(), PersistedValue::String(tag.clone()))].into(),
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: 91,
            ordinal: 0,
            behavior_id: 2,
            payload: vec![PersistedValue::String(tag)],
        }],
    }
}

fn no_partial_commit(path: &PathBuf) {
    let reader = RusqliteRecoveryReader::open(path).unwrap();
    assert_eq!(reader.tail(42).unwrap().unwrap().sequence, 2);
    assert_eq!(reader.snapshot(42).unwrap().unwrap().sequence, 2);
    assert!(reader.outbox(42).unwrap().is_empty());
    assert_eq!(reader.journal(42).unwrap().len(), 2);
    drop(reader);
    let c = Connection::open(path).unwrap();
    for table in ["durable_transitions", "durable_outbox", "journal"] {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE actor_id=42 AND sequence=3");
        assert_eq!(
            c.query_row(&sql, [], |r| r.get::<_, i64>(0)).unwrap(),
            0,
            "partially committed {table}"
        );
    }
}

#[test]
fn sqlite_page_budget_exhaustion_rolls_back_and_retry_succeeds() {
    let (dir, path) = fixture("sqlite-full");
    let mut writer = RusqliteAtomicWriter::open_existing(&path).unwrap();
    let limit = writer.limit_pages_to_current_for_testing().unwrap();
    assert!(limit > 0);
    let huge = transition("x".repeat(2 * 1024 * 1024));
    let err = writer
        .commit(&huge)
        .expect_err("page cap must reject growth with SQLITE_FULL");
    assert!(
        err.to_string().contains("full") || err.to_string().contains("FULL"),
        "must observe SQLite FULL (not unrelated error): {err}"
    );
    drop(writer);
    no_partial_commit(&path);
    let mut retry = RusqliteAtomicWriter::open_existing(&path).unwrap();
    retry.commit(&transition("normal".to_owned())).unwrap();
    drop(retry);
    let reader = RusqliteRecoveryReader::open(&path).unwrap();
    assert_eq!(reader.tail(42).unwrap().unwrap().sequence, 3);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn kernel_file_size_limit_rejects_write_and_preserves_committed_tail() {
    use std::process::Command;
    let (dir, path) = fixture("rlimit-fsize");
    let bin = env!("CARGO_BIN_EXE_rusqlite_atomic_fixture");
    // RLIMIT_FSIZE is a real kernel-enforced per-file write constraint,
    // not a proof of ENOSPC or actual filesystem exhaustion. The WAL must
    // exceed 8 KiB for the large synthetic transition.
    let huge = "x".repeat(64 * 1024);
    let status = Command::new("prlimit")
        .arg("--fsize=8192")
        .arg("--")
        .arg(bin)
        .arg(&path)
        .args(["42", "1", "3", "2"])
        .arg(huge)
        .status()
        .expect("CI runner must install util-linux prlimit");
    assert!(!status.success(), "OS write limit must not report success");
    no_partial_commit(&path);
    let mut retry = RusqliteAtomicWriter::open_existing(&path).unwrap();
    retry.commit(&transition("after-rlimit".into())).unwrap();
    drop(retry);
    assert_eq!(
        RusqliteRecoveryReader::open(&path)
            .unwrap()
            .tail(42)
            .unwrap()
            .unwrap()
            .sequence,
        3
    );
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(target_os = "linux")]
fn injected_wal_fault(mode: &str, marker: &str) {
    use std::process::Command;

    let (dir, path) = fixture(mode);
    let shim = std::env::var_os("NULANG_SQLITE_FAULT_SHIM_PATH")
        .expect("test runner must build the isolated Linux preload shim");
    let bin = env!("CARGO_BIN_EXE_rusqlite_atomic_fixture");

    // Only this subprocess receives LD_PRELOAD. The shim matches the
    // exact disposable SQLite database WAL path and never touches
    // another file, process, or service.
    let output = Command::new(bin)
        .arg(&path)
        .args(["42", "1", "3", "2"])
        .arg("atomic-write-with-fault")
        .env("LD_PRELOAD", shim)
        .env("NULANG_SQLITE_FAULT_DB_PATH", &path)
        .env("NULANG_SQLITE_FAULT_MODE", mode)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "injected {mode} must prevent reporting a committed transition"
    );
    assert!(
        stderr.contains(marker),
        "required SQLite WAL syscall was not intercepted in {mode}: {stderr}"
    );
    no_partial_commit(&path);
    let mut retry = RusqliteAtomicWriter::open_existing(&path).unwrap();
    retry.commit(&transition(format!("retry-{mode}"))).unwrap();
    drop(retry);
    let reader = RusqliteRecoveryReader::open(&path).unwrap();
    assert_eq!(reader.tail(42).unwrap().unwrap().sequence, 3);
    assert_eq!(reader.outbox(42).unwrap().len(), 1);
    drop(reader);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn injected_kernel_enospc_on_wal_write_preserves_committed_state() {
    injected_wal_fault("enospc-write", "NULANG_FAULT_INJECTED_ENOSPC_WAL_WRITE");
}

#[cfg(target_os = "linux")]
#[test]
fn injected_kernel_eio_on_wal_sync_preserves_committed_state() {
    injected_wal_fault("eio-sync", "NULANG_FAULT_INJECTED_EIO_WAL_SYNC");
}

#[cfg(target_os = "linux")]
#[test]
fn syscall_fault_shim_does_not_affect_unrelated_sqlite_files() {
    use std::process::Command;

    let shim = std::env::var_os("NULANG_SQLITE_FAULT_SHIM_PATH")
        .expect("test runner must configure scoped fault library");
    let bin = env!("CARGO_BIN_EXE_rusqlite_atomic_fixture");
    for mode in ["enospc-write", "eio-sync"] {
        let (dir, path) = fixture("fault-whitelist");
        let other_path = dir.join("some-other-database.db");
        let output = Command::new(bin)
            .arg(&path)
            .args(["42", "1", "3", "2", "whitelist"])
            .env("LD_PRELOAD", &shim)
            .env("NULANG_SQLITE_FAULT_DB_PATH", &other_path)
            .env("NULANG_SQLITE_FAULT_MODE", mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "shim must not affect a different WAL path in mode {mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("NULANG_FAULT_INJECTED_"),
            "shims must not intercept unrelated WAL descriptors"
        );
        assert_eq!(
            RusqliteRecoveryReader::open(&path)
                .unwrap()
                .tail(42)
                .unwrap()
                .unwrap()
                .sequence,
            3
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
