use nulang::database::retention::RetentionClass;
use nulang::database::retention_store::{DurableRetentionRegistry, RegistryError};
use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::FileWal;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nudb_compacted_bootstrap_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
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
fn compact(s: &Sandbox) -> WalBackedTablet {
    let mut db = open(s);
    put(&mut db, b"original");
    db.checkpoint().unwrap();
    db
}
fn raw_registry(db: &WalBackedTablet) -> Result<DurableRetentionRegistry, RegistryError> {
    DurableRetentionRegistry::open(
        db.retention_registry_path(),
        db.descriptor().id().get(),
        db.descriptor().ownership_epoch(),
    )
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
#[test]
fn fresh_compacted_wal_refuses_unverified_registry_initialization() {
    let s = Sandbox::new();
    let db = compact(&s);
    let before = fs::read(s.wal()).unwrap();
    assert!(matches!(
        raw_registry(&db),
        Err(RegistryError::BootstrapRequired { wal_base: 1 })
    ));
    assert!(!db.retention_registry_path().exists());
    assert_eq!(fs::read(s.wal()).unwrap(), before);
}

#[test]
fn verified_bootstrap_persists_floor_and_rejects_late_consumer_after_restart() {
    let s = Sandbox::new();
    let db = compact(&s);
    let mut registry = db.bootstrap_retention_after_verified_compaction().unwrap();
    assert_eq!(registry.minimum_wal_cursor(), 2);
    assert!(matches!(
        registry.register(RetentionClass::Changefeed, "too-old", 1),
        Err(RegistryError::HistoryUnavailable {
            earliest: 2,
            requested: 1
        })
    ));
    registry
        .register(RetentionClass::Changefeed, "good", 2)
        .unwrap();
    drop(registry);
    drop(db);
    let db = open(&s);
    let registry = raw_registry(&db).unwrap();
    assert_eq!(registry.minimum_wal_cursor(), 2);
    assert_eq!(
        registry
            .consumer_cursor(RetentionClass::Changefeed, "good")
            .unwrap(),
        Some(2)
    );
    assert_eq!(db.read_latest(b"key"), Some(&b"original"[..]));
}

#[test]
fn matching_identity_but_logically_wrong_checkpoint_cannot_bootstrap() {
    let s = Sandbox::new();
    let altered = Sandbox::new();
    let db = compact(&s);
    let mut other = open(&altered);
    put(&mut other, b"forged");
    other.publish_checkpoint().unwrap();
    let source = altered.wal().with_extension("checkpoint");
    let destination = s.wal().with_extension("checkpoint");
    fs::copy(source, &destination).unwrap();
    let before = fs::read(s.wal()).unwrap();
    assert!(db.bootstrap_retention_after_verified_compaction().is_err());
    assert!(!db.retention_registry_path().exists());
    assert_eq!(fs::read(s.wal()).unwrap(), before);
}

#[test]
fn damaged_checkpoint_cannot_create_verified_registry() {
    let s = Sandbox::new();
    let db = compact(&s);
    let checkpoint = s.wal().with_extension("checkpoint");
    let mut bytes = fs::read(&checkpoint).unwrap();
    *bytes.last_mut().unwrap() ^= 0x80;
    fs::write(&checkpoint, bytes).unwrap();
    assert!(db.bootstrap_retention_after_verified_compaction().is_err());
    assert!(!db.retention_registry_path().exists());
}

#[test]
fn existing_stale_registry_below_wal_base_is_refused() {
    let s = Sandbox::new();
    let db = compact(&s);
    let source = Sandbox::new();
    let other = open(&source);
    let fresh_registry = raw_registry(&other).unwrap();
    drop(fresh_registry);
    let copy_from = other.retention_registry_path();
    let copy_to = db.retention_registry_path();
    fs::copy(copy_from, &copy_to).unwrap();
    assert!(matches!(
        raw_registry(&db),
        Err(RegistryError::BootstrapRequired { wal_base: 1 })
    ));
    assert!(db.bootstrap_retention_after_verified_compaction().is_err());
    assert_eq!(fs::read(copy_to).unwrap().len() > 44, true);
}

#[test]
fn verified_adoption_allows_guarded_reclaim_after_certification() {
    let s = Sandbox::new();
    let mut db = compact(&s);
    let mut owner = db.bootstrap_retention_after_verified_compaction().unwrap();
    certify(&mut owner);
    put(&mut db, b"newer");
    db.checkpoint_with_retention(&mut owner).unwrap();
    assert_eq!(owner.minimum_wal_cursor(), 3);
    assert_eq!(FileWal::verify_read_only(s.wal()).unwrap().base_sequence, 2);
    assert_eq!(db.read_latest(b"key"), Some(&b"newer"[..]));
}

#[test]
fn competing_admission_owner_blocks_even_unverified_bootstrap_attempt() {
    use std::fs::OpenOptions;
    let sandbox = Sandbox::new();
    let db = compact(&sandbox);
    let gate = PathBuf::from(format!(
        "{}.admission.lock",
        db.retention_registry_path().display()
    ));
    let locked = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(gate)
        .unwrap();
    locked.try_lock().unwrap();
    assert!(matches!(
        raw_registry(&db),
        Err(RegistryError::WriterLocked)
    ));
    assert!(db.bootstrap_retention_after_verified_compaction().is_err());
    assert!(!db.retention_registry_path().exists());
    locked.unlock().unwrap();
}

#[test]
fn verified_bootstrap_replays_commits_after_legacy_wal_base() {
    let s = Sandbox::new();
    let mut db = compact(&s);
    put(&mut db, b"after-prefix");
    assert_eq!(db.current_sequence(), 2);
    let owner = db.bootstrap_retention_after_verified_compaction().unwrap();
    assert_eq!(owner.minimum_wal_cursor(), 2);
    assert_eq!(owner.policy().wal_reclaim_bound(Some(2), 2).is_err(), true);
    drop(owner);
    drop(db);
    let restored = open(&s);
    assert_eq!(restored.current_sequence(), 2);
    assert_eq!(restored.read_latest(b"key"), Some(&b"after-prefix"[..]));
    assert_eq!(raw_registry(&restored).unwrap().minimum_wal_cursor(), 2);
}

#[test]
fn repeated_bootstrap_does_not_reset_existing_pins() {
    let s = Sandbox::new();
    let db = compact(&s);
    let mut owner = db.bootstrap_retention_after_verified_compaction().unwrap();
    owner.register(RetentionClass::Replica, "r1", 2).unwrap();
    let before = fs::read(db.retention_registry_path()).unwrap();
    drop(owner);
    assert!(db.bootstrap_retention_after_verified_compaction().is_err());
    assert_eq!(fs::read(db.retention_registry_path()).unwrap(), before);
}
