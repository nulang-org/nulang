//! Exclusive single-node NuDB directory ownership contract.
//! The lock must outlive the store, including after routing promotion.
//! A second OS process must not independently replay or write the same WALs.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::split::{SingleNodeSplitStore, SplitError};
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nudb_exclusive_owner_{name}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ))
}

fn parent() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(891).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        7,
    )
    .unwrap()
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

#[test]
fn cannot_open_same_directory_twice_until_first_store_is_dropped() {
    let dir = root("reopen");
    {
        let mut primary = SingleNodeSplitStore::open(&dir, parent()).unwrap();
        assert!(matches!(
            SingleNodeSplitStore::open(&dir, parent()),
            Err(SplitError::OwnerBusy)
        ));
        primary.commit(put(b"b", b"before")).unwrap();
    }
    let reopened = SingleNodeSplitStore::open(&dir, parent()).unwrap();
    assert_eq!(
        reopened.read_latest(b"b").unwrap(),
        Some(b"before".to_vec())
    );
    drop(reopened);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn exclusive_ownership_survives_split_publication() {
    let dir = root("split");
    let plan = parent()
        .plan_split(
            b"m",
            TabletId::new(892).unwrap(),
            TabletId::new(893).unwrap(),
            8,
        )
        .unwrap();
    {
        let mut primary = SingleNodeSplitStore::open(&dir, parent()).unwrap();
        primary.commit(put(b"b", b"left")).unwrap();
        primary.split(&plan).unwrap();
        assert!(matches!(
            SingleNodeSplitStore::open(&dir, parent()),
            Err(SplitError::OwnerBusy)
        ));
        primary.commit(put(b"n", b"right")).unwrap();
    }
    let recovered = SingleNodeSplitStore::open(&dir, parent()).unwrap();
    assert!(recovered.is_split());
    assert_eq!(
        recovered.read_latest(b"n").unwrap(),
        Some(b"right".to_vec())
    );
    drop(recovered);
    let _ = fs::remove_dir_all(&dir);
}

/// Child process is invoked explicitly by the parent test below.
#[test]
#[ignore]
fn locked_directory_child_fixture() {
    let dir = std::env::var("NULANG_NUDB_CHILD_LOCK_ROOT").unwrap();
    assert!(matches!(
        SingleNodeSplitStore::open(dir, parent()),
        Err(SplitError::OwnerBusy)
    ));
}

#[test]
fn another_process_cannot_take_the_live_directory_lock() {
    let dir = root("crossprocess");
    let primary = SingleNodeSplitStore::open(&dir, parent()).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("locked_directory_child_fixture")
        .arg("--ignored")
        .arg("--nocapture")
        .env("NULANG_NUDB_CHILD_LOCK_ROOT", &dir)
        .output()
        .unwrap();
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "contending child test must execute and reject ownership: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    drop(primary);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn lock_file_inode_is_retained_after_owner_releases_it() {
    let dir = root("inode");
    let owner = SingleNodeSplitStore::open(&dir, parent()).unwrap();
    let lock_file = dir.join(".nudb-owner.lock");
    assert!(lock_file.exists());
    drop(owner);

    // Deleting/recreating an advisory lock file permits concurrent locks on
    // different inodes. The coordinator must never unlink it on drop.
    assert!(lock_file.exists());
    let recovered = SingleNodeSplitStore::open(&dir, parent()).unwrap();
    drop(recovered);
    let _ = fs::remove_dir_all(dir);
}
