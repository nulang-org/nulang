//! Point-key optimistic transactions on the canonical fenced NuDB WAL.
//! Concurrent preparation must not force unrelated writers to conflict.
//! This contract intentionally excludes range/predicate reads and multi-tablet
//! serializability.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation, TransactionId,
};

static NEXT: AtomicU64 = AtomicU64::new(1);
const EPOCH: u64 = 3;

struct TestWal {
    dir: PathBuf,
    path: PathBuf,
}
impl TestWal {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nulang_nudb_occ_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        }
        #[cfg(not(unix))]
        fs::create_dir(&dir).unwrap();
        Self {
            path: dir.join("tablet.wal"),
            dir,
        }
    }
    fn open(&self) -> WalBackedTablet {
        WalBackedTablet::open(descriptor(), &self.path).unwrap()
    }
}
impl Drop for TestWal {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(707).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        EPOCH,
    )
    .unwrap()
}
fn id(n: u8) -> TransactionId {
    TransactionId::new([n; 16])
}
fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

#[test]
fn unrelated_writers_from_one_snapshot_both_commit_durably() {
    let fixture = TestWal::new();
    {
        let mut db = fixture.open();
        let snap = db.current_sequence();
        let first = db
            .prepare_optimistic_write(EPOCH, snap, vec![], vec![put(b"k", b"first")])
            .unwrap();
        let second = db
            .prepare_optimistic_write(EPOCH, snap, vec![], vec![put(b"m", b"second")])
            .unwrap();
        assert_eq!(
            db.commit_optimistic_idempotent(id(1), first)
                .unwrap()
                .sequence(),
            1
        );
        assert_eq!(
            db.commit_optimistic_idempotent(id(2), second)
                .unwrap()
                .sequence(),
            2
        );
        assert_eq!(db.read_at(b"k", 1).unwrap(), Some(&b"first"[..]));
        assert_eq!(db.read_at(b"m", 1).unwrap(), None);
    }
    let db = fixture.open();
    assert_eq!(db.current_sequence(), 2);
    assert_eq!(db.read_latest(b"k"), Some(&b"first"[..]));
    assert_eq!(db.read_latest(b"m"), Some(&b"second"[..]));
}

#[test]
fn same_key_updates_conflict_without_advancing_wal() {
    let fixture = TestWal::new();
    let mut db = fixture.open();
    let snap = db.current_sequence();
    let first = db
        .prepare_optimistic_write(EPOCH, snap, vec![], vec![put(b"k", b"first")])
        .unwrap();
    let second = db
        .prepare_optimistic_write(EPOCH, snap, vec![], vec![put(b"k", b"lost")])
        .unwrap();
    db.commit_optimistic_idempotent(id(3), first).unwrap();
    assert_eq!(
        db.commit_optimistic_idempotent(id(4), second).unwrap_err(),
        WalBackedError::Tablet(TabletError::OptimisticConflict {
            key: b"k".to_vec(),
            snapshot: snap,
            committed: 1
        })
    );
    assert_eq!(db.current_sequence(), 1);
    drop(db);
    let db = fixture.open();
    assert_eq!(db.current_sequence(), 1);
    assert_eq!(db.read_latest(b"k"), Some(&b"first"[..]));
}

#[test]
fn point_read_sets_prevent_classic_write_skew() {
    let fixture = TestWal::new();
    let mut db = fixture.open();
    db.commit_idempotent(id(5), vec![put(b"x", b"on"), put(b"y", b"on")])
        .unwrap();
    let snap = db.current_sequence();
    let tx_a = db
        .prepare_optimistic_write(EPOCH, snap, vec![b"x".to_vec()], vec![put(b"y", b"off")])
        .unwrap();
    let tx_b = db
        .prepare_optimistic_write(EPOCH, snap, vec![b"y".to_vec()], vec![put(b"x", b"off")])
        .unwrap();
    db.commit_optimistic_idempotent(id(6), tx_a).unwrap();
    assert!(matches!(
        db.commit_optimistic_idempotent(id(7), tx_b),
        Err(WalBackedError::Tablet(
            TabletError::OptimisticConflict { .. }
        ))
    ));
    assert_eq!(db.read_latest(b"x"), Some(&b"on"[..]));
    assert_eq!(db.read_latest(b"y"), Some(&b"off"[..]));
    assert_eq!(db.current_sequence(), 2);
}

#[test]
fn creation_of_previously_absent_read_key_conflicts() {
    let fixture = TestWal::new();
    let mut db = fixture.open();
    let snap = db.current_sequence();
    assert_eq!(db.read_at(b"x", snap).unwrap(), None);
    let pending = db
        .prepare_optimistic_write(EPOCH, snap, vec![b"x".to_vec()], vec![put(b"y", b"value")])
        .unwrap();
    db.commit_idempotent(id(8), vec![put(b"x", b"appeared")])
        .unwrap();
    assert!(matches!(
        db.commit_optimistic_idempotent(id(9), pending),
        Err(WalBackedError::Tablet(
            TabletError::OptimisticConflict { .. }
        ))
    ));
    assert_eq!(db.read_latest(b"y"), None);
}

#[test]
fn preparation_rejects_future_snapshots_invalid_epochs_and_read_ranges() {
    let fixture = TestWal::new();
    let db = fixture.open();
    assert!(matches!(
        db.prepare_optimistic_write(EPOCH, 1, vec![], vec![put(b"m", b"v")]),
        Err(TabletError::SnapshotAhead { .. })
    ));
    assert!(matches!(
        db.prepare_optimistic_write(EPOCH - 1, 0, vec![], vec![put(b"m", b"v")]),
        Err(TabletError::StaleEpoch { .. })
    ));
    assert!(matches!(
        db.prepare_optimistic_write(EPOCH, 0, vec![b"z".to_vec()], vec![put(b"m", b"v")]),
        Err(TabletError::KeyOutsideTabletRange)
    ));
    assert_eq!(db.current_sequence(), 0);
}

#[test]
fn idempotent_retry_survives_checkpoint_and_newer_conflicting_history() {
    let fixture = TestWal::new();
    let pending;
    {
        let mut db = fixture.open();
        pending = db
            .prepare_optimistic_write(EPOCH, 0, vec![], vec![put(b"k", b"first")])
            .unwrap();
        assert_eq!(
            db.commit_optimistic_idempotent(id(10), pending.clone())
                .unwrap()
                .sequence(),
            1
        );
        db.checkpoint().unwrap();
    }
    let mut db = fixture.open();
    db.commit_idempotent(id(11), vec![put(b"k", b"newer")])
        .unwrap();
    assert_eq!(
        db.commit_optimistic_idempotent(id(10), pending.clone())
            .unwrap()
            .sequence(),
        1
    );
    assert_eq!(db.current_sequence(), 2, "replay cannot append again");
    assert_eq!(db.read_latest(b"k"), Some(&b"newer"[..]));
    let different = db
        .prepare_optimistic_write(EPOCH, 2, vec![], vec![put(b"m", b"other")])
        .unwrap();
    assert_eq!(
        db.commit_optimistic_idempotent(id(10), different)
            .unwrap_err(),
        WalBackedError::IdempotencyConflict(id(10))
    );
}

#[test]
fn deleted_key_still_conflicts_even_when_latest_read_is_absent() {
    let fixture = TestWal::new();
    let mut db = fixture.open();
    let pending = db
        .prepare_optimistic_write(EPOCH, 0, vec![b"x".to_vec()], vec![put(b"y", b"attempt")])
        .unwrap();
    db.commit_idempotent(id(12), vec![put(b"x", b"temporary")])
        .unwrap();
    db.commit_idempotent(id(13), vec![TabletMutation::Delete { key: b"x".to_vec() }])
        .unwrap();
    assert_eq!(db.read_latest(b"x"), None);
    assert_eq!(
        db.commit_optimistic_idempotent(id(14), pending)
            .unwrap_err(),
        WalBackedError::Tablet(TabletError::OptimisticConflict {
            key: b"x".to_vec(),
            snapshot: 0,
            committed: 2
        })
    );
    assert_eq!(db.read_latest(b"y"), None);
    assert_eq!(db.current_sequence(), 2);
}

#[test]
fn read_set_admission_is_bounded_and_wrong_tablet_writes_fail_closed() {
    use nulang::database::optimistic::MAX_POINT_READ_KEYS;
    use nulang::database::tablet::MemoryTablet;

    let mut db = MemoryTablet::new(descriptor());
    assert_eq!(
        db.prepare_optimistic_write(
            EPOCH,
            0,
            vec![b"k".to_vec(); MAX_POINT_READ_KEYS + 1],
            vec![put(b"x", b"new")],
        )
        .unwrap_err(),
        TabletError::TooManyOptimisticReadKeys {
            requested: MAX_POINT_READ_KEYS + 1,
            limit: MAX_POINT_READ_KEYS,
        }
    );
    let foreign_descriptor = TabletDescriptor::new(
        TabletId::new(708).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        EPOCH,
    )
    .unwrap();
    let foreign = MemoryTablet::new(foreign_descriptor);
    let pending = foreign
        .prepare_optimistic_write(EPOCH, 0, vec![], vec![put(b"x", b"wrong")])
        .unwrap();
    assert_eq!(
        db.commit_optimistic_write(pending).unwrap_err(),
        TabletError::WrongTablet {
            expected: TabletId::new(707).unwrap(),
            presented: TabletId::new(708).unwrap(),
        }
    );
    assert_eq!(db.current_sequence(), 0);
}

#[test]
fn independent_writes_from_many_threads_preserve_every_durable_result() {
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    const WRITERS: usize = 16;
    let fixture = TestWal::new();
    let db = Arc::new(Mutex::new(fixture.open()));
    let start = Arc::new(Barrier::new(WRITERS));
    let mut handles = Vec::new();
    for i in 0..WRITERS {
        let key = format!("key_{i:02}").into_bytes();
        let pending = db
            .lock()
            .unwrap()
            .prepare_optimistic_write(EPOCH, 0, vec![], vec![put(&key, b"value")])
            .unwrap();
        let db = Arc::clone(&db);
        let start = Arc::clone(&start);
        handles.push(thread::spawn(move || {
            start.wait();
            db.lock()
                .unwrap()
                .commit_optimistic_idempotent(id(20 + i as u8), pending)
                .unwrap()
                .sequence()
        }));
    }
    let mut seqs = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=WRITERS as u64).collect::<Vec<_>>());
    assert_eq!(db.lock().unwrap().current_sequence(), WRITERS as u64);
    drop(db);
    let recovered = fixture.open();
    for i in 0..WRITERS {
        assert_eq!(
            recovered.read_latest(format!("key_{i:02}").as_bytes()),
            Some(&b"value"[..])
        );
    }
}

#[test]
fn retry_receipts_must_not_bypass_tablet_epoch_or_snapshot_fences() {
    use nulang::database::tablet::MemoryTablet;

    let fixture = TestWal::new();
    let mut db = fixture.open();
    let mutations = vec![put(b"k", b"original")];
    let original = db
        .prepare_optimistic_write(EPOCH, 0, vec![], mutations.clone())
        .unwrap();
    assert_eq!(
        db.commit_optimistic_idempotent(id(100), original)
            .unwrap()
            .sequence(),
        1
    );

    let wrong_descriptor = TabletDescriptor::new(
        TabletId::new(708).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        EPOCH,
    )
    .unwrap();
    let wrong = MemoryTablet::new(wrong_descriptor)
        .prepare_optimistic_write(EPOCH, 0, vec![], mutations.clone())
        .unwrap();
    assert_eq!(
        db.commit_optimistic_idempotent(id(100), wrong).unwrap_err(),
        WalBackedError::Tablet(TabletError::WrongTablet {
            expected: TabletId::new(707).unwrap(),
            presented: TabletId::new(708).unwrap(),
        }),
        "same ID and mutations cannot skip tablet routing validation"
    );

    let stale_descriptor = TabletDescriptor::new(
        TabletId::new(707).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        EPOCH - 1,
    )
    .unwrap();
    let stale = MemoryTablet::new(stale_descriptor)
        .prepare_optimistic_write(EPOCH - 1, 0, vec![], mutations.clone())
        .unwrap();
    assert_eq!(
        db.commit_optimistic_idempotent(id(100), stale).unwrap_err(),
        WalBackedError::Tablet(TabletError::StaleEpoch {
            current: EPOCH,
            presented: EPOCH - 1,
        }),
        "receipt reuse must not bypass the current owner epoch"
    );

    let mut ahead_tablet = MemoryTablet::new(descriptor());
    for seq in 0..2 {
        let write = ahead_tablet
            .prepare_write(EPOCH, seq, vec![put(b"x", b"unrelated")])
            .unwrap();
        ahead_tablet.commit(write).unwrap();
    }
    let future = ahead_tablet
        .prepare_optimistic_write(EPOCH, 2, vec![], mutations)
        .unwrap();
    assert_eq!(
        db.commit_optimistic_idempotent(id(100), future)
            .unwrap_err(),
        WalBackedError::Tablet(TabletError::SnapshotAhead {
            committed: 1,
            requested: 2,
        })
    );
    assert_eq!(db.current_sequence(), 1);
    assert_eq!(db.read_latest(b"k"), Some(&b"original"[..]));
}

#[test]
fn tracked_point_reads_prevent_write_skew_without_manual_read_keys() {
    let fixture = TestWal::new();
    let mut db = fixture.open();
    db.commit_idempotent(id(101), vec![put(b"x", b"on"), put(b"y", b"on")])
        .unwrap();

    let mut first = db.begin_point_transaction();
    let mut second = db.begin_point_transaction();
    assert_eq!(
        db.read_point(&mut first, b"x").unwrap(),
        Some(b"on".to_vec())
    );
    assert_eq!(
        db.read_point(&mut second, b"y").unwrap(),
        Some(b"on".to_vec())
    );
    first.put(b"y".to_vec(), b"off".to_vec()).unwrap();
    second.put(b"x".to_vec(), b"off".to_vec()).unwrap();

    assert_eq!(
        db.commit_point_transaction_idempotent(id(102), first)
            .unwrap()
            .sequence(),
        2
    );
    assert!(matches!(
        db.commit_point_transaction_idempotent(id(103), second),
        Err(WalBackedError::Tablet(TabletError::OptimisticConflict {
            key,
            snapshot: 1,
            committed: 2
        })) if key == b"y"
    ));
    assert_eq!(db.read_latest(b"x"), Some(&b"on"[..]));
    assert_eq!(db.read_latest(b"y"), Some(&b"off"[..]));
}

#[test]
fn tracked_absent_reads_detect_creation_and_own_writes_are_visible() {
    let fixture = TestWal::new();
    let mut db = fixture.open();
    let mut tx = db.begin_point_transaction();
    assert_eq!(db.read_point(&mut tx, b"missing").unwrap(), None);
    tx.put(b"x".to_vec(), b"one".to_vec()).unwrap();
    assert_eq!(db.read_point(&mut tx, b"x").unwrap(), Some(b"one".to_vec()));
    tx.delete(b"x".to_vec()).unwrap();
    assert_eq!(db.read_point(&mut tx, b"x").unwrap(), None);
    db.commit_idempotent(id(104), vec![put(b"missing", b"appeared")])
        .unwrap();
    assert!(matches!(
        db.commit_point_transaction_idempotent(id(105), tx),
        Err(WalBackedError::Tablet(TabletError::OptimisticConflict {
            key, committed: 1, ..
        })) if key == b"missing"
    ));
    assert_eq!(db.read_latest(b"x"), None);
}

#[test]
fn tracked_point_txn_rejects_foreign_tablets_and_stale_writes() {
    let fixture = TestWal::new();
    let db = fixture.open();
    let mut tx = db.begin_point_transaction();
    assert_eq!(
        db.read_point(&mut tx, b"z").unwrap_err(),
        TabletError::KeyOutsideTabletRange
    );
    assert_eq!(db.current_sequence(), 0);

    let foreign_descriptor = TabletDescriptor::new(
        TabletId::new(708).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        EPOCH,
    )
    .unwrap();
    let foreign = nulang::database::tablet::MemoryTablet::new(foreign_descriptor);
    assert_eq!(
        tx.read(&foreign, b"k").unwrap_err(),
        TabletError::WrongTablet {
            expected: TabletId::new(708).unwrap(),
            presented: TabletId::new(707).unwrap(),
        }
    );
    assert_eq!(
        tx.put(b"z".to_vec(), b"out-of-range".to_vec()).unwrap_err(),
        TabletError::KeyOutsideTabletRange
    );
}

#[test]
fn tracked_read_set_deduplicates_keys_and_bounds_distinct_reads() {
    use nulang::database::optimistic::MAX_POINT_READ_KEYS;

    let fixture = TestWal::new();
    let db = fixture.open();
    let mut tx = db.begin_point_transaction();
    for i in 0..MAX_POINT_READ_KEYS {
        let key = format!("key_{i:04}");
        assert_eq!(db.read_point(&mut tx, key.as_bytes()).unwrap(), None);
    }
    // A duplicate read does not exhaust a slot.
    assert_eq!(db.read_point(&mut tx, b"key_0000").unwrap(), None);
    assert_eq!(
        db.read_point(&mut tx, b"key_extra").unwrap_err(),
        TabletError::TooManyOptimisticReadKeys {
            requested: MAX_POINT_READ_KEYS + 1,
            limit: MAX_POINT_READ_KEYS,
        }
    );
}
