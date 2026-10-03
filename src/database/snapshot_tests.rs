use super::snapshot::{SnapshotError, SnapshotRegistry};
use super::store::WalBackedTablet;
use super::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

static NEXT_TABLET_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_snapshot_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TABLET_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(909).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
    let _ = fs::remove_file(path.with_extension("manifest"));
    let _ = fs::remove_dir_all(path.with_extension("sstables"));
}

fn commit_put(tablet: &mut WalBackedTablet, key: &[u8], value: &[u8]) {
    let sequence = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            1,
            sequence,
            vec![TabletMutation::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
}

#[test]
fn oldest_live_snapshot_tracks_duplicate_pins_and_release_order() {
    let registry = SnapshotRegistry::new();
    assert_eq!(registry.oldest_live_snapshot(), None);
    assert_eq!(registry.active_snapshot_count(), 0);

    let seven_a = registry.pin(7);
    let three = registry.pin(3);
    let seven_b = registry.pin(7);

    assert_eq!(registry.oldest_live_snapshot(), Some(3));
    assert_eq!(registry.active_snapshot_count(), 3);

    drop(three);
    assert_eq!(registry.oldest_live_snapshot(), Some(7));
    assert_eq!(registry.active_snapshot_count(), 2);

    drop(seven_a);
    assert_eq!(registry.oldest_live_snapshot(), Some(7));
    assert_eq!(registry.active_snapshot_count(), 1);

    drop(seven_b);
    assert_eq!(registry.oldest_live_snapshot(), None);
    assert_eq!(registry.active_snapshot_count(), 0);
}

#[test]
fn retention_floor_is_current_sequence_without_readers_and_oldest_pin_with_readers() {
    let registry = SnapshotRegistry::new();
    assert_eq!(registry.retention_floor(12), 12);

    let eight = registry.pin(8);
    let ten = registry.pin(10);
    assert_eq!(registry.retention_floor(12), 8);

    drop(eight);
    assert_eq!(registry.retention_floor(12), 10);

    drop(ten);
    assert_eq!(registry.retention_floor(12), 12);
}

#[test]
fn retention_floor_never_advances_beyond_current_sequence() {
    let registry = SnapshotRegistry::new();
    let future = registry.pin(20);

    assert_eq!(registry.oldest_live_snapshot(), Some(20));
    assert_eq!(registry.retention_floor(10), 10);

    drop(future);
}

#[test]
fn snapshot_zero_is_a_valid_pin_and_blocks_floor_advancement() {
    let registry = SnapshotRegistry::new();
    let zero = registry.pin(0);

    assert_eq!(zero.sequence(), 0);
    assert_eq!(registry.oldest_live_snapshot(), Some(0));
    assert_eq!(registry.retention_floor(50), 0);

    drop(zero);
    assert_eq!(registry.retention_floor(50), 50);
}

#[test]
fn explicit_release_is_idempotent_with_drop() {
    let registry = SnapshotRegistry::new();
    let pin = registry.pin(4);
    assert_eq!(registry.active_snapshot_count(), 1);

    pin.release();
    assert_eq!(registry.active_snapshot_count(), 0);
    assert_eq!(registry.oldest_live_snapshot(), None);
}

#[test]
fn registry_is_thread_safe_and_observes_cross_thread_pins() {
    let registry = SnapshotRegistry::new();
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));

    let worker_registry = registry.clone();
    let worker_ready = Arc::clone(&ready);
    let worker_release = Arc::clone(&release);
    let worker = thread::spawn(move || {
        let pin = worker_registry.pin(6);
        worker_ready.wait();
        worker_release.wait();
        drop(pin);
    });

    ready.wait();
    assert_eq!(registry.oldest_live_snapshot(), Some(6));
    assert_eq!(registry.active_snapshot_count(), 1);

    release.wait();
    worker.join().unwrap();
    assert_eq!(registry.oldest_live_snapshot(), None);
    assert_eq!(registry.active_snapshot_count(), 0);
}

#[test]
fn tablet_pins_current_snapshot_and_rejects_future_sequence() {
    let path = temp_wal("tablet-pin");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    commit_put(&mut tablet, b"k", b"one");
    let current = tablet.pin_snapshot();
    assert_eq!(current.sequence(), 1);
    assert_eq!(tablet.oldest_live_snapshot(), Some(1));
    assert_eq!(tablet.snapshot_retention_floor(), 1);

    assert_eq!(
        tablet.pin_snapshot_at(2).unwrap_err(),
        SnapshotError::FutureSnapshot {
            requested: 2,
            committed: 1,
        }
    );

    drop(current);
    assert_eq!(tablet.oldest_live_snapshot(), None);
    assert_eq!(tablet.snapshot_retention_floor(), 1);
    cleanup(&path);
}

#[test]
fn active_old_snapshot_holds_retention_floor_while_new_commits_arrive() {
    let path = temp_wal("floor-hold");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    commit_put(&mut tablet, b"k", b"one");
    let old_reader = tablet.pin_snapshot_at(1).unwrap();
    commit_put(&mut tablet, b"k", b"two");

    assert_eq!(tablet.current_sequence(), 2);
    assert_eq!(tablet.oldest_live_snapshot(), Some(1));
    assert_eq!(tablet.snapshot_retention_floor(), 1);

    drop(old_reader);
    assert_eq!(tablet.oldest_live_snapshot(), None);
    assert_eq!(tablet.snapshot_retention_floor(), 2);
    cleanup(&path);
}

#[test]
fn tablet_allows_pinning_snapshot_zero_after_commits() {
    let path = temp_wal("zero-after-commit");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"k", b"one");

    let zero = tablet.pin_snapshot_at(0).unwrap();
    assert_eq!(zero.sequence(), 0);
    assert_eq!(tablet.snapshot_retention_floor(), 0);

    drop(zero);
    assert_eq!(tablet.snapshot_retention_floor(), 1);
    cleanup(&path);
}
