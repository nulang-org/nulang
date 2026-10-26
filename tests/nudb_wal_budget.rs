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
