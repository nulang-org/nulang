//! Safety contracts for bounded WAL group commit.
//!
//! A successful batch writes individually replayable transactions through one
//! WAL durability barrier. An I/O failure is ambiguous: callers must reopen
//! and retry with their stable transaction IDs, never blindly append again.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TabletWrite, TransactionId,
};
use nulang::database::wal::FileWal;

static NEXT: AtomicU64 = AtomicU64::new(1);
const EPOCH: u64 = 3;

struct TestWal {
    dir: PathBuf,
    path: PathBuf,
}
impl TestWal {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nulang_nudb_group_commit_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("tablet.wal");
        Self { dir, path }
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
        TabletId::new(107).unwrap(),
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
fn group_commit_retains_distinct_receipts_and_mvcc_snapshots_after_reopen() {
    let fixture = TestWal::new();
    let entries = vec![
        (id(1), vec![put(b"k", b"first")]),
        (id(2), vec![put(b"k", b"second"), put(b"m", b"other")]),
        (id(3), vec![TabletMutation::Delete { key: b"k".to_vec() }]),
    ];
    {
        let mut tablet = fixture.open();
        let receipts = tablet.commit_batch_idempotent(entries.clone()).unwrap();
        assert_eq!(
            receipts.iter().map(|r| r.sequence()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"first"[..]));
        assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"second"[..]));
        assert_eq!(tablet.read_at(b"k", 3).unwrap(), None);
        tablet.checkpoint().unwrap();
    }
    let mut tablet = fixture.open();
    assert_eq!(tablet.current_sequence(), 3);
    let retry = tablet.commit_batch_idempotent(entries).unwrap();
    assert_eq!(
        retry.iter().map(|r| r.sequence()).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        tablet.current_sequence(),
        3,
        "retries must not duplicate writes"
    );
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"first"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"second"[..]));
}

#[test]
fn preflight_rejects_duplicate_ids_conflicts_and_out_of_range_keys() {
    let fixture = TestWal::new();
    let mut tablet = fixture.open();
    assert_eq!(
        tablet
            .commit_batch_idempotent(vec![
                (id(1), vec![put(b"k", b"a")]),
                (id(1), vec![put(b"m", b"b")]),
            ])
            .unwrap_err(),
        WalBackedError::DuplicateBatchId(id(1))
    );
    assert_eq!(tablet.current_sequence(), 0);

    assert!(tablet
        .commit_batch_idempotent(vec![
            (id(2), vec![put(b"k", b"good")]),
            (id(3), vec![put(b"z", b"out of range")]),
        ])
        .is_err());
    assert_eq!(tablet.current_sequence(), 0);
    assert_eq!(tablet.read_latest(b"k"), None);

    assert_eq!(
        tablet
            .commit_batch_idempotent(vec![(id(2), vec![put(b"k", b"good")])])
            .unwrap()[0]
            .sequence(),
        1
    );
    assert_eq!(
        tablet
            .commit_batch_idempotent(vec![(id(2), vec![put(b"k", b"conflict")])])
            .unwrap_err(),
        WalBackedError::IdempotencyConflict(id(2))
    );
    assert_eq!(tablet.current_sequence(), 1);
}

#[test]
fn mixed_retry_and_new_writes_preserve_input_receipt_order() {
    let fixture = TestWal::new();
    let mut tablet = fixture.open();
    assert_eq!(
        tablet
            .commit_idempotent(id(1), vec![put(b"k", b"a")])
            .unwrap()
            .sequence(),
        1
    );
    let receipts = tablet
        .commit_batch_idempotent(vec![
            (id(2), vec![put(b"k", b"b")]),
            (id(1), vec![put(b"k", b"a")]),
            (id(3), vec![put(b"m", b"c")]),
        ])
        .unwrap();
    assert_eq!(
        receipts.iter().map(|r| r.sequence()).collect::<Vec<_>>(),
        vec![2, 1, 3]
    );
    assert_eq!(tablet.current_sequence(), 3);
}

#[test]
fn malformed_batch_sequence_is_rejected_before_wal_mutation() {
    let fixture = TestWal::new();
    let mut wal = FileWal::open(&fixture.path).unwrap();
    let first = TabletWrite::prepare(&descriptor(), EPOCH, 0, 0, vec![put(b"k", b"one")]).unwrap();
    let stale = TabletWrite::prepare(&descriptor(), EPOCH, 0, 0, vec![put(b"k", b"two")]).unwrap();

    assert!(wal.append_batch(&[first, stale]).is_err());
    assert_eq!(wal.last_sequence(), 0);
    assert_eq!(wal.records().len(), 0);
    drop(wal);

    let wal = FileWal::open(&fixture.path).unwrap();
    assert_eq!(wal.last_sequence(), 0);
}

#[test]
fn oversized_record_count_is_rejected_without_advancing_the_wal() {
    let fixture = TestWal::new();
    let mut wal = FileWal::open(&fixture.path).unwrap();
    let writes: Vec<TabletWrite> = (0..65)
        .map(|sequence| {
            TabletWrite::prepare(
                &descriptor(),
                EPOCH,
                sequence,
                sequence,
                vec![put(b"k", b"small")],
            )
            .unwrap()
        })
        .collect();

    assert_eq!(
        wal.append_batch(&writes).unwrap_err(),
        nulang::database::wal::WalError::BatchTooLarge {
            records: 65,
            bytes: 0,
        }
    );
    assert_eq!(wal.last_sequence(), 0);
    assert!(wal.records().is_empty());
}
