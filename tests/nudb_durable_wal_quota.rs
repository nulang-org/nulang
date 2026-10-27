//! Fail-closed persisted WAL admission for one canonical owner/epoch.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TabletWrite, TransactionId,
};
use nulang::database::wal::{FileWal, WalError};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nudb-durable-quota-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> PathBuf {
        self.0.join("data.wal")
    }
    fn quota(&self) -> PathBuf {
        self.0.join("data.wal.quota")
    }
    fn open(&self) -> WalBackedTablet {
        WalBackedTablet::open(descriptor(), self.path()).unwrap()
    }
}
impl Drop for Fixture {
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
fn mutation(size: usize) -> TabletMutation {
    TabletMutation::Put {
        key: b"k".to_vec(),
        value: vec![b'v'; size],
    }
}
fn write(db: &WalBackedTablet, size: usize) -> TabletWrite {
    db.prepare_write(1, db.current_sequence(), vec![mutation(size)])
        .unwrap()
}
fn id(n: u8) -> TransactionId {
    TransactionId::new([n; 16])
}
fn denied(e: WalBackedError) {
    assert!(
        matches!(e, WalBackedError::Wal(WalError::BudgetExceeded { .. })),
        "{e:?}"
    );
}

#[test]
fn quota_survives_restart_and_all_legacy_commit_apis_obey_it() {
    let f = Fixture::new();
    {
        let mut db = f.open();
        db.activate_durable_wal_quota(1024).unwrap();
        assert_eq!(db.durable_wal_quota(), Some(1024));
        let first = write(&db, 4);
        assert_eq!(db.commit(first).unwrap(), 1);
        let original = fs::read(f.path()).unwrap();
        let too_large = write(&db, 2048);
        denied(db.commit(too_large).unwrap_err());
        let batch = vec![write(&db, 2048)];
        denied(db.commit_batch(batch).unwrap_err());
        denied(
            db.commit_idempotent(id(1), vec![mutation(2048)])
                .unwrap_err(),
        );
        denied(
            db.commit_batch_idempotent(vec![(id(2), vec![mutation(2048)])])
                .unwrap_err(),
        );
        assert_eq!(fs::read(f.path()).unwrap(), original);
        assert_eq!(db.current_sequence(), 1);
        assert_eq!(db.read_at(b"k", 1).unwrap(), Some(&vec![b'v'; 4][..]));
        assert!(matches!(
            db.activate_durable_wal_quota(8192),
            Err(WalBackedError::Wal(WalError::QuotaAlreadySet { .. }))
        ));
    }
    {
        let mut db = f.open();
        assert_eq!(db.durable_wal_quota(), Some(1024));
        let saved = fs::read(f.path()).unwrap();
        denied(db.commit(write(&db, 2048)).unwrap_err());
        assert_eq!(fs::read(f.path()).unwrap(), saved);
        assert_eq!(db.current_sequence(), 1);
    }
    let mut wal = FileWal::open(f.path()).unwrap();
    let directly_prepared =
        TabletWrite::prepare(&descriptor(), 1, 1, 1, vec![mutation(2048)]).unwrap();
    assert!(matches!(
        wal.append_write(&directly_prepared),
        Err(WalError::BudgetExceeded { .. })
    ));
    assert_eq!(wal.last_sequence(), 1);
}

#[test]
fn corrupt_or_torn_quota_fails_closed_before_wal_recovery_mutation() {
    let f = Fixture::new();
    let wal_bytes;
    {
        let mut db = f.open();
        db.activate_durable_wal_quota(4096).unwrap();
        assert_eq!(db.commit(write(&db, 2)).unwrap(), 1);
        wal_bytes = fs::read(f.path()).unwrap();
    }
    let valid = fs::read(f.quota()).unwrap();
    assert!(!valid.is_empty());
    fs::write(f.quota(), &valid[..7]).unwrap();
    assert!(matches!(
        WalBackedTablet::open(descriptor(), f.path()),
        Err(WalBackedError::Wal(WalError::QuotaCorrupt))
    ));
    assert_eq!(fs::read(f.path()).unwrap(), wal_bytes);

    let mut invalid = valid.clone();
    invalid[14] ^= 0x40;
    fs::write(f.quota(), invalid).unwrap();
    assert!(matches!(
        WalBackedTablet::open(descriptor(), f.path()),
        Err(WalBackedError::Wal(WalError::QuotaCorrupt))
    ));
    assert_eq!(fs::read(f.path()).unwrap(), wal_bytes);
    fs::write(f.quota(), valid).unwrap();
    assert_eq!(f.open().current_sequence(), 1);
}

#[test]
fn zero_durable_quota_denies_all_writes_after_reopen_and_is_idempotently_installed() {
    let f = Fixture::new();
    {
        let mut db = f.open();
        db.activate_durable_wal_quota(0).unwrap();
        db.activate_durable_wal_quota(0).unwrap();
        denied(db.commit(write(&db, 8)).unwrap_err());
    }
    let mut db = f.open();
    assert_eq!(db.durable_wal_quota(), Some(0));
    denied(db.commit(write(&db, 8)).unwrap_err());
    assert_eq!(db.current_sequence(), 0);
}

#[test]
fn checkpoint_and_reclaim_do_not_clear_the_durable_quota() {
    let f = Fixture::new();
    {
        let mut db = f.open();
        db.activate_durable_wal_quota(512).unwrap();
        assert_eq!(db.commit(write(&db, 4)).unwrap(), 1);
        db.checkpoint().unwrap();
        assert_eq!(db.durable_wal_quota(), Some(512));
    }
    let mut db = f.open();
    assert_eq!(db.current_sequence(), 1);
    assert_eq!(db.durable_wal_quota(), Some(512));
    denied(db.commit(write(&db, 4096)).unwrap_err());
    assert_eq!(db.read_at(b"k", 1).unwrap(), Some(&vec![b'v'; 4][..]));
}

#[test]
fn direct_batch_cannot_bypass_quota_or_invent_a_different_owner_epoch() {
    let f = Fixture::new();
    {
        let mut db = f.open();
        db.activate_durable_wal_quota(512).unwrap();
        assert_eq!(db.commit(write(&db, 1)).unwrap(), 1);
    }
    let before = fs::read(f.path()).unwrap();
    let mut wal = FileWal::open(f.path()).unwrap();
    let batch = vec![
        TabletWrite::prepare(&descriptor(), 1, 1, 1, vec![mutation(100)]).unwrap(),
        TabletWrite::prepare(&descriptor(), 1, 2, 2, vec![mutation(2000)]).unwrap(),
    ];
    assert!(matches!(
        wal.append_batch(&batch),
        Err(WalError::BudgetExceeded { .. })
    ));
    assert_eq!(fs::read(f.path()).unwrap(), before);
    let successor_descriptor = TabletDescriptor::new(
        TabletId::new(819).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        2,
    )
    .unwrap();
    let successor =
        TabletWrite::prepare(&successor_descriptor, 2, 1, 1, vec![mutation(1)]).unwrap();
    assert!(matches!(
        wal.append_write(&successor),
        Err(WalError::QuotaOwnerMismatch)
    ));
    assert!(matches!(
        wal.activate_durable_quota(TabletId::new(819).unwrap(), 2, 512),
        Err(WalError::QuotaOwnerMismatch)
    ));
    assert_eq!(fs::read(f.path()).unwrap(), before);
}

#[test]
fn quota_protected_missing_wal_fails_closed_without_recreating_storage() {
    let fixture = Fixture::new();
    {
        let mut db = fixture.open();
        db.activate_durable_wal_quota(4096).unwrap();
        assert_eq!(db.commit(write(&db, 8)).unwrap(), 1);
    }
    let persisted_quota = fs::read(fixture.quota()).unwrap();
    fs::remove_file(fixture.path()).unwrap();

    let reopened = WalBackedTablet::open(descriptor(), fixture.path());
    assert!(matches!(
        reopened,
        Err(WalBackedError::Wal(WalError::QuotaWalMissing))
    ));
    assert!(
        !fixture.path().exists(),
        "open must not recreate a missing WAL"
    );
    assert_eq!(fs::read(fixture.quota()).unwrap(), persisted_quota);
}

#[test]
fn missing_quota_protected_wal_cannot_be_replaced_from_checkpoint_implicitly() {
    let fixture = Fixture::new();
    {
        let mut db = fixture.open();
        db.activate_durable_wal_quota(4096).unwrap();
        assert_eq!(db.commit(write(&db, 8)).unwrap(), 1);
        db.publish_checkpoint().unwrap();
    }
    let checkpoint = fixture.path().with_extension("checkpoint");
    assert!(checkpoint.exists());
    let preserved_checkpoint = fs::read(&checkpoint).unwrap();
    fs::remove_file(fixture.path()).unwrap();

    assert!(matches!(
        WalBackedTablet::open(descriptor(), fixture.path()),
        Err(WalBackedError::Wal(WalError::QuotaWalMissing))
    ));
    assert!(!fixture.path().exists());
    assert_eq!(fs::read(&checkpoint).unwrap(), preserved_checkpoint);
}

#[test]
fn quota_protected_truncated_wal_fails_before_reinitialization() {
    let fixture = Fixture::new();
    {
        let mut db = fixture.open();
        db.activate_durable_wal_quota(4096).unwrap();
        assert_eq!(db.commit(write(&db, 8)).unwrap(), 1);
    }
    let saved_quota = fs::read(fixture.quota()).unwrap();
    fs::write(fixture.path(), []).unwrap();

    let result = WalBackedTablet::open(descriptor(), fixture.path());
    assert!(matches!(
        result,
        Err(WalBackedError::Wal(WalError::QuotaWalEmpty))
    ));
    assert_eq!(fs::metadata(fixture.path()).unwrap().len(), 0);
    assert_eq!(fs::read(fixture.quota()).unwrap(), saved_quota);
}

#[test]
fn quota_protected_truncated_wal_with_checkpoint_fails_without_modifying_either() {
    let fixture = Fixture::new();
    {
        let mut db = fixture.open();
        db.activate_durable_wal_quota(4096).unwrap();
        assert_eq!(db.commit(write(&db, 8)).unwrap(), 1);
        db.publish_checkpoint().unwrap();
    }
    let checkpoint = fixture.path().with_extension("checkpoint");
    let saved_checkpoint = fs::read(&checkpoint).unwrap();
    fs::write(fixture.path(), []).unwrap();

    let result = WalBackedTablet::open(descriptor(), fixture.path());
    assert!(matches!(
        result,
        Err(WalBackedError::Wal(WalError::QuotaWalEmpty))
    ));
    assert_eq!(fs::metadata(fixture.path()).unwrap().len(), 0);
    assert_eq!(fs::read(checkpoint).unwrap(), saved_checkpoint);
}
