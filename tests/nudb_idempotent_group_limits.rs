//! Additional boundary contracts for the bounded NuDB group-commit path.
//! These tests use real WAL files and verify that rejected groups leave the
//! same writer usable, without advancing the durable transaction chain.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};

static NEXT_CASE: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    dir: PathBuf,
    wal: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nulang_group_limits_{}_{}",
            std::process::id(),
            NEXT_CASE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let wal = dir.join("tablet.wal");
        Self { dir, wal }
    }

    fn open(&self) -> WalBackedTablet {
        WalBackedTablet::open(descriptor(), &self.wal).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(109).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        5,
    )
    .unwrap()
}

fn put(value: Vec<u8>) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: b"k".to_vec(),
        value,
    }]
}

fn id(i: u8) -> TransactionId {
    TransactionId::new([i; 16])
}

#[test]
fn exactly_sixty_four_transactions_commit_and_survive_checkpoint_recovery() {
    let fixture = Fixture::new();
    let mut tablet = fixture.open();
    let entries: Vec<_> = (1..=64_u8).map(|i| (id(i), put(vec![i]))).collect();

    let receipts = tablet.commit_batch_idempotent(entries.clone()).unwrap();
    assert_eq!(receipts.len(), 64);
    assert_eq!(
        receipts
            .iter()
            .map(|receipt| receipt.sequence())
            .collect::<Vec<_>>(),
        (1..=64).collect::<Vec<_>>()
    );
    assert_eq!(tablet.current_sequence(), 64);
    tablet.checkpoint().unwrap();
    drop(tablet);

    let mut tablet = fixture.open();
    assert_eq!(tablet.current_sequence(), 64);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&[1_u8][..]));
    assert_eq!(tablet.read_latest(b"k"), Some(&[64_u8][..]));
    let retry = tablet.commit_batch_idempotent(entries).unwrap();
    assert_eq!(retry, receipts);
    assert_eq!(tablet.current_sequence(), 64);
}

#[test]
fn a_retry_only_request_larger_than_the_group_limit_is_rejected_up_front() {
    let fixture = Fixture::new();
    let mut tablet = fixture.open();
    // Duplicate identifiers let us detect whether the input count is checked
    // *before* allocating a large seen-ID set and processing every item.
    let entries = (0..65).map(|_| (id(1), put(b"same".to_vec()))).collect();
    assert_eq!(
        tablet.commit_batch_idempotent(entries).unwrap_err(),
        WalBackedError::TooManyReceiptTransactions {
            requested: 65,
            limit: 64,
        }
    );
    assert_eq!(tablet.current_sequence(), 0);

    assert_eq!(
        tablet
            .commit_idempotent(id(2), put(b"safe".to_vec()))
            .unwrap()
            .sequence(),
        1
    );
}
