//! Contract tests for the Bigtable-style NuDB read/split preparation seam.
//! These tests intentionally use the ordinary local storage loops, not actors.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletError, TabletId, TabletMutation, TabletScanRow,
};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(51).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        4,
    )
    .unwrap()
}

fn commit(tablet: &mut MemoryTablet, mutations: Vec<TabletMutation>) {
    let write = tablet
        .prepare_write(4, tablet.current_sequence(), mutations)
        .unwrap();
    tablet.commit(write).unwrap();
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn row(key: &[u8], value: &[u8]) -> TabletScanRow {
    TabletScanRow {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

#[test]
fn scan_at_preserves_historical_values_tombstones_order_and_limit() {
    let mut tablet = MemoryTablet::new(descriptor());
    commit(&mut tablet, vec![put(b"b", b"old"), put(b"m", b"one"), put(b"t", b"t1")]);
    commit(
        &mut tablet,
        vec![
            put(b"b", b"new"),
            TabletMutation::Delete { key: b"m".to_vec() },
            put(b"u", b"u2"),
        ],
    );

    let snapshot_one = tablet.scan_at(b"a", Some(b"z"), 1, 10).unwrap();
    assert_eq!(
        snapshot_one,
        vec![row(b"b", b"old"), row(b"m", b"one"), row(b"t", b"t1")]
    );
    assert_eq!(
        tablet.scan_at(b"a", Some(b"z"), 2, 10).unwrap(),
        vec![row(b"b", b"new"), row(b"t", b"t1"), row(b"u", b"u2")]
    );
    assert_eq!(
        tablet.scan_at(b"m", Some(b"u"), 2, 10).unwrap(),
        vec![row(b"t", b"t1")]
    );
    assert_eq!(
        tablet.scan_at(b"a", Some(b"z"), 2, 1).unwrap(),
        vec![row(b"b", b"new")]
    );
    assert!(tablet.scan_at(b"a", Some(b"z"), 2, 0).unwrap().is_empty());

    commit(&mut tablet, vec![put(b"b", b"newer")]);
    assert_eq!(snapshot_one[0].value.as_slice(), b"old");
    assert_eq!(
        tablet.scan_at(b"a", Some(b"z"), 1, 10).unwrap(),
        snapshot_one
    );
}

#[test]
fn scan_at_rejects_invalid_snapshots_and_cross_tablet_ranges() {
    let tablet = MemoryTablet::new(descriptor());
    assert_eq!(
        tablet.scan_at(b"a", Some(b"z"), 1, 10).unwrap_err(),
        TabletError::SnapshotAhead { committed: 0, requested: 1 }
    );
    assert_eq!(
        tablet.scan_at(b"m", Some(b"m"), 0, 10).unwrap_err(),
        TabletError::InvalidScanBounds
    );
    assert_eq!(
        tablet.scan_at(b"m", Some(b"b"), 0, 10).unwrap_err(),
        TabletError::InvalidScanBounds
    );
    for (start, end) in [
        (&b"0"[..], Some(&b"m"[..])),
        (&b"a"[..], Some(&b"zz"[..])),
        (&b"a"[..], None),
        (&b"z"[..], Some(&b"zz"[..])),
    ] {
        assert_eq!(
            tablet.scan_at(start, end, 0, 10).unwrap_err(),
            TabletError::KeyOutsideTabletRange
        );
    }
}

#[test]
fn split_materialization_preserves_mvcc_history_and_rejects_stale_child_epochs() {
    let mut source = MemoryTablet::new(descriptor());
    commit(&mut source, vec![put(b"a", b"a1"), put(b"m", b"m1"), put(b"y", b"y1")]);
    commit(
        &mut source,
        vec![TabletMutation::Delete { key: b"a".to_vec() }, put(b"m", b"m2")],
    );

    let plan = descriptor()
        .plan_split(b"m", TabletId::new(52).unwrap(), TabletId::new(53).unwrap(), 5)
        .unwrap();
    let (mut left, right) = source.materialize_split(&plan).unwrap();

    assert_eq!(left.current_sequence(), 2);
    assert_eq!(right.current_sequence(), 2);
    assert_eq!(left.read_at(b"a", 1).unwrap(), Some(&b"a1"[..]));
    assert_eq!(left.read_at(b"a", 2).unwrap(), None);
    assert_eq!(right.read_at(b"m", 1).unwrap(), Some(&b"m1"[..]));
    assert_eq!(right.read_at(b"m", 2).unwrap(), Some(&b"m2"[..]));
    assert_eq!(right.read_at(b"y", 2).unwrap(), Some(&b"y1"[..]));
    assert_eq!(left.read_at(b"m", 2).unwrap_err(), TabletError::KeyOutsideTabletRange);
    assert_eq!(right.read_at(b"a", 2).unwrap_err(), TabletError::KeyOutsideTabletRange);
    assert_eq!(left.scan_at(b"a", Some(b"m"), 1, 10).unwrap(), vec![row(b"a", b"a1")]);
    assert_eq!(
        right.scan_at(b"m", Some(b"z"), 1, 10).unwrap(),
        vec![row(b"m", b"m1"), row(b"y", b"y1")]
    );

    assert_eq!(
        left.prepare_write(4, 2, vec![put(b"b", b"stale")]).unwrap_err(),
        TabletError::StaleEpoch { current: 5, presented: 4 }
    );
    let write = left.prepare_write(5, 2, vec![put(b"b", b"new")]).unwrap();
    left.commit(write).unwrap();
    assert_eq!(left.current_sequence(), 3);
    assert_eq!(right.current_sequence(), 2);
    assert_eq!(source.current_sequence(), 2);
    assert_eq!(source.read_at(b"b", 2).unwrap(), None);

    let mut invalid_plan = plan.clone();
    invalid_plan.right = plan.left.clone();
    assert_eq!(
        source.materialize_split(&invalid_plan).unwrap_err(),
        TabletError::SplitPlanMismatch
    );
}

#[test]
fn wal_recovery_and_checkpoint_keep_historical_scans_stable() {
    let wal_path: PathBuf = std::env::temp_dir().join(format!(
        "nulang_nudb_scan_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    let checkpoint = wal_path.with_extension("checkpoint");
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        let first = tablet
            .prepare_write(4, 0, vec![put(b"b", b"first"), put(b"m", b"first")])
            .unwrap();
        tablet.commit(first).unwrap();
        let second = tablet
            .prepare_write(
                4,
                1,
                vec![
                    put(b"b", b"second"),
                    TabletMutation::Delete { key: b"m".to_vec() },
                ],
            )
            .unwrap();
        tablet.commit(second).unwrap();
        tablet.checkpoint().unwrap();
        let third = tablet
            .prepare_write(4, 2, vec![put(b"y", b"third")])
            .unwrap();
        tablet.commit(third).unwrap();
    }

    let reopened = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(
        reopened.scan_at(b"a", Some(b"z"), 1, 10).unwrap(),
        vec![row(b"b", b"first"), row(b"m", b"first")]
    );
    assert_eq!(
        reopened.scan_at(b"a", Some(b"z"), 3, 10).unwrap(),
        vec![row(b"b", b"second"), row(b"y", b"third")]
    );

    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);
}

#[test]
fn repeated_mutations_to_one_key_restore_as_one_committed_mvcc_version() {
    let wal_path = std::env::temp_dir().join(format!(
        "nulang_nudb_duplicate_keys_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    let checkpoint = wal_path.with_extension("checkpoint");
    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        let initial = tablet.prepare_write(4, 0, vec![put(b"b", b"first")]).unwrap();
        tablet.commit(initial).unwrap();
        let repeated = tablet
            .prepare_write(
                4,
                1,
                vec![
                    put(b"b", b"temporary"),
                    TabletMutation::Delete { key: b"b".to_vec() },
                    put(b"b", b"final"),
                ],
            )
            .unwrap();
        tablet.commit(repeated).unwrap();
        assert_eq!(tablet.read_at(b"b", 2).unwrap(), Some(&b"final"[..]));
        tablet.checkpoint().unwrap();
    }

    // A checkpoint must not contain duplicate versions at the same commit
    // sequence: they make restore_snapshot reject the complete history.
    let reopened = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(reopened.read_at(b"b", 1).unwrap(), Some(&b"first"[..]));
    assert_eq!(reopened.read_at(b"b", 2).unwrap(), Some(&b"final"[..]));
    assert_eq!(
        reopened.scan_at(b"a", Some(b"z"), 2, 10).unwrap(),
        vec![row(b"b", b"final")]
    );

    let _ = fs::remove_file(&wal_path);
    let _ = fs::remove_file(&checkpoint);
}
