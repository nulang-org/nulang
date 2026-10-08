//! Demonstrates the known raw-parent-WAL bypass of the single-node owner lock.
//! This is an exclusion test, NOT a proof of fencing or a supported writer API.
//! Do not promote direct parent WAL access while a split coordinator is active.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::split::SingleNodeSplitStore;
use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn parent() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(991).unwrap(),
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
fn raw_parent_wal_writer_can_bypass_owner_after_published_split() {
    let dir = std::env::temp_dir().join(format!(
        "nudb_raw_wal_boundary_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut owner = SingleNodeSplitStore::open(&dir, parent()).unwrap();
    owner.commit(put(b"b", b"published")).unwrap();

    // The direct tablet deliberately does not acquire .nudb-owner.lock.
    // Capturing it before split models an uncooperative stale parent handle.
    let mut stale_parent = WalBackedTablet::open(parent(), dir.join("parent.wal")).unwrap();
    let plan = parent()
        .plan_split(
            b"m",
            TabletId::new(992).unwrap(),
            TabletId::new(993).unwrap(),
            8,
        )
        .unwrap();
    owner.split(&plan).unwrap();

    let write = stale_parent
        .prepare_write(7, 1, vec![put(b"b", b"unrouted-stale-write")])
        .unwrap();
    // This currently succeeds: the owner lock is advisory and not enforced by
    // raw WAL APIs. It must not be mistaken for successful parent fencing.
    assert_eq!(stale_parent.commit(write).unwrap(), 2);
    assert_eq!(
        owner.read_latest(b"b").unwrap(),
        Some(b"published".to_vec())
    );
    drop(stale_parent);
    drop(owner);

    // Recovery follows the durable child-routing manifest, not stale parent WAL.
    let reopened = SingleNodeSplitStore::open(&dir, parent()).unwrap();
    assert!(reopened.is_split());
    assert_eq!(
        reopened.read_latest(b"b").unwrap(),
        Some(b"published".to_vec())
    );
    drop(reopened);
    let _ = fs::remove_dir_all(dir);
}
