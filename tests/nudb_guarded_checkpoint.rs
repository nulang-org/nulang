use nulang::database::retention::RetentionClass;
use nulang::database::retention_store::DurableRetentionRegistry;
use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};
use nulang::database::wal::FileWal;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nudb_guarded_checkpoint_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
    fn wal(&self) -> PathBuf {
        self.0.join("tablet.wal")
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(211).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        4,
    )
    .unwrap()
}
fn open(s: &Sandbox) -> WalBackedTablet {
    WalBackedTablet::open(descriptor(), s.wal()).unwrap()
}
fn registry(db: &WalBackedTablet) -> DurableRetentionRegistry {
    DurableRetentionRegistry::open(
        db.retention_registry_path(),
        db.descriptor().id().get(),
        db.descriptor().ownership_epoch(),
    )
    .unwrap()
}
fn certify(r: &mut DurableRetentionRegistry) {
    for class in [
        RetentionClass::Changefeed,
        RetentionClass::Backup,
        RetentionClass::Replica,
        RetentionClass::Branch,
    ] {
        r.certify_complete(class).unwrap();
    }
}
fn put(db: &mut WalBackedTablet, value: &[u8]) {
    let write = db
        .prepare_write(
            4,
            db.current_sequence(),
            vec![TabletMutation::Put {
                key: b"key".to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap();
    db.commit(write).unwrap();
}

#[test]
fn legacy_checkpoint_is_blocked_once_retention_is_initialized() {
    let root = Sandbox::new();
    let mut db = open(&root);
    put(&mut db, b"one");
    let raw = fs::read(root.wal()).unwrap();
    let registry = registry(&db);
    assert!(matches!(
        db.checkpoint(),
        Err(WalBackedError::RetentionManaged)
    ));
    assert_eq!(fs::read(root.wal()).unwrap(), raw);
    assert!(!root.wal().with_extension("checkpoint").exists());
    drop(registry);
    assert!(matches!(
        db.checkpoint(),
        Err(WalBackedError::RetentionManaged)
    ));
    drop(db);
    let mut reopened = open(&root);
    assert!(matches!(
        reopened.checkpoint(),
        Err(WalBackedError::RetentionManaged)
    ));
}

#[test]
fn unknown_and_pinned_consumers_block_without_checkpoint_or_wal_mutation() {
    let root = Sandbox::new();
    let mut db = open(&root);
    put(&mut db, b"one");
    put(&mut db, b"two");
    let mut owner = registry(&db);
    let wal_before = fs::read(root.wal()).unwrap();
    assert!(matches!(
        db.checkpoint_with_retention(&mut owner),
        Err(WalBackedError::Retention(_))
    ));
    assert_eq!(owner.minimum_wal_cursor(), 1);
    assert_eq!(fs::read(root.wal()).unwrap(), wal_before);
    assert!(!root.wal().with_extension("checkpoint").exists());

    certify(&mut owner);
    owner
        .register(RetentionClass::Changefeed, "slow", 2)
        .unwrap();
    assert!(matches!(
        db.checkpoint_with_retention(&mut owner),
        Err(WalBackedError::Retention(_))
    ));
    assert_eq!(owner.minimum_wal_cursor(), 1);
    assert_eq!(fs::read(root.wal()).unwrap(), wal_before);
    assert!(!root.wal().with_extension("checkpoint").exists());
}

#[test]
fn verified_managed_checkpoint_preserves_receipts_and_prevents_late_consumers() {
    let root = Sandbox::new();
    let mut db = open(&root);
    put(&mut db, b"early");
    let receipt_id = TransactionId::new([19; 16]);
    let receipt_mutation = vec![TabletMutation::Put {
        key: b"key".to_vec(),
        value: b"latest".to_vec(),
    }];
    let receipt = db
        .commit_idempotent(receipt_id, receipt_mutation.clone())
        .unwrap();
    let mut owner = registry(&db);
    certify(&mut owner);
    owner.register(RetentionClass::Replica, "r1", 3).unwrap();
    db.checkpoint_with_retention(&mut owner).unwrap();
    assert_eq!(owner.minimum_wal_cursor(), 3);
    assert_eq!(
        FileWal::verify_read_only(root.wal()).unwrap().base_sequence,
        2
    );
    assert_eq!(db.read_at(b"key", 1).unwrap(), Some(&b"early"[..]));
    assert_eq!(db.read_latest(b"key"), Some(&b"latest"[..]));
    assert!(owner
        .register(RetentionClass::Changefeed, "stale", 1)
        .is_err());

    drop(db);
    drop(owner);
    let mut restored = open(&root);
    assert_eq!(restored.current_sequence(), 2);
    assert_eq!(restored.read_at(b"key", 1).unwrap(), Some(&b"early"[..]));
    assert_eq!(
        restored
            .commit_idempotent(receipt_id, receipt_mutation)
            .unwrap(),
        receipt
    );
    assert!(matches!(
        restored.checkpoint(),
        Err(WalBackedError::RetentionManaged)
    ));
    let recovered_owner = registry(&restored);
    assert_eq!(recovered_owner.minimum_wal_cursor(), 3);
}

#[test]
fn mismatched_retention_registry_cannot_authorize_reclamation() {
    let root = Sandbox::new();
    let other = Sandbox::new();
    let mut db = open(&root);
    let wrong_db = open(&other);
    put(&mut db, b"one");
    let mut wrong_registry = registry(&wrong_db);
    certify(&mut wrong_registry);
    let before = fs::read(root.wal()).unwrap();
    assert!(matches!(
        db.checkpoint_with_retention(&mut wrong_registry),
        Err(WalBackedError::RetentionMismatch)
    ));
    assert_eq!(fs::read(root.wal()).unwrap(), before);
    assert!(!root.wal().with_extension("checkpoint").exists());
}

#[test]
fn absent_or_invalid_published_checkpoint_cannot_be_reclaimed() {
    let root = Sandbox::new();
    let mut db = open(&root);
    put(&mut db, b"one");
    let mut owner = registry(&db);
    certify(&mut owner);
    let before = fs::read(root.wal()).unwrap();
    assert!(matches!(
        db.reclaim_verified_checkpoint_with_retention(&mut owner),
        Err(WalBackedError::UnverifiedCheckpoint)
    ));
    assert_eq!(owner.minimum_wal_cursor(), 1);
    db.publish_checkpoint().unwrap();
    let checkpoint = root.wal().with_extension("checkpoint");
    let mut broken = fs::read(&checkpoint).unwrap();
    *broken.last_mut().unwrap() ^= 0x80;
    fs::write(&checkpoint, &broken).unwrap();
    assert!(db
        .reclaim_verified_checkpoint_with_retention(&mut owner)
        .is_err());
    assert_eq!(owner.minimum_wal_cursor(), 1);
    assert_eq!(fs::read(root.wal()).unwrap(), before);
    assert_eq!(fs::read(checkpoint).unwrap(), broken);
}

#[test]
fn checksum_valid_but_logically_forged_checkpoint_is_not_reclaimed() {
    let root = Sandbox::new();
    let attacker = Sandbox::new();
    let mut db = open(&root);
    let mut alternate = open(&attacker);
    put(&mut db, b"original");
    put(&mut alternate, b"forged-but-checksummed");
    db.publish_checkpoint().unwrap();
    alternate.publish_checkpoint().unwrap();
    // Same descriptor and sequence, individually valid checkpoint digest.
    let checkpoint = root.wal().with_extension("checkpoint");
    let alternate_checkpoint = attacker.wal().with_extension("checkpoint");
    fs::copy(alternate_checkpoint, &checkpoint).unwrap();
    let before_wal = fs::read(root.wal()).unwrap();
    let before_checkpoint = fs::read(&checkpoint).unwrap();
    let mut owner = registry(&db);
    certify(&mut owner);
    assert!(matches!(
        db.reclaim_verified_checkpoint_with_retention(&mut owner),
        Err(WalBackedError::UnverifiedCheckpoint)
    ));
    assert_eq!(owner.minimum_wal_cursor(), 1);
    assert_eq!(fs::read(root.wal()).unwrap(), before_wal);
    assert_eq!(fs::read(checkpoint).unwrap(), before_checkpoint);
}

#[test]
fn old_compacted_wal_requires_explicit_retention_bootstrap() {
    let root = Sandbox::new();
    let mut db = open(&root);
    put(&mut db, b"original");
    db.checkpoint().unwrap(); // A legacy prefix was already physically dropped.
    assert_eq!(
        FileWal::verify_read_only(root.wal()).unwrap().base_sequence,
        1
    );
    let mut owner = registry(&db);
    certify(&mut owner);
    let wal_before = fs::read(root.wal()).unwrap();
    assert!(matches!(
        db.checkpoint_with_retention(&mut owner),
        Err(WalBackedError::RetentionBootstrapRequired)
    ));
    assert_eq!(fs::read(root.wal()).unwrap(), wal_before);
    assert_eq!(owner.minimum_wal_cursor(), 1);
    // The external operator may reconcile the existing WAL base only after
    // certifying all detached consumers and their resume positions.
    owner.reserve_reclaim_through(1, 1).unwrap();
    assert_eq!(owner.minimum_wal_cursor(), 2);
    db.checkpoint_with_retention(&mut owner).unwrap();
    assert_eq!(
        FileWal::verify_read_only(root.wal()).unwrap().base_sequence,
        1
    );
}

#[test]
fn unmanaged_checkpoint_remains_backwards_compatible_without_registry() {
    let root = Sandbox::new();
    let mut db = open(&root);
    put(&mut db, b"one");
    db.checkpoint().unwrap();
    assert_eq!(
        FileWal::verify_read_only(root.wal()).unwrap().base_sequence,
        1
    );
    assert!(!db.retention_registry_path().exists());
}
