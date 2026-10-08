//! Synchronize ordinary WAL mutations with managed directory adoption.
//! API-level fencing uses the same stable OS lock inode in both paths.

use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::split::{SingleNodeSplitStore, SplitError};
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation, TabletWrite};
use nulang::database::wal::{FileWal, WalError};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nudb_io_gate_{name}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(921).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap()
}

fn put(previous: u64) -> TabletWrite {
    TabletWrite::prepare(
        &descriptor(),
        3,
        previous,
        previous,
        vec![TabletMutation::Put {
            key: b"b".to_vec(),
            value: b"test".to_vec(),
        }],
    )
    .unwrap()
}

#[test]
fn active_public_io_gate_prevents_owner_adoption_without_creating_manifest() {
    let root = root("takeover");
    fs::create_dir_all(&root).unwrap();
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(root.join(".nudb-write-gate.lock"))
        .unwrap();
    lock.try_lock().unwrap();

    assert!(matches!(
        SingleNodeSplitStore::open(&root, descriptor()),
        Err(SplitError::OwnerBusy)
    ));
    assert!(!root.join("route.manifest").exists());
    assert!(!root.join("parent.wal").exists());

    drop(lock);
    let owner = SingleNodeSplitStore::open(&root, descriptor()).unwrap();
    assert!(root.join("route.manifest").exists());
    drop(owner);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn owner_gate_prevents_direct_public_open_from_initializing_wal() {
    let root = root("open");
    let owner = SingleNodeSplitStore::open(&root, descriptor()).unwrap();
    let missing = root.join("other.wal");
    assert!(matches!(
        FileWal::open(&missing),
        Err(WalError::ManagedDirectory)
    ));
    assert!(!missing.exists());
    drop(owner);
    assert!(matches!(
        FileWal::open(&missing),
        Err(WalError::ManagedDirectory)
    ));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn old_public_handle_cannot_append_or_reclaim_while_new_owner_has_gate() {
    let root = root("stale");
    fs::create_dir_all(&root).unwrap();
    let legacy_path = root.join("other.wal");
    let mut stale = FileWal::open(&legacy_path).unwrap();
    stale.append_write(&put(0)).unwrap();

    let owner = SingleNodeSplitStore::open(&root, descriptor()).unwrap();
    assert!(matches!(
        stale.append_write(&put(1)),
        Err(WalError::ManagedDirectory)
    ));
    assert!(matches!(
        stale.reclaim_through(1),
        Err(WalError::ManagedDirectory)
    ));
    assert_eq!(stale.last_sequence(), 1);
    drop(owner);
    assert!(matches!(
        stale.append_write(&put(1)),
        Err(WalError::ManagedDirectory)
    ));

    // A denied write did not alter committed state or WAL bytes.
    assert_eq!(stale.last_sequence(), 1);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn unmanaged_wal_writes_and_reclamation_still_work_with_persistent_gate() {
    let root = root("standalone");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("ordinary.wal");
    let mut wal = FileWal::open(&path).unwrap();
    wal.append_write(&put(0)).unwrap();
    wal.reclaim_through(1).unwrap();
    assert!(root.join(".nudb-write-gate.lock").exists());
    drop(wal);
    let wal = FileWal::open(&path).unwrap();
    assert_eq!(wal.base_sequence(), 1);
    assert_eq!(wal.last_sequence(), 1);
    drop(wal);
    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn public_symlink_to_unmanaged_wal_uses_real_directory_gate() {
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::time::Duration;

    let actual_root = root("actual");
    let aliases = root("aliases");
    fs::create_dir_all(&actual_root).unwrap();
    fs::create_dir_all(&aliases).unwrap();
    let real = actual_root.join("ordinary.wal");
    let old = FileWal::open(&real).unwrap();
    drop(old);

    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(actual_root.join(".nudb-write-gate.lock"))
        .unwrap();
    lock.try_lock().unwrap();

    let alias = aliases.join("linked.wal");
    symlink(&real, &alias).unwrap();
    let (sender, done) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        sender.send(FileWal::open(&alias).is_ok()).unwrap();
    });
    // The public open must wait while the real target's lock is held.
    assert!(matches!(
        done.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(lock);
    assert!(done.recv_timeout(Duration::from_secs(5)).unwrap());
    reader.join().unwrap();
    let _ = fs::remove_dir_all(aliases);
    let _ = fs::remove_dir_all(actual_root);
}
