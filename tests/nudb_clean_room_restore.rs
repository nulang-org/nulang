//! Destructive, single-host clean-room restore rehearsal for a completed NuDB backup.
//! This proves that the captured artifact does not depend on the source process
//! or its WAL files. It does NOT certify off-host archival, power-loss behavior,
//! network filesystems, or a managed Cloud restore service.
#![cfg(unix)]

use nulang::database::backup::audit_completed_backup_read_only;
use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};
use nulang::database::wal::WalError;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(1);
const QUOTA: u64 = 16 * 1024;
const FIRST_ID: [u8; 16] = [0x31; 16];
const SECOND_ID: [u8; 16] = [0x32; 16];

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nudb_clean_room_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        private_dir(&root);
        private_dir(&root.join("source"));
        Self(root)
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn private_dir(path: &Path) {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path).unwrap();
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(807).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap()
}

fn put(key: &[u8], value: &[u8]) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }]
}

// Child fixture is dormant in ordinary test runs and only writes disposable
// data when launched by the parent with an explicit scoped WAL path.
#[test]
fn clean_room_fixture_child() {
    let Some(root) = std::env::var_os("NUDB_CLEAN_ROOM_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let mut tablet = WalBackedTablet::open(descriptor(), root.join("source/tablet.wal")).unwrap();
    tablet
        .commit_idempotent(TransactionId::new(FIRST_ID), put(b"k", b"early"))
        .unwrap();
    tablet.checkpoint().unwrap();
    tablet
        .commit_idempotent(TransactionId::new(SECOND_ID), put(b"k", b"latest"))
        .unwrap();
    tablet.activate_durable_wal_quota(QUOTA).unwrap();
    let backup = root.join("completed-backup");
    let report = tablet.capture_consistent_backup(&backup).unwrap();
    assert_eq!(report.sequence, 2);

    // Notify the parent only after backup verification and manifest publication.
    let ready_path = root.join("READY");
    let mut ready = File::create(&ready_path).unwrap();
    ready.write_all(b"ACK 2\n").unwrap();
    ready.sync_all().unwrap();
    drop(ready);
    File::open(&root).unwrap().sync_all().unwrap();

    // The parent kills this process instead of allowing graceful shutdown.
    loop {
        thread::park();
    }
}

fn launch_and_kill_after_backup(root: &Path) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "clean_room_fixture_child", "--nocapture"])
        .env("NUDB_CLEAN_ROOM_FIXTURE", root)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_for_ack_and_kill(&mut child, &root.join("READY"));
}

fn wait_for_ack_and_kill(child: &mut Child, marker: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::read(marker).is_ok_and(|s| s == b"ACK 2\n") {
            break;
        }
        if let Some(exit) = child.try_wait().unwrap() {
            panic!("backup fixture exited before a verified receipt: {exit}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("backup fixture did not acknowledge within deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.kill().unwrap(); // Unix Child::kill sends SIGKILL.
    assert!(!child.wait().unwrap().success(), "fixture must be killed");
}

// Copy ONLY published backup components; never copy the source WAL or locks.
// Stage the completion marker last, so incomplete copies cannot be audited as
// complete backups. The fixture uses trusted, quiescent local directories.
fn stage_backup_into_clean_room(backup: &Path, restore: &Path) {
    assert!(!restore.exists(), "restore destination must be new");
    private_dir(restore);
    for name in [
        "tablet.wal",
        "tablet.checkpoint",
        "tablet.wal.quota",
        "COMPLETE.json",
    ] {
        let from = backup.join(name);
        let meta = match fs::symlink_metadata(&from) {
            Ok(meta) => meta,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && matches!(name, "tablet.checkpoint" | "tablet.wal.quota") =>
            {
                continue
            }
            Err(error) => panic!("missing backup artifact {name}: {error}"),
        };
        assert!(
            meta.file_type().is_file(),
            "backup component must be a regular file"
        );
        assert!(
            meta.len() <= 64 * 1024 * 1024,
            "test fixture rejects oversized component"
        );
        let copied = fs::copy(&from, restore.join(name)).unwrap();
        assert_eq!(copied, meta.len());
    }
}

#[test]
fn sigkill_source_then_restore_independent_backup_with_receipts_and_quota() {
    let sandbox = Sandbox::new();
    launch_and_kill_after_backup(&sandbox.0);
    let backup = sandbox.path("completed-backup");
    let before = audit_completed_backup_read_only(descriptor(), &backup).unwrap();
    assert_eq!(before.sequence, 2);

    // Require recovery to be independent of original storage and its owner.
    fs::remove_dir_all(sandbox.path("source")).unwrap();
    let restore = sandbox.path("recovery");
    let restore_started = Instant::now();
    stage_backup_into_clean_room(&backup, &restore);

    let verified = audit_completed_backup_read_only(descriptor(), &restore).unwrap();
    assert_eq!(verified.sequence, before.sequence);
    assert_eq!(verified.wal_blake3, before.wal_blake3);
    assert_eq!(verified.checkpoint_blake3, before.checkpoint_blake3);
    let mut tablet = WalBackedTablet::open(descriptor(), restore.join("tablet.wal")).unwrap();
    assert_eq!(tablet.current_sequence(), 2);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"early"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"latest"[..]));
    assert_eq!(tablet.durable_wal_quota(), Some(QUOTA));
    eprintln!(
        "diagnostic: local clean-room restore stages/audits/opens in {} ms ({} WAL bytes); not off-host RTO",
        restore_started.elapsed().as_millis(),
        verified.wal_bytes
    );

    // Committed identity must be durable: retries cannot create a new version.
    let first = tablet
        .commit_idempotent(TransactionId::new(FIRST_ID), put(b"k", b"early"))
        .unwrap();
    let second = tablet
        .commit_idempotent(TransactionId::new(SECOND_ID), put(b"k", b"latest"))
        .unwrap();
    assert_eq!(first.sequence(), 1);
    assert_eq!(second.sequence(), 2);
    assert_eq!(tablet.current_sequence(), 2);

    // A restored tablet may not silently lose its durable write quota.
    let enormous = tablet
        .prepare_write(3, 2, put(b"large", &vec![1; QUOTA as usize]))
        .unwrap();
    assert!(matches!(
        tablet.commit(enormous),
        Err(WalBackedError::Wal(WalError::BudgetExceeded { .. }))
    ));
    assert_eq!(tablet.current_sequence(), 2);
    drop(tablet);

    // A different tablet identity cannot adopt the artifact.
    let mismatched = TabletDescriptor::new(
        TabletId::new(808).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap();
    assert!(audit_completed_backup_read_only(mismatched, &restore).is_err());
}
