//! Explicit, fail-closed NuDB WAL append admission budget; no automatic reclaim.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::retention::RetentionClass;
use nulang::database::retention_store::DurableRetentionRegistry;
use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::WalError;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TempWal(PathBuf);
impl TempWal {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nudb-budget-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("isolated directory");
        Self(path)
    }
    fn wal(&self) -> PathBuf {
        self.0.join("tablet.wal")
    }
}
impl Drop for TempWal {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(819).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}
fn write(tablet: &WalBackedTablet, value: &[u8]) -> nulang::database::tablet::TabletWrite {
    tablet
        .prepare_write(
            1,
            tablet.current_sequence(),
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap()
}

#[test]
fn budget_exhaustion_preserves_wal_and_mvcc_then_allows_explicit_larger_budget() {
    let temp = TempWal::new();
    let wal = temp.wal();
    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal).unwrap();
        let initial_size = fs::metadata(&wal).unwrap().len();
        let first = write(&tablet, b"first");
        assert_eq!(
            tablet
                .commit_with_wal_budget(first, initial_size + 4096)
                .unwrap(),
            1
        );
        let committed_bytes = fs::read(&wal).unwrap();

        let second = write(&tablet, &vec![b'q'; 256]);
        let error = tablet
            .commit_with_wal_budget(second.clone(), committed_bytes.len() as u64)
            .unwrap_err();
        assert!(matches!(
            error,
            WalBackedError::Wal(WalError::BudgetExceeded {
                max_bytes, ..
            }) if max_bytes == committed_bytes.len() as u64
        ));
        assert_eq!(
            fs::read(&wal).unwrap(),
            committed_bytes,
            "rejected write cannot touch WAL"
        );
        assert_eq!(
            tablet.current_sequence(),
            1,
            "rejected write cannot publish MVCC"
        );
        assert_eq!(tablet.read_latest(b"k"), Some(&b"first"[..]));

        // Rejection happens before the first file mutation and is retryable.
        assert_eq!(
            tablet
                .commit_with_wal_budget(second, initial_size + 8192)
                .unwrap(),
            2
        );
        assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"first"[..]));
        assert_eq!(tablet.read_latest(b"k"), Some(&vec![b'q'; 256][..]));
    }
    let restored = WalBackedTablet::open(descriptor(), &wal).unwrap();
    assert_eq!(restored.current_sequence(), 2);
    assert_eq!(restored.read_at(b"k", 1).unwrap(), Some(&b"first"[..]));
    assert_eq!(restored.read_latest(b"k"), Some(&vec![b'q'; 256][..]));
}

#[test]
fn zero_budget_fails_before_first_commit_without_poisoning_writer() {
    let temp = TempWal::new();
    let wal = temp.wal();
    let mut tablet = WalBackedTablet::open(descriptor(), &wal).unwrap();
    let before = fs::read(&wal).unwrap();
    let pending = write(&tablet, b"kept");
    let error = tablet
        .commit_with_wal_budget(pending.clone(), 0)
        .unwrap_err();
    assert!(matches!(
        error,
        WalBackedError::Wal(WalError::BudgetExceeded { max_bytes: 0, .. })
    ));
    assert_eq!(fs::read(&wal).unwrap(), before);
    assert_eq!(tablet.current_sequence(), 0);
    assert_eq!(tablet.commit_with_wal_budget(pending, 4096).unwrap(), 1);
}

#[test]
fn pinned_consumer_budget_exhaustion_never_checkpoints_or_reclaims_history() {
    let temp = TempWal::new();
    let wal = temp.wal();
    let mut db = WalBackedTablet::open(descriptor(), &wal).unwrap();
    let first = write(&db, b"durable");
    assert_eq!(db.commit(first).unwrap(), 1);
    let before = fs::read(&wal).unwrap();
    let registry_path = db.retention_registry_path();
    let mut pins = DurableRetentionRegistry::open(&registry_path, 819, 1).unwrap();
    pins.register(RetentionClass::Changefeed, "slow-reader", 1)
        .unwrap();

    let proposed = write(&db, &vec![b'b'; 4096]);
    let error = db
        .commit_with_wal_budget(proposed, before.len() as u64)
        .unwrap_err();
    assert!(matches!(
        error,
        WalBackedError::Wal(WalError::BudgetExceeded { .. })
    ));
    assert_eq!(fs::read(&wal).unwrap(), before);
    assert!(!wal.with_extension("checkpoint").exists());
    assert_eq!(pins.minimum_wal_cursor(), 1);
    drop(db);
    drop(pins);

    let reopened = WalBackedTablet::open(descriptor(), &wal).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"durable"[..]));
    let persisted_pins = DurableRetentionRegistry::open(&registry_path, 819, 1).unwrap();
    assert_eq!(persisted_pins.minimum_wal_cursor(), 1);
}

fn identified(n: u8) -> nulang::database::tablet::TransactionId {
    nulang::database::tablet::TransactionId::new([n; 16])
}

fn mutation(value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: b"k".to_vec(),
        value: value.to_vec(),
    }
}

#[test]
fn batch_budget_rejects_all_records_atomically_before_mutating_wal() {
    use nulang::database::tablet::TabletWrite;
    let root = TempWal::new();
    let wal = root.wal();
    let mut db = WalBackedTablet::open(descriptor(), &wal).unwrap();
    let writes = vec![
        TabletWrite::prepare(&descriptor(), 1, 0, 0, vec![mutation(b"one")]).unwrap(),
        TabletWrite::prepare(&descriptor(), 1, 1, 1, vec![mutation(&vec![b'q'; 2048])]).unwrap(),
    ];
    let initial = fs::read(&wal).unwrap();
    assert!(matches!(
        db.commit_batch_with_wal_budget(writes.clone(), initial.len() as u64 + 512),
        Err(WalBackedError::Wal(WalError::BudgetExceeded { .. }))
    ));
    assert_eq!(fs::read(&wal).unwrap(), initial);
    assert_eq!(db.current_sequence(), 0);
    assert_eq!(db.read_latest(b"k"), None);
    assert_eq!(
        db.commit_batch_with_wal_budget(writes, 16 * 1024).unwrap(),
        2
    );
    drop(db);
    let db = WalBackedTablet::open(descriptor(), &wal).unwrap();
    assert_eq!(db.read_at(b"k", 1).unwrap(), Some(&b"one"[..]));
    assert_eq!(db.read_latest(b"k"), Some(&vec![b'q'; 2048][..]));
}

#[test]
fn idempotent_single_budget_denial_preserves_receipts_and_retry() {
    let root = TempWal::new();
    let wal = root.wal();
    let mut db = WalBackedTablet::open(descriptor(), &wal).unwrap();
    let first = vec![mutation(b"first")];
    let committed = db
        .commit_idempotent_with_wal_budget(identified(1), first.clone(), 4096)
        .unwrap();
    assert_eq!(committed.sequence(), 1);
    let bytes = fs::read(&wal).unwrap();
    let denied = db.commit_idempotent_with_wal_budget(
        identified(2),
        vec![mutation(b"second")],
        bytes.len() as u64,
    );
    assert!(matches!(
        denied,
        Err(WalBackedError::Wal(WalError::BudgetExceeded { .. }))
    ));
    assert_eq!(fs::read(&wal).unwrap(), bytes);
    assert_eq!(db.current_sequence(), 1);
    assert_eq!(
        db.commit_idempotent_with_wal_budget(identified(1), first, 0)
            .unwrap(),
        committed,
        "previously durable idempotent retry has no WAL growth"
    );
    assert_eq!(
        db.commit_idempotent_with_wal_budget(identified(2), vec![mutation(b"second")], 8192)
            .unwrap()
            .sequence(),
        2
    );
    drop(db);
    let db = WalBackedTablet::open(descriptor(), &wal).unwrap();
    assert_eq!(db.current_sequence(), 2);
    assert_eq!(db.read_at(b"k", 1).unwrap(), Some(&b"first"[..]));
}

#[test]
fn identified_group_budget_denial_keeps_all_new_receipts_absent() {
    let root = TempWal::new();
    let wal = root.wal();
    let mut db = WalBackedTablet::open(descriptor(), &wal).unwrap();
    let prior = db
        .commit_idempotent(identified(3), vec![mutation(b"prior")])
        .unwrap();
    let bytes = fs::read(&wal).unwrap();
    let entries = vec![
        (identified(4), vec![mutation(b"four")]),
        (identified(3), vec![mutation(b"prior")]),
        (identified(5), vec![mutation(&vec![b'z'; 2048])]),
    ];
    assert!(matches!(
        db.commit_batch_idempotent_with_wal_budget(entries.clone(), bytes.len() as u64 + 512),
        Err(WalBackedError::Wal(WalError::BudgetExceeded { .. }))
    ));
    assert_eq!(fs::read(&wal).unwrap(), bytes);
    assert_eq!(db.current_sequence(), 1);
    let receipts = db
        .commit_batch_idempotent_with_wal_budget(entries, 16 * 1024)
        .unwrap();
    assert_eq!(
        receipts.iter().map(|r| r.sequence()).collect::<Vec<_>>(),
        vec![2, 1, 3]
    );
    assert_eq!(receipts[1], prior);
    drop(db);
    let mut recovered = WalBackedTablet::open(descriptor(), &wal).unwrap();
    assert_eq!(recovered.current_sequence(), 3);
    let bytes = fs::read(&wal).unwrap();
    let replay = recovered
        .commit_batch_idempotent_with_wal_budget(
            vec![
                (identified(4), vec![mutation(b"four")]),
                (identified(5), vec![mutation(&vec![b'z'; 2048])]),
            ],
            0,
        )
        .unwrap();
    assert_eq!(
        replay.iter().map(|r| r.sequence()).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert_eq!(fs::read(&wal).unwrap(), bytes);
}
