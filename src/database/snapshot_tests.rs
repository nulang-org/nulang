use super::snapshot::SnapshotRegistry;
use std::sync::{Arc, Barrier};
use std::thread;

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
