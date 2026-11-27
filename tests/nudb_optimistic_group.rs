//! Safety contracts for bounded WAL group commit.
//!
//! A successful batch writes individually replayable transactions through one
//! WAL durability barrier. An I/O failure is ambiguous: callers must reopen
//! and retry with their stable transaction IDs, never blindly append again.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::optimistic::OptimisticTabletWrite;
use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::TabletError;
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};
use nulang::database::wal::WalError;

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

fn prepare(
    tablet: &WalBackedTablet,
    reads: Vec<Vec<u8>>,
    mutations: Vec<TabletMutation>,
) -> OptimisticTabletWrite {
    tablet
        .prepare_optimistic_write(EPOCH, tablet.current_sequence(), reads, mutations)
        .unwrap()
}

#[test]
fn disjoint_group_commits_and_retries_in_input_order_after_reopen() {
    let fixture = TestWal::new();
    let mut tablet = fixture.open();
    let entries = vec![
        (id(1), prepare(&tablet, vec![], vec![put(b"a", b"one")])),
        (
            id(2),
            prepare(&tablet, vec![b"c".to_vec()], vec![put(b"b", b"two")]),
        ),
    ];
    let receipts = tablet
        .commit_optimistic_batch_idempotent(entries.clone())
        .unwrap();
    assert_eq!(
        receipts.iter().map(|r| r.sequence()).collect::<Vec<_>>(),
        vec![1, 2]
    );
    tablet.reclaim_history_before(2).unwrap();
    drop(tablet);
    let mut tablet = fixture.open();
    let length = fs::metadata(&fixture.path).unwrap().len();
    let retry = tablet
        .commit_optimistic_batch_idempotent(entries.into_iter().rev().collect())
        .unwrap();
    assert_eq!(retry, vec![receipts[1], receipts[0]]);
    assert_eq!(fs::metadata(&fixture.path).unwrap().len(), length);
    assert_eq!(tablet.current_sequence(), 2);
}

#[test]
fn intra_group_write_and_read_conflicts_reject_before_wal_mutation() {
    let fixture = TestWal::new();
    let mut tablet = fixture.open();
    let length = fs::metadata(&fixture.path).unwrap().len();
    for (reads, key) in [(vec![], b"a"), (vec![b"a".to_vec()], b"b")] {
        let entries = vec![
            (
                id(1),
                prepare(
                    &tablet,
                    vec![],
                    vec![TabletMutation::Delete { key: b"a".to_vec() }],
                ),
            ),
            (id(2), prepare(&tablet, reads, vec![put(key, b"two")])),
        ];
        assert!(matches!(
            tablet.commit_optimistic_batch_idempotent(entries),
            Err(WalBackedError::Tablet(TabletError::OptimisticConflict {
                committed: 1,
                ..
            }))
        ));
        assert_eq!(tablet.current_sequence(), 0);
        assert_eq!(fs::metadata(&fixture.path).unwrap().len(), length);
    }
    assert_eq!(
        tablet
            .commit_idempotent(id(1), vec![put(b"a", b"ok")])
            .unwrap()
            .sequence(),
        1
    );
}

#[test]
fn replay_does_not_count_as_a_new_conflicting_write() {
    let fixture = TestWal::new();
    let mut tablet = fixture.open();
    let old = prepare(&tablet, vec![], vec![put(b"a", b"one")]);
    let receipt = tablet
        .commit_optimistic_idempotent(id(1), old.clone())
        .unwrap();
    let new = prepare(&tablet, vec![b"a".to_vec()], vec![put(b"a", b"two")]);
    let receipts = tablet
        .commit_optimistic_batch_idempotent(vec![(id(1), old), (id(2), new)])
        .unwrap();
    assert_eq!(receipts[0], receipt);
    assert_eq!(receipts[1].sequence(), 2);
    assert_eq!(tablet.read_latest(b"a"), Some(&b"two"[..]));
}

#[test]
fn duplicates_and_request_budget_fail_without_mutation() {
    let fixture = TestWal::new();
    let mut tablet = fixture.open();
    let length = fs::metadata(&fixture.path).unwrap().len();
    let p = prepare(&tablet, vec![], vec![put(b"a", b"one")]);
    assert!(matches!(
        tablet.commit_optimistic_batch_idempotent(vec![(id(1), p.clone()), (id(1), p.clone())]),
        Err(WalBackedError::DuplicateBatchId(_))
    ));
    assert!(matches!(
        tablet.commit_optimistic_batch_idempotent((1..=65).map(|i| (id(i), p.clone())).collect()),
        Err(WalBackedError::Wal(WalError::BatchTooLarge { .. }))
    ));
    let huge = prepare(&tablet, vec![vec![b'a'; 8 * 1024 * 1024]], vec![]);
    assert!(matches!(
        tablet.commit_optimistic_batch_idempotent(vec![(id(1), huge)]),
        Err(WalBackedError::Wal(WalError::BatchTooLarge { .. }))
    ));
    assert_eq!(fs::metadata(&fixture.path).unwrap().len(), length);
    assert_eq!(tablet.current_sequence(), 0);
}

#[test]
fn stale_new_write_fails_even_after_retention_and_tombstone_pruning() {
    let fixture = TestWal::new();
    let mut tablet = fixture.open();
    let stale = prepare(&tablet, vec![b"a".to_vec()], vec![put(b"b", b"two")]);
    tablet
        .commit_idempotent(id(1), vec![TabletMutation::Delete { key: b"a".to_vec() }])
        .unwrap();
    tablet.reclaim_history_before(1).unwrap();
    let length = fs::metadata(&fixture.path).unwrap().len();
    assert!(tablet
        .commit_optimistic_batch_idempotent(vec![(id(2), stale)])
        .is_err());
    assert_eq!(tablet.current_sequence(), 1);
    assert_eq!(fs::metadata(&fixture.path).unwrap().len(), length);
}

#[test]
fn encoded_frame_budget_rejects_before_publishing_any_group_member() {
    let fixture = TestWal::new();
    let mut tablet = fixture.open();
    let length = fs::metadata(&fixture.path).unwrap().len();
    let entries = vec![
        (id(1), prepare(&tablet, vec![], vec![put(b"a", b"small")])),
        (
            id(2),
            prepare(
                &tablet,
                vec![],
                vec![put(b"b", &vec![255; 3 * 1024 * 1024])],
            ),
        ),
    ];
    assert!(matches!(
        tablet.commit_optimistic_batch_idempotent(entries),
        Err(WalBackedError::Wal(WalError::BatchTooLarge { .. }))
    ));
    assert_eq!(tablet.current_sequence(), 0);
    assert_eq!(fs::metadata(&fixture.path).unwrap().len(), length);
    assert_eq!(
        tablet
            .commit_idempotent(id(1), vec![put(b"a", b"small")])
            .unwrap()
            .sequence(),
        1
    );
}

#[test]
fn conflicting_receipt_retry_rejects_entire_group() {
    let fixture = TestWal::new();
    let mut tablet = fixture.open();
    tablet
        .commit_idempotent(id(1), vec![put(b"a", b"original")])
        .unwrap();
    let length = fs::metadata(&fixture.path).unwrap().len();
    let entries = vec![
        (id(2), prepare(&tablet, vec![], vec![put(b"b", b"new")])),
        (
            id(1),
            prepare(&tablet, vec![], vec![put(b"a", b"different")]),
        ),
    ];
    assert_eq!(
        tablet
            .commit_optimistic_batch_idempotent(entries)
            .unwrap_err(),
        WalBackedError::IdempotencyConflict(id(1))
    );
    assert_eq!(tablet.current_sequence(), 1);
    assert_eq!(tablet.read_latest(b"b"), None);
    assert_eq!(fs::metadata(&fixture.path).unwrap().len(), length);
}
