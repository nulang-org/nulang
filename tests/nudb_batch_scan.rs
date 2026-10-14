//! Differential coverage for bounded, borrowed MVCC batch scans.
//! The scalar read path is the reference for visibility and tombstone rules.

use std::fs;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, MemoryTablet, TabletDescriptor, TabletError, TabletId, TabletMutation};

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(91).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn window() -> KeyRange {
    KeyRange::new(b"k010".to_vec(), Some(b"k054".to_vec())).unwrap()
}

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put { key: key.to_vec(), value: value.to_vec() }
}

fn collect_memory(tablet: &MemoryTablet, range: &KeyRange, snapshot: u64, batch_size: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut output = Vec::new();
    for batch in tablet.scan_range_at(range, snapshot, size(batch_size)).unwrap() {
        assert!(!batch.rows.is_empty());
        assert!(batch.rows.len() <= batch_size);
        for row in batch.rows {
            output.push((row.key.to_vec(), row.value.to_vec()));
        }
    }
    output
}

#[test]
fn batched_scans_match_scalar_visibility_for_updates_deletes_and_all_snapshots() {
    let mut tablet = MemoryTablet::new(descriptor());
    let keys: Vec<Vec<u8>> = (0..64).map(|i| format!("k{i:03}").into_bytes()).collect();

    // Multiple commits produce history gaps, tombstones, and keys absent at old snapshots.
    for step in 0..9usize {
        let mutations = (0..64usize)
            .filter(|i| (i * 7 + step * 11) % 9 == 0)
            .map(|i| {
                if (i + step) % 4 == 0 {
                    TabletMutation::Delete { key: keys[i].clone() }
                } else {
                    put(&keys[i], format!("version-{step}-{i}").as_bytes())
                }
            })
            .collect();
        let write = tablet.prepare_write(1, tablet.current_sequence(), mutations).unwrap();
        tablet.commit(write).unwrap();
    }

    for snapshot in 0..=tablet.current_sequence() {
        let expected: Vec<_> = keys[10..54]
            .iter()
            .filter_map(|key| tablet.read_at(key, snapshot).unwrap().map(|value| (key.clone(), value.to_vec())))
            .collect();
        for batch_size in [1, 2, 7, 64, 256] {
            assert_eq!(
                collect_memory(&tablet, &window(), snapshot, batch_size),
                expected,
                "snapshot={snapshot}, batch_size={batch_size}"
            );
        }
    }
}

#[test]
fn scan_enforces_snapshot_and_tablet_bounds_and_skips_deleted_rows() {
    let mut tablet = MemoryTablet::new(descriptor());
    let write = tablet.prepare_write(1, 0, vec![put(b"b", b"one"), put(b"c", b"two")]).unwrap();
    tablet.commit(write).unwrap();
    let write = tablet.prepare_write(1, 1, vec![TabletMutation::Delete { key: b"b".to_vec() }]).unwrap();
    tablet.commit(write).unwrap();

    let range = KeyRange::new(b"b".to_vec(), Some(b"c".to_vec())).unwrap();
    assert_eq!(collect_memory(&tablet, &range, 1, 1), vec![(b"b".to_vec(), b"one".to_vec())]);
    assert!(collect_memory(&tablet, &range, 2, 1).is_empty());

    let future = tablet.scan_range_at(&range, 3, size(4)).unwrap_err();
    assert_eq!(future, TabletError::SnapshotAhead { committed: 2, requested: 3 });

    let before_tablet = KeyRange::new(b"0".to_vec(), Some(b"b".to_vec())).unwrap();
    assert_eq!(
        tablet.scan_range_at(&before_tablet, 2, size(4)).unwrap_err(),
        TabletError::KeyOutsideTabletRange
    );
    let past_tablet = KeyRange::new(b"y".to_vec(), Some(b"zz".to_vec())).unwrap();
    assert_eq!(
        tablet.scan_range_at(&past_tablet, 2, size(4)).unwrap_err(),
        TabletError::KeyOutsideTabletRange
    );
    let unbounded = KeyRange::new(b"b".to_vec(), None).unwrap();
    assert_eq!(
        tablet.scan_range_at(&unbounded, 2, size(4)).unwrap_err(),
        TabletError::KeyOutsideTabletRange
    );
}

#[test]
fn wal_backed_scans_are_identical_after_checkpoint_and_reopen() {
    let path: PathBuf = std::env::temp_dir().join(format!(
        "nulang_nudb_batch_scan_{}_{}.wal",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_file(&path);
    let range = KeyRange::new(b"b".to_vec(), Some(b"n".to_vec())).unwrap();

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        let first = tablet.prepare_write(1, 0, vec![put(b"b", b"old"), put(b"f", b"keep"), put(b"n", b"excluded")]).unwrap();
        tablet.commit(first).unwrap();
        let second = tablet.prepare_write(1, 1, vec![put(b"b", b"new"), TabletMutation::Delete { key: b"f".to_vec() }, put(b"k", b"created")]).unwrap();
        tablet.commit(second).unwrap();

        let at_one: Vec<_> = tablet.scan_range_at(&range, 1, size(1)).unwrap()
            .flat_map(|batch| batch.rows.into_iter().map(|row| (row.key.to_vec(), row.value.to_vec())))
            .collect();
        assert_eq!(at_one, vec![(b"b".to_vec(), b"old".to_vec()), (b"f".to_vec(), b"keep".to_vec())]);
        tablet.checkpoint().unwrap();
    }

    let tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let at_one: Vec<_> = tablet.scan_range_at(&range, 1, size(2)).unwrap()
        .flat_map(|batch| batch.rows.into_iter().map(|row| (row.key.to_vec(), row.value.to_vec())))
        .collect();
    assert_eq!(at_one, vec![(b"b".to_vec(), b"old".to_vec()), (b"f".to_vec(), b"keep".to_vec())]);
    let at_two: Vec<_> = tablet.scan_range_at(&range, 2, size(1)).unwrap()
        .flat_map(|batch| batch.rows.into_iter().map(|row| (row.key.to_vec(), row.value.to_vec())))
        .collect();
    assert_eq!(at_two, vec![(b"b".to_vec(), b"new".to_vec()), (b"k".to_vec(), b"created".to_vec())]);
    drop(tablet);

    let _ = fs::remove_file(&path);
    // Checkpoint path is not a test-wide shared path; remove its sidecar too.
    let _ = fs::remove_file(path.with_extension("checkpoint"));
}
