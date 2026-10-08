//! Prevent public WAL APIs from bypassing a coordinator-owned NuDB directory.
//! This is cooperative API fencing, not physical fencing of arbitrary writes.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::split::SingleNodeSplitStore;
use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TabletWrite,
};
use nulang::database::wal::{FileWal, WalError};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nudb_managed_wal_{name}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(901).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap()
}

fn write(previous: u64, key: &[u8]) -> TabletWrite {
    TabletWrite::prepare(
        &descriptor(),
        3,
        previous,
        previous,
        vec![TabletMutation::Put {
            key: key.to_vec(),
            value: b"direct".to_vec(),
        }],
    )
    .unwrap()
}

#[test]
fn direct_wal_and_tablet_open_are_rejected_for_coordinator_directories() {
    let root = root("managed_open");
    let mut owner = SingleNodeSplitStore::open(&root, descriptor()).unwrap();
    let parent = root.join("parent.wal");

    assert!(matches!(
        FileWal::open(&parent),
        Err(WalError::ManagedDirectory)
    ));
    assert!(matches!(
        WalBackedTablet::open(descriptor(), &parent),
        Err(WalBackedError::Wal(WalError::ManagedDirectory))
    ));

    owner.commit(TabletMutation::Put {
        key: b"b".to_vec(),
        value: b"owner".to_vec(),
    }).unwrap();
    assert_eq!(owner.read_latest(b"b").unwrap(), Some(b"owner".to_vec()));
    drop(owner);

    // A dead owner's lock file is deliberately retained. Public WAL opens
    // remain rejected; a new coordinator must recover the manifest instead.
    assert!(matches!(
        FileWal::open(&parent),
        Err(WalError::ManagedDirectory)
    ));
    let owner = SingleNodeSplitStore::open(&root, descriptor()).unwrap();
    assert_eq!(owner.read_latest(b"b").unwrap(), Some(b"owner".to_vec()));
    drop(owner);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn direct_parent_and_child_wal_open_are_rejected_after_promotion() {
    let root = root("promoted");
    let mut owner = SingleNodeSplitStore::open(&root, descriptor()).unwrap();
    owner.commit(TabletMutation::Put {
        key: b"b".to_vec(),
        value: b"before".to_vec(),
    }).unwrap();
    let plan = descriptor()
        .plan_split(b"m", TabletId::new(902).unwrap(), TabletId::new(903).unwrap(), 4)
        .unwrap();
    owner.split(&plan).unwrap();
    for name in ["parent.wal", "left.wal", "right.wal"] {
        assert!(matches!(
            FileWal::open(root.join(name)),
            Err(WalError::ManagedDirectory)
        ), "direct open must reject {name}");
    }
    owner.commit(TabletMutation::Put {
        key: b"n".to_vec(),
        value: b"child".to_vec(),
    }).unwrap();
    assert_eq!(owner.read_latest(b"n").unwrap(), Some(b"child".to_vec()));
    drop(owner);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn preexisting_public_wal_handle_cannot_append_or_reclaim_after_directory_is_managed() {
    let root = root("old_handle");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("ordinary.wal");
    let mut legacy = FileWal::open(&path).unwrap();

    // A previously acquired unguarded handle must check the managed marker
    // again when it attempts an operation which changes durable bytes.
    fs::write(root.join(".nudb-owner.lock"), b"").unwrap();
    assert!(matches!(
        legacy.append_write(&write(0, b"b")),
        Err(WalError::ManagedDirectory)
    ));
    assert!(matches!(
        legacy.reclaim_through(0),
        Err(WalError::ManagedDirectory)
    ));
    assert_eq!(legacy.last_sequence(), 0);
    drop(legacy);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn public_wal_api_retains_unmanaged_directory_compatibility() {
    let root = root("legacy");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("ordinary.wal");
    let mut wal = FileWal::open(&path).unwrap();
    wal.append_write(&write(0, b"b")).unwrap();
    wal.reclaim_through(1).unwrap();
    let wal = FileWal::open(&path).unwrap();
    assert_eq!(wal.base_sequence(), 1);
    drop(wal);
    let _ = fs::remove_dir_all(root);
}
