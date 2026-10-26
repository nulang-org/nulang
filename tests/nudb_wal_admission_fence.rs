use nulang::database::retention::RetentionClass;
use nulang::database::retention_store::{DurableRetentionRegistry, RegistryError};
use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::{FileWal, WalError};
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nudb_wal_fence_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
    fn wal(&self) -> PathBuf {
        self.0.join("tablet.wal")
    }
    fn registry(&self) -> PathBuf {
        self.0.join("tablet.wal.pins")
    }
    fn gate(&self) -> PathBuf {
        self.0.join("tablet.wal.pins.admission.lock")
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
fn filled(s: &Sandbox) -> WalBackedTablet {
    let mut db = WalBackedTablet::open(descriptor(), s.wal()).unwrap();
    let write = db
        .prepare_write(
            4,
            0,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"value".to_vec(),
            }],
        )
        .unwrap();
    db.commit(write).unwrap();
    db.publish_checkpoint().unwrap();
    db
}
fn owner(s: &Sandbox) -> DurableRetentionRegistry {
    DurableRetentionRegistry::open(s.registry(), 211, 4).unwrap()
}

#[test]
fn direct_filewal_reclaim_cannot_bypass_managed_retention() {
    let s = Sandbox::new();
    let db = filled(&s);
    let _owner = owner(&s);
    drop(db);
    let before = fs::read(s.wal()).unwrap();
    let mut wal = FileWal::open(s.wal()).unwrap();
    assert!(matches!(
        wal.reclaim_through(1),
        Err(WalError::RetentionManaged)
    ));
    assert_eq!(fs::read(s.wal()).unwrap(), before);
    assert_eq!(FileWal::verify_read_only(s.wal()).unwrap().base_sequence, 0);
}

#[test]
fn stranded_registry_lock_or_pending_stage_also_blocks_direct_reclaim() {
    for extension in [".lock", ".pending", ".stage"] {
        let s = Sandbox::new();
        let db = filled(&s);
        {
            let _ = owner(&s);
        }
        fs::remove_file(s.registry()).unwrap();
        let path = PathBuf::from(format!("{}{}", s.registry().display(), extension));
        fs::write(path, b"leftover").unwrap();
        drop(db);
        let before = fs::read(s.wal()).unwrap();
        let mut wal = FileWal::open(s.wal()).unwrap();
        assert!(matches!(
            wal.reclaim_through(1),
            Err(WalError::RetentionManaged)
        ));
        assert_eq!(fs::read(s.wal()).unwrap(), before);
    }
}

#[test]
fn acquisition_gate_blocks_both_registration_and_legacy_reclamation() {
    let s = Sandbox::new();
    let mut db = filled(&s);
    let fence = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(s.gate())
        .unwrap();
    fence.try_lock().unwrap();
    assert!(matches!(
        DurableRetentionRegistry::open(s.registry(), 211, 4),
        Err(RegistryError::WriterLocked)
    ));
    assert!(matches!(
        db.checkpoint(),
        Err(WalBackedError::Wal(WalError::WriterLocked))
    ));
    assert_eq!(FileWal::verify_read_only(s.wal()).unwrap().base_sequence, 0);
    fence.unlock().unwrap();
    drop(fence);
    let _owner = owner(&s);
    assert!(matches!(
        db.checkpoint(),
        Err(WalBackedError::RetentionManaged)
    ));
    assert_eq!(FileWal::verify_read_only(s.wal()).unwrap().base_sequence, 0);
}

#[test]
fn managed_reclaim_remains_available_via_verified_registry_path() {
    let s = Sandbox::new();
    let mut db = filled(&s);
    let mut registry = owner(&s);
    for class in [
        RetentionClass::Changefeed,
        RetentionClass::Backup,
        RetentionClass::Replica,
        RetentionClass::Branch,
    ] {
        registry.certify_complete(class).unwrap();
    }
    db.reclaim_verified_checkpoint_with_retention(&mut registry)
        .unwrap();
    assert_eq!(registry.minimum_wal_cursor(), 2);
    assert_eq!(FileWal::verify_read_only(s.wal()).unwrap().base_sequence, 1);
    drop(registry);
    assert!(matches!(
        db.checkpoint(),
        Err(WalBackedError::RetentionManaged)
    ));
}

#[test]
fn unmanaged_reclaim_retains_backward_compatibility() {
    let s = Sandbox::new();
    let mut db = filled(&s);
    db.checkpoint().unwrap();
    assert_eq!(FileWal::verify_read_only(s.wal()).unwrap().base_sequence, 1);
}

#[test]
fn admission_lock_child_fixture() {
    use std::io::Write;
    let Ok(lock) = std::env::var("NUDB_ADMISSION_CHILD_GATE") else {
        return;
    };
    let ready = std::env::var("NUDB_ADMISSION_CHILD_READY").unwrap();
    let gate = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock)
        .unwrap();
    gate.try_lock().unwrap();
    let mut file = fs::File::create(ready).unwrap();
    file.write_all(b"ready").unwrap();
    file.sync_all().unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn cross_process_admission_lock_fences_legacy_reclaim_and_registry_creation() {
    use std::process::Command;
    use std::thread;
    use std::time::Duration;
    let s = Sandbox::new();
    let mut db = filled(&s);
    OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(s.gate())
        .unwrap();
    let ready = s.0.join("child-ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("admission_lock_child_fixture")
        .arg("--nocapture")
        .env("NUDB_ADMISSION_CHILD_GATE", s.gate())
        .env("NUDB_ADMISSION_CHILD_READY", &ready)
        .spawn()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for _ in 0..200 {
            if ready.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "child did not confirm admission ownership");
        let before = fs::read(s.wal()).unwrap();
        assert!(matches!(
            DurableRetentionRegistry::open(s.registry(), 211, 4),
            Err(RegistryError::WriterLocked)
        ));
        assert!(matches!(
            db.checkpoint(),
            Err(WalBackedError::Wal(WalError::WriterLocked))
        ));
        assert_eq!(fs::read(s.wal()).unwrap(), before);
    }));
    let _ = child.kill();
    child.wait().unwrap();
    result.unwrap();
    let _registry = owner(&s);
    assert!(matches!(
        db.checkpoint(),
        Err(WalBackedError::RetentionManaged)
    ));
}

#[test]
fn subprocess_aborts_after_watermark_before_wal_replacement_fixture() {
    let Ok(path) = std::env::var("NUDB_WATERMARK_CRASH_WAL") else {
        return;
    };
    let wal = PathBuf::from(path);
    let mut db = WalBackedTablet::open(descriptor(), &wal).unwrap();
    let write = db
        .prepare_write(
            4,
            0,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"crash-safe".to_vec(),
            }],
        )
        .unwrap();
    db.commit(write).unwrap();
    db.publish_checkpoint().unwrap();
    let registry = PathBuf::from(format!("{}.pins", wal.display()));
    let mut owner = DurableRetentionRegistry::open(registry, 211, 4).unwrap();
    for class in [
        RetentionClass::Changefeed,
        RetentionClass::Backup,
        RetentionClass::Replica,
        RetentionClass::Branch,
    ] {
        owner.certify_complete(class).unwrap();
    }
    owner.reserve_reclaim_through(1, 1).unwrap();
    // Abrupt process exit skips Drop without risking a large core dump.
    std::process::exit(19);
}

#[test]
fn process_death_after_durable_watermark_is_recoverable_and_fail_closed() {
    use std::process::Command;
    let s = Sandbox::new();
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("subprocess_aborts_after_watermark_before_wal_replacement_fixture")
        .env("NUDB_WATERMARK_CRASH_WAL", s.wal())
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "subprocess should abort at the simulated crash window"
    );
    assert_eq!(FileWal::verify_read_only(s.wal()).unwrap().base_sequence, 0);
    let mut db = WalBackedTablet::open(descriptor(), s.wal()).unwrap();
    assert_eq!(db.read_latest(b"k"), Some(&b"crash-safe"[..]));
    let mut owner = owner(&s);
    assert_eq!(owner.minimum_wal_cursor(), 2);
    assert!(matches!(
        owner.register(RetentionClass::Changefeed, "late", 1),
        Err(RegistryError::HistoryUnavailable {
            earliest: 2,
            requested: 1
        })
    ));
    db.reclaim_verified_checkpoint_with_retention(&mut owner)
        .unwrap();
    assert_eq!(FileWal::verify_read_only(s.wal()).unwrap().base_sequence, 1);
    assert_eq!(db.read_latest(b"k"), Some(&b"crash-safe"[..]));
}

#[cfg(unix)]
#[test]
fn symlinked_gate_is_rejected_before_any_log_mutation() {
    use std::os::unix::fs::symlink;
    let s = Sandbox::new();
    let db = filled(&s);
    let target = s.0.join("external");
    fs::write(&target, b"not a lock").unwrap();
    symlink(&target, s.gate()).unwrap();
    drop(db);
    let before = fs::read(s.wal()).unwrap();
    let mut wal = FileWal::open(s.wal()).unwrap();
    assert!(wal.reclaim_through(1).is_err());
    assert_eq!(fs::read(s.wal()).unwrap(), before);
    assert!(DurableRetentionRegistry::open(s.registry(), 211, 4).is_err());
}
