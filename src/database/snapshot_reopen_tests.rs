use super::snapshot::SnapshotRegistry;
use super::store::WalBackedTablet;
use super::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_snapshot_reopen_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(910).unwrap(),
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

fn commit_put(tablet: &mut WalBackedTablet, value: &[u8]) {
    let previous = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            1,
            previous,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
}

#[test]
fn shared_registry_reuses_live_state_for_the_same_storage_path() {
    let path = temp_wal("registry");
    cleanup(&path);

    let first = SnapshotRegistry::shared_for_storage_path(&path);
    let pin = first.pin(4);
    drop(first);

    let reopened = SnapshotRegistry::shared_for_storage_path(&path);
    assert_eq!(reopened.oldest_live_snapshot(), Some(4));
    assert_eq!(reopened.active_snapshot_count(), 1);

    drop(pin);
    assert_eq!(reopened.oldest_live_snapshot(), None);

    cleanup(&path);
}

#[test]
fn live_pin_survives_tablet_drop_and_reopen_and_holds_gc_floor() {
    let path = temp_wal("tablet");
    cleanup(&path);

    let pin = {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit_put(&mut tablet, b"one");
        let pin = tablet.pin_snapshot();
        assert_eq!(pin.sequence(), 1);
        pin
    };

    let mut reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.oldest_live_snapshot(), Some(1));
    assert_eq!(reopened.snapshot_retention_floor(), 1);

    commit_put(&mut reopened, b"two");
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.snapshot_retention_floor(), 1);

    drop(pin);
    assert_eq!(reopened.oldest_live_snapshot(), None);
    assert_eq!(reopened.snapshot_retention_floor(), 2);

    cleanup(&path);
}

#[test]
fn different_storage_paths_never_share_snapshot_pins() {
    let left = temp_wal("left");
    let right = temp_wal("right");
    cleanup(&left);
    cleanup(&right);

    let left_registry = SnapshotRegistry::shared_for_storage_path(&left);
    let right_registry = SnapshotRegistry::shared_for_storage_path(&right);
    let pin = left_registry.pin(3);

    assert_eq!(left_registry.oldest_live_snapshot(), Some(3));
    assert_eq!(right_registry.oldest_live_snapshot(), None);

    drop(pin);
    cleanup(&left);
    cleanup(&right);
}
