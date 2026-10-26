use nulang::database::retention::{RetentionBlock, RetentionClass};
use nulang::database::retention_store::{DurableRetentionRegistry, RegistryError};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nudb_retention_registry_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn registry(&self) -> PathBuf {
        self.0.join("tablet.pins")
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn open(root: &Sandbox) -> DurableRetentionRegistry {
    DurableRetentionRegistry::open(root.registry(), 201, 4).unwrap()
}

#[test]
fn unknown_coverage_remains_unknown_until_explicit_durable_certification() {
    let root = Sandbox::new();
    {
        let mut owner = open(&root);
        assert_eq!(
            owner.policy().mvcc_snapshot_floor(9),
            Err(RetentionBlock::Unknown(vec![RetentionClass::Snapshot]))
        );
        owner
            .register(RetentionClass::Snapshot, "reader-1", 4)
            .unwrap();
        assert_eq!(
            owner.policy().mvcc_snapshot_floor(9),
            Err(RetentionBlock::Unknown(vec![RetentionClass::Snapshot]))
        );
        owner.certify_complete(RetentionClass::Snapshot).unwrap();
        assert_eq!(owner.policy().mvcc_snapshot_floor(9), Ok(Some(4)));
    }
    let reopened = open(&root);
    assert_eq!(reopened.policy().mvcc_snapshot_floor(9), Ok(Some(4)));
}

#[test]
fn checkpoint_reclaim_is_capped_by_recovered_disconnected_consumers() {
    let root = Sandbox::new();
    {
        let mut owner = open(&root);
        for class in [
            RetentionClass::Changefeed,
            RetentionClass::Backup,
            RetentionClass::Replica,
            RetentionClass::Branch,
        ] {
            owner.certify_complete(class).unwrap();
        }
        owner
            .register(RetentionClass::Changefeed, "export-1", 8)
            .unwrap();
        owner
            .register(RetentionClass::Replica, "replica-1", 6)
            .unwrap();
        assert_eq!(owner.policy().wal_reclaim_bound(Some(10), 10), Ok(5));
    }
    let mut recovered = open(&root);
    assert_eq!(recovered.policy().wal_reclaim_bound(Some(10), 10), Ok(5));
    recovered
        .advance(RetentionClass::Replica, "replica-1", 9)
        .unwrap();
    assert_eq!(recovered.policy().wal_reclaim_bound(Some(10), 10), Ok(7));
    recovered
        .release(RetentionClass::Changefeed, "export-1")
        .unwrap();
    assert_eq!(recovered.policy().wal_reclaim_bound(Some(10), 10), Ok(8));
    drop(recovered);
    let recovered = open(&root);
    assert_eq!(recovered.policy().wal_reclaim_bound(Some(10), 10), Ok(8));
}

#[test]
fn multiple_pins_minimize_and_release_only_by_explicit_mutation() {
    let root = Sandbox::new();
    let mut owner = open(&root);
    owner.certify_complete(RetentionClass::Snapshot).unwrap();
    owner.register(RetentionClass::Snapshot, "a", 4).unwrap();
    owner.register(RetentionClass::Snapshot, "b", 7).unwrap();
    assert_eq!(owner.policy().mvcc_snapshot_floor(10), Ok(Some(4)));
    owner.release(RetentionClass::Snapshot, "a").unwrap();
    assert_eq!(owner.policy().mvcc_snapshot_floor(10), Ok(Some(7)));
    owner.release(RetentionClass::Snapshot, "b").unwrap();
    assert_eq!(owner.policy().mvcc_snapshot_floor(10), Ok(None));
    assert!(matches!(
        owner.advance(RetentionClass::Snapshot, "b", 9),
        Err(RegistryError::MissingConsumer)
    ));
}

#[test]
fn invalid_and_rewound_cursors_never_modify_committed_registry() {
    let root = Sandbox::new();
    let mut owner = open(&root);
    assert!(matches!(
        owner.register(RetentionClass::Changefeed, "", 2),
        Err(RegistryError::InvalidConsumer)
    ));
    assert!(matches!(
        owner.register(RetentionClass::Changefeed, "c", 0),
        Err(RegistryError::InvalidCursor)
    ));
    owner.register(RetentionClass::Changefeed, "c", 5).unwrap();
    let before = fs::read(root.registry()).unwrap();
    assert!(matches!(
        owner.advance(RetentionClass::Changefeed, "c", 4),
        Err(RegistryError::CursorRegression)
    ));
    assert!(matches!(
        owner.register(RetentionClass::Changefeed, "c", 6),
        Err(RegistryError::AlreadyRegistered)
    ));
    assert_eq!(before, fs::read(root.registry()).unwrap());
    assert_eq!(
        owner
            .consumer_cursor(RetentionClass::Changefeed, "c")
            .unwrap(),
        Some(5)
    );
}

#[test]
fn an_ambiguous_pending_commit_is_not_recovered_as_unpinned() {
    let root = Sandbox::new();
    {
        let mut owner = open(&root);
        owner.certify_complete(RetentionClass::Changefeed).unwrap();
        owner
            .register(RetentionClass::Changefeed, "critical", 2)
            .unwrap();
    }
    let state_before = fs::read(root.registry()).unwrap();
    fs::write(root.0.join("tablet.pins.pending"), b"interrupted").unwrap();
    assert!(matches!(
        DurableRetentionRegistry::open(root.registry(), 201, 4),
        Err(RegistryError::AmbiguousCommit)
    ));
    assert_eq!(fs::read(root.registry()).unwrap(), state_before);
}

#[test]
fn corrupted_or_missing_registry_does_not_silently_become_empty() {
    let root = Sandbox::new();
    {
        let mut owner = open(&root);
        owner.certify_complete(RetentionClass::Backup).unwrap();
        owner
            .register(RetentionClass::Backup, "archive", 3)
            .unwrap();
    }
    let mut raw = fs::read(root.registry()).unwrap();
    *raw.last_mut().unwrap() ^= 0x01;
    fs::write(root.registry(), raw.clone()).unwrap();
    assert!(matches!(
        DurableRetentionRegistry::open(root.registry(), 201, 4),
        Err(RegistryError::Corrupt(_))
    ));
    assert_eq!(fs::read(root.registry()).unwrap(), raw);
}

#[test]
fn accidentally_missing_registry_reinitializes_to_unknown_not_empty_coverage() {
    let root = Sandbox::new();
    {
        let mut owner = open(&root);
        owner.certify_complete(RetentionClass::Backup).unwrap();
        owner
            .register(RetentionClass::Backup, "archive", 3)
            .unwrap();
    }
    fs::remove_file(root.registry()).unwrap();
    let restarted = open(&root);
    assert_eq!(restarted.revision(), 0);
    assert_eq!(
        restarted.policy().wal_reclaim_bound(Some(7), 7),
        Err(RetentionBlock::Unknown(vec![
            RetentionClass::Changefeed,
            RetentionClass::Backup,
            RetentionClass::Replica,
            RetentionClass::Branch,
        ]))
    );
}

#[test]
fn orphaned_stage_file_blocks_reopen_instead_of_auto_repair() {
    let root = Sandbox::new();
    {
        let _ = open(&root);
    }
    fs::write(root.0.join("tablet.pins.stage"), b"interrupted").unwrap();
    assert!(matches!(
        DurableRetentionRegistry::open(root.registry(), 201, 4),
        Err(RegistryError::AmbiguousCommit)
    ));
}

#[test]
fn second_writer_and_wrong_tablet_or_epoch_are_rejected() {
    let root = Sandbox::new();
    let first = open(&root);
    assert!(matches!(
        DurableRetentionRegistry::open(root.registry(), 201, 4),
        Err(RegistryError::WriterLocked)
    ));
    drop(first);
    assert!(matches!(
        DurableRetentionRegistry::open(root.registry(), 202, 4),
        Err(RegistryError::IdentityMismatch)
    ));
    assert!(matches!(
        DurableRetentionRegistry::open(root.registry(), 201, 5),
        Err(RegistryError::IdentityMismatch)
    ));
    assert_eq!(open(&root).revision(), 0);
}

#[test]
fn ambiguous_live_update_blocks_all_reclamation_plans_until_recovery() {
    let root = Sandbox::new();
    let mut owner = open(&root);
    for class in [
        RetentionClass::Changefeed,
        RetentionClass::Backup,
        RetentionClass::Replica,
        RetentionClass::Branch,
    ] {
        owner.certify_complete(class).unwrap();
    }
    assert_eq!(owner.policy().wal_reclaim_bound(Some(8), 8), Ok(8));
    fs::write(root.0.join("tablet.pins.pending"), b"interrupted").unwrap();
    assert!(matches!(
        owner.register(RetentionClass::Changefeed, "consumer", 3),
        Err(RegistryError::AmbiguousCommit)
    ));
    assert_eq!(
        owner.policy().wal_reclaim_bound(Some(8), 8),
        Err(RetentionBlock::Unknown(vec![
            RetentionClass::Changefeed,
            RetentionClass::Backup,
            RetentionClass::Replica,
            RetentionClass::Branch
        ]))
    );
    assert!(matches!(
        owner.consumer_cursor(RetentionClass::Changefeed, "consumer"),
        Err(RegistryError::Poisoned)
    ));
    assert!(matches!(
        owner.certify_complete(RetentionClass::Changefeed),
        Err(RegistryError::Poisoned)
    ));
    assert!(matches!(
        owner.register(RetentionClass::Backup, "new", 2),
        Err(RegistryError::Poisoned)
    ));
}

#[cfg(unix)]
#[test]
fn symlinked_registry_is_rejected() {
    use std::os::unix::fs::symlink;
    let root = Sandbox::new();
    {
        let _ = open(&root);
    }
    let moved = root.0.join("real.pins");
    fs::rename(root.registry(), &moved).unwrap();
    symlink(&moved, root.registry()).unwrap();
    assert!(matches!(
        DurableRetentionRegistry::open(root.registry(), 201, 4),
        Err(RegistryError::UnsafePath)
    ));
}
