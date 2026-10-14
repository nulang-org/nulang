//! NuDB batch-read / MVCC-retention integration contract.
//! This test is intentionally based on both the retention and batch-scan PRs.

use std::fs;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletError, TabletId, TabletMutation,
};

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(193).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn scan_range() -> KeyRange {
    KeyRange::new(b"b".to_vec(), Some(b"m".to_vec())).unwrap()
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn duplicate_key_history() -> [Vec<TabletMutation>; 2] {
    [
        vec![put(b"b", b"v1"), put(b"c", b"present"), put(b"d", b"retained")],
        vec![
            put(b"b", b"intermediate"),
            TabletMutation::Delete { key: b"b".to_vec() },
            put(b"b", b"v2"),
            TabletMutation::Delete { key: b"c".to_vec() },
            put(b"e", b"new"),
        ],
    ]
}

fn expected_snapshot_two() -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![
        (b"b".to_vec(), b"v2".to_vec()),
        (b"d".to_vec(), b"retained".to_vec()),
        (b"e".to_vec(), b"new".to_vec()),
    ]
}

#[test]
fn memory_batch_scan_observes_retention_floor_and_last_mutation_wins() {
    let mut tablet = MemoryTablet::new(descriptor());
    for mutations in duplicate_key_history() {
        let prior = tablet.current_sequence();
        let write = tablet.prepare_write(1, prior, mutations).unwrap();
        tablet.commit(write).unwrap();
    }

    let range = scan_range();
    let original: Vec<_> = tablet
        .scan_range_at(&range, 1, NonZeroUsize::new(1).unwrap())
        .unwrap()
        .flat_map(|batch| batch.rows.into_iter().map(|row| (row.key.to_vec(), row.value.to_vec())))
        .collect();
    assert_eq!(
        original,
        vec![
            (b"b".to_vec(), b"v1".to_vec()),
            (b"c".to_vec(), b"present".to_vec()),
            (b"d".to_vec(), b"retained".to_vec()),
        ]
    );

    tablet.prune_history_before(2).unwrap();
    assert_eq!(
        tablet.scan_range_at(&range, 1, NonZeroUsize::new(2).unwrap()).unwrap_err(),
        TabletError::SnapshotExpired { requested: 1, oldest_retained: 2 },
    );
    let latest: Vec<_> = tablet
        .scan_range_at(&range, 2, NonZeroUsize::new(2).unwrap())
        .unwrap()
        .flat_map(|batch| batch.rows.into_iter().map(|row| (row.key.to_vec(), row.value.to_vec())))
        .collect();
    assert_eq!(latest, expected_snapshot_two());
}

#[test]
fn wal_backed_batch_scan_retention_survives_reopen_and_rejects_expired_history() {
    let wal_path: PathBuf = std::env::temp_dir().join(format!(
        "nulang_nudb_batch_retention_{}_{}.wal",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(wal_path.with_extension("checkpoint"));
    let range = scan_range();

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        for mutations in duplicate_key_history() {
            let prior = tablet.current_sequence();
            let write = tablet.prepare_write(1, prior, mutations).unwrap();
            tablet.commit(write).unwrap();
        }
        tablet.reclaim_history_before(2).unwrap();
        assert_eq!(tablet.oldest_retained_snapshot(), 2);
        assert_eq!(
            tablet.scan_range_at(&range, 1, NonZeroUsize::new(8).unwrap()).unwrap_err(),
            TabletError::SnapshotExpired { requested: 1, oldest_retained: 2 },
        );
    }

    let reopened = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(reopened.oldest_retained_snapshot(), 2);
    assert_eq!(
        reopened.scan_range_at(&range, 1, NonZeroUsize::new(8).unwrap()).unwrap_err(),
        TabletError::SnapshotExpired { requested: 1, oldest_retained: 2 },
    );
    let visible: Vec<_> = reopened
        .scan_range_at(&range, 2, NonZeroUsize::new(1).unwrap())
        .unwrap()
        .flat_map(|batch| batch.rows.into_iter().map(|row| (row.key.to_vec(), row.value.to_vec())))
        .collect();
    assert_eq!(visible, expected_snapshot_two());
    drop(reopened);

    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(wal_path.with_extension("checkpoint"));
}
