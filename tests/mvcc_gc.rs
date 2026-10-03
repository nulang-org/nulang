use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, MemoryTablet, SnapshotTracker, TabletDescriptor, TabletError, TabletId,
    TabletMutation,
};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(81).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_mvcc_gc_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn commit_put(tablet: &mut MemoryTablet, key: &[u8], value: &[u8]) {
    let previous = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            1,
            previous,
            vec![TabletMutation::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
}

fn commit_delete(tablet: &mut MemoryTablet, key: &[u8]) {
    let previous = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            1,
            previous,
            vec![TabletMutation::Delete { key: key.to_vec() }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
}

fn commit_wal_put(tablet: &mut WalBackedTablet, key: &[u8], value: &[u8]) {
    let previous = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            1,
            previous,
            vec![TabletMutation::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
}

#[test]
fn gc_retains_the_safe_point_version_and_rejects_older_snapshots() {
    let mut tablet = MemoryTablet::new(descriptor());
    commit_put(&mut tablet, b"k", b"v1");
    commit_put(&mut tablet, b"k", b"v2");
    commit_put(&mut tablet, b"k", b"v3");

    let stats = tablet.collect_garbage(2).unwrap();

    assert_eq!(stats.safe_point, 2);
    assert_eq!(stats.versions_removed, 1);
    assert_eq!(stats.keys_removed, 0);
    assert_eq!(tablet.oldest_readable_sequence(), 2);
    assert_eq!(
        tablet.read_at(b"k", 1).unwrap_err(),
        TabletError::SnapshotCollected {
            oldest_readable: 2,
            requested: 1,
        }
    );
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    assert_eq!(tablet.read_at(b"k", 3).unwrap(), Some(&b"v3"[..]));
}

#[test]
fn gc_can_reclaim_a_tombstoned_key_when_no_newer_version_exists() {
    let mut tablet = MemoryTablet::new(descriptor());
    commit_put(&mut tablet, b"k", b"value");
    commit_delete(&mut tablet, b"k");
    commit_put(&mut tablet, b"other", b"x");

    let stats = tablet.collect_garbage(2).unwrap();

    assert_eq!(stats.versions_removed, 2);
    assert_eq!(stats.keys_removed, 1);
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), None);
    assert_eq!(tablet.read_latest(b"k"), None);
    assert_eq!(tablet.read_latest(b"other"), Some(&b"x"[..]));
}

#[test]
fn gc_keeps_a_tombstone_anchor_when_a_newer_value_exists() {
    let mut tablet = MemoryTablet::new(descriptor());
    commit_put(&mut tablet, b"k", b"v1");
    commit_delete(&mut tablet, b"k");
    commit_put(&mut tablet, b"k", b"v3");

    let stats = tablet.collect_garbage(2).unwrap();

    assert_eq!(stats.versions_removed, 1);
    assert_eq!(stats.keys_removed, 0);
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), None);
    assert_eq!(tablet.read_at(b"k", 3).unwrap(), Some(&b"v3"[..]));
}

#[test]
fn snapshot_tracker_derives_the_oldest_active_safe_point() {
    let mut tablet = MemoryTablet::new(descriptor());
    commit_put(&mut tablet, b"k", b"v1");
    commit_put(&mut tablet, b"k", b"v2");
    commit_put(&mut tablet, b"k", b"v3");

    let mut snapshots = SnapshotTracker::default();
    assert_eq!(snapshots.safe_point(tablet.current_sequence()), 3);

    snapshots.pin(&tablet, 2).unwrap();
    snapshots.pin(&tablet, 2).unwrap();
    snapshots.pin(&tablet, 3).unwrap();
    assert_eq!(snapshots.safe_point(tablet.current_sequence()), 2);

    assert!(snapshots.unpin(2));
    assert_eq!(snapshots.safe_point(tablet.current_sequence()), 2);
    assert!(snapshots.unpin(2));
    assert_eq!(snapshots.safe_point(tablet.current_sequence()), 3);
    assert!(!snapshots.unpin(2));

    assert_eq!(
        snapshots.pin(&tablet, 4).unwrap_err(),
        TabletError::SnapshotAhead {
            committed: 3,
            requested: 4,
        }
    );

    tablet.collect_garbage(3).unwrap();
    assert_eq!(
        snapshots.pin(&tablet, 2).unwrap_err(),
        TabletError::SnapshotCollected {
            oldest_readable: 3,
            requested: 2,
        }
    );
}

#[test]
fn checkpoint_preserves_the_mvcc_retention_floor() {
    let wal_path = temp_wal("checkpoint");
    let checkpoint_path = wal_path.with_extension("checkpoint");
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint_path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        commit_wal_put(&mut tablet, b"k", b"v1");
        commit_wal_put(&mut tablet, b"k", b"v2");
        commit_wal_put(&mut tablet, b"k", b"v3");
        tablet.collect_garbage(2).unwrap();
        tablet.checkpoint().unwrap();
    }

    let tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(tablet.oldest_readable_sequence(), 2);
    assert_eq!(
        tablet.read_at(b"k", 1).unwrap_err(),
        TabletError::SnapshotCollected {
            oldest_readable: 2,
            requested: 1,
        }
    );
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    assert_eq!(tablet.read_latest(b"k"), Some(&b"v3"[..]));

    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint_path);
}
