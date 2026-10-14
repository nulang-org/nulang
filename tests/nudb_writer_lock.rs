//! WAL writer ownership regressions using cross-process OS file locks.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};
use nulang::database::wal::{FileWal, WalError};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_writer_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn cleanup(path: &Path) {
    // Call only after all holders are dropped. Never unlink an active lock.
    for target in [
        path.to_path_buf(),
        suffix(path, ".lock"),
        suffix(path, ".reclaim.tmp"),
        path.with_extension("checkpoint"),
        suffix(&path.with_extension("checkpoint"), ".tmp"),
        suffix(path, ".ack"),
    ] {
        let _ = fs::remove_file(target);
    }
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(999).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        7,
    )
    .unwrap()
}

#[test]
fn two_open_wal_handles_cannot_be_writers_simultaneously() {
    let path = path("same_process");
    cleanup(&path);
    let first = FileWal::open(&path).unwrap();
    assert!(matches!(
        FileWal::open(&path),
        Err(WalError::WriterAlreadyActive { .. })
    ));
    assert!(matches!(
        WalBackedTablet::open(descriptor(), &path),
        Err(WalBackedError::Wal(WalError::WriterAlreadyActive { .. }))
    ));
    drop(first);

    // Lock release is tied to the file descriptor, not lockfile removal.
    let reopened = FileWal::open(&path).unwrap();
    assert_eq!(reopened.last_sequence(), 0);
    assert!(suffix(&path, ".lock").exists());
    drop(reopened);
    cleanup(&path);
}

#[test]
fn checkpoint_reclamation_keeps_the_same_writer_lock() {
    let path = path("checkpoint");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let id = TransactionId::new([21; 16]);
    let mutations = vec![TabletMutation::Put {
        key: b"k".to_vec(),
        value: b"value".to_vec(),
    }];
    let receipt = tablet.commit_idempotent(id, mutations.clone()).unwrap();

    tablet.checkpoint().unwrap();
    assert!(matches!(
        FileWal::open(&path),
        Err(WalError::WriterAlreadyActive { .. })
    ));
    assert_eq!(tablet.commit_idempotent(id, mutations.clone()).unwrap(), receipt);
    assert_eq!(tablet.current_sequence(), 1);
    drop(tablet);

    let mut reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.commit_idempotent(id, mutations).unwrap(), receipt);
    assert_eq!(reopened.current_sequence(), 1);
    drop(reopened);
    cleanup(&path);
}

#[test]
#[ignore = "subprocess fixture; started by the competing_writer_process test"]
fn writer_lock_child() {
    let Some(path) = std::env::var_os("NUDB_WRITER_LOCK_WAL") else {
        return;
    };
    let ack = PathBuf::from(std::env::var_os("NUDB_WRITER_LOCK_ACK").unwrap());
    let _owner = FileWal::open(path).unwrap();
    let mut file = fs::File::create(ack).unwrap();
    file.write_all(b"locked").unwrap();
    file.sync_all().unwrap();
    loop {
        thread::park();
    }
}

#[test]
fn competing_writer_process_is_rejected_and_killed_owner_releases_lock() {
    let path = path("process");
    cleanup(&path);
    let ack = suffix(&path, ".ack");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "writer_lock_child", "--ignored", "--nocapture"])
        .env("NUDB_WRITER_LOCK_WAL", &path)
        .env("NUDB_WRITER_LOCK_ACK", &ack)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let mut ready = false;
    for _ in 0..1_000 {
        if fs::read(&ack).is_ok_and(|value| value.as_slice() == b"locked") {
            ready = true;
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("writer subprocess exited before acquiring lock: {status}");
        }
        thread::sleep(Duration::from_millis(5));
    }

    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        panic!("writer subprocess did not report lock acquisition");
    }

    assert!(matches!(
        FileWal::open(&path),
        Err(WalError::WriterAlreadyActive { .. })
    ));
    child.kill().unwrap();
    child.wait().unwrap();

    let new_owner = FileWal::open(&path).unwrap();
    assert_eq!(new_owner.last_sequence(), 0);
    drop(new_owner);
    cleanup(&path);
}
