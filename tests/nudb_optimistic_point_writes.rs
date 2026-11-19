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
