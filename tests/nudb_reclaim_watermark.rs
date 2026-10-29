use nulang::database::retention::RetentionClass;
use nulang::database::retention_store::{DurableRetentionRegistry, RegistryError};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nudb_reclaim_floor_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("tablet.pins")
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn open(s: &Sandbox) -> DurableRetentionRegistry {
    DurableRetentionRegistry::open(s.path(), 205, 9).unwrap()
}
fn certify_wal_classes(registry: &mut DurableRetentionRegistry) {
    for class in [
        RetentionClass::Changefeed,
        RetentionClass::Backup,
        RetentionClass::Replica,
        RetentionClass::Branch,
    ] {
        registry.certify_complete(class).unwrap();
    }
}

#[test]
fn watermark_advances_durably_and_rejects_late_stale_consumer() {
    let s = Sandbox::new();
    let revision;
    {
        let mut owner = open(&s);
        assert_eq!(owner.minimum_wal_cursor(), 1);
        certify_wal_classes(&mut owner);
        revision = owner.reserve_reclaim_through(5, 8).unwrap();
        assert_eq!(owner.minimum_wal_cursor(), 6);
        assert!(matches!(
            owner.register(RetentionClass::Changefeed, "late", 5),
            Err(RegistryError::HistoryUnavailable {
                earliest: 6,
                requested: 5
            })
        ));
        owner
            .register(RetentionClass::Changefeed, "good", 6)
            .unwrap();
    }
    let mut recovered = open(&s);
    assert_eq!(recovered.minimum_wal_cursor(), 6);
    assert!(recovered.revision() > revision);
    assert!(matches!(
        recovered.register(RetentionClass::Replica, "late-replica", 2),
        Err(RegistryError::HistoryUnavailable {
            earliest: 6,
            requested: 2
        })
    ));
    assert_eq!(
        recovered
            .consumer_cursor(RetentionClass::Changefeed, "good")
            .unwrap(),
        Some(6)
    );
}

#[test]
fn pinned_and_unknown_consumers_block_watermark_promotion_without_mutation() {
    let s = Sandbox::new();
    let mut owner = open(&s);
    let before = fs::read(s.path()).unwrap();
    assert!(matches!(
        owner.reserve_reclaim_through(3, 8),
        Err(RegistryError::ReclaimBlocked(_))
    ));
    assert_eq!(fs::read(s.path()).unwrap(), before);
    certify_wal_classes(&mut owner);
    owner
        .register(RetentionClass::Backup, "archive", 3)
        .unwrap();
    let before = fs::read(s.path()).unwrap();
    assert!(matches!(
        owner.reserve_reclaim_through(3, 8),
        Err(RegistryError::ReclaimBlocked(_))
    ));
    assert_eq!(fs::read(s.path()).unwrap(), before);
    assert_eq!(owner.minimum_wal_cursor(), 1);
    owner.advance(RetentionClass::Backup, "archive", 4).unwrap();
    owner.reserve_reclaim_through(3, 8).unwrap();
    assert_eq!(owner.minimum_wal_cursor(), 4);
}

#[test]
fn watermark_never_exceeds_known_tail_or_overflows() {
    let s = Sandbox::new();
    let mut owner = open(&s);
    certify_wal_classes(&mut owner);
    assert!(matches!(
        owner.reserve_reclaim_through(9, 8),
        Err(RegistryError::InvalidReclaimBound)
    ));
    assert!(matches!(
        owner.reserve_reclaim_through(u64::MAX, u64::MAX),
        Err(RegistryError::InvalidReclaimBound)
    ));
    assert_eq!(owner.minimum_wal_cursor(), 1);
    owner.reserve_reclaim_through(8, 8).unwrap();
    assert_eq!(owner.minimum_wal_cursor(), 9);
}

#[test]
fn repeating_watermark_reservation_is_idempotent() {
    let s = Sandbox::new();
    let mut owner = open(&s);
    certify_wal_classes(&mut owner);
    let revision = owner.reserve_reclaim_through(4, 7).unwrap();
    let before = fs::read(s.path()).unwrap();
    assert_eq!(owner.reserve_reclaim_through(4, 7).unwrap(), revision);
    assert_eq!(before, fs::read(s.path()).unwrap());
    owner.reserve_reclaim_through(6, 7).unwrap();
    assert_eq!(owner.minimum_wal_cursor(), 7);
}

#[test]
fn snapshot_pins_are_not_wal_consumers() {
    let s = Sandbox::new();
    let mut owner = open(&s);
    certify_wal_classes(&mut owner);
    owner
        .register(RetentionClass::Snapshot, "snapshot", 1)
        .unwrap();
    owner.reserve_reclaim_through(4, 4).unwrap();
    assert_eq!(
        owner
            .consumer_cursor(RetentionClass::Snapshot, "snapshot")
            .unwrap(),
        Some(1)
    );
}

#[test]
fn missing_registry_with_existing_lock_is_rejected_not_reinitialized() {
    let s = Sandbox::new();
    {
        let mut owner = open(&s);
        certify_wal_classes(&mut owner);
        owner.reserve_reclaim_through(4, 8).unwrap();
    }
    fs::remove_file(s.path()).unwrap();
    assert!(matches!(
        DurableRetentionRegistry::open(s.path(), 205, 9),
        Err(RegistryError::MissingRegistry)
    ));
}

#[test]
fn invalid_watermark_corruption_is_detected_at_reopen() {
    let s = Sandbox::new();
    {
        let mut owner = open(&s);
        certify_wal_classes(&mut owner);
        owner.reserve_reclaim_through(5, 8).unwrap();
    }
    let mut bytes = fs::read(s.path()).unwrap();
    bytes[15] ^= 0x04;
    fs::write(s.path(), &bytes).unwrap();
    assert!(matches!(
        DurableRetentionRegistry::open(s.path(), 205, 9),
        Err(RegistryError::Corrupt(_))
    ));
}

#[test]
fn snapshot_registry_updates_cannot_rewind_wal_floor() {
    let s = Sandbox::new();
    let mut owner = open(&s);
    certify_wal_classes(&mut owner);
    owner.reserve_reclaim_through(4, 8).unwrap();
    owner.certify_complete(RetentionClass::Snapshot).unwrap();
    owner.register(RetentionClass::Snapshot, "s", 1).unwrap();
    owner.release(RetentionClass::Snapshot, "s").unwrap();
    assert_eq!(owner.minimum_wal_cursor(), 5);
}
