use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Sandbox {
    root: PathBuf,
}
impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nudb_owner_backup_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self { root }
    }
    fn wal(&self) -> PathBuf {
        self.root.join("original.wal")
    }
    fn target(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(120).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        4,
    )
    .unwrap()
}
fn put(key: &[u8], value: &[u8]) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }]
}
fn commit(db: &mut WalBackedTablet, key: &[u8], value: &[u8]) {
    let write = db
        .prepare_write(4, db.current_sequence(), put(key, value))
        .unwrap();
    db.commit(write).unwrap();
}
fn checkpoint_backup_case(reclaim: bool) {
    let sandbox = Sandbox::new();
    let wal = sandbox.wal();
    let id = TransactionId::new([7; 16]);
    let mut db = WalBackedTablet::open(descriptor(), &wal).unwrap();
    commit(&mut db, b"k", b"early");
    let receipt = db.commit_idempotent(id, put(b"k", b"later")).unwrap();
    if reclaim {
        db.checkpoint().unwrap();
    } else {
        db.publish_checkpoint().unwrap();
    }
    let write = db
        .prepare_write(4, 2, vec![TabletMutation::Delete { key: b"k".to_vec() }])
        .unwrap();
    db.commit(write).unwrap();
    commit(&mut db, b"other", b"present");
    let source_wal_before = fs::read(&wal).unwrap();
    let source_checkpoint_before = fs::read(wal.with_extension("checkpoint")).unwrap();

    let destination = sandbox.target(if reclaim {
        "reclaimed-backup"
    } else {
        "full-wal-backup"
    });
    let backup = db.capture_consistent_backup(&destination).unwrap();
    assert_eq!(backup.sequence, 4);
    assert_eq!(backup.destination, destination);
    assert!(backup.wal_bytes > 64);
    assert!(backup.checkpoint_bytes.is_some());
    assert!(destination.join("COMPLETE.json").is_file());
    assert!(!destination.join(".restore-proof").exists());
    assert_eq!(fs::read(&wal).unwrap(), source_wal_before);
    assert_eq!(
        fs::read(wal.with_extension("checkpoint")).unwrap(),
        source_checkpoint_before
    );
    // Independently inspect recovered logical history and an idempotency receipt.
    let mut restored = WalBackedTablet::open(descriptor(), destination.join("tablet.wal")).unwrap();
    assert_eq!(restored.current_sequence(), 4);
    assert_eq!(restored.read_at(b"k", 1).unwrap(), Some(&b"early"[..]));
    assert_eq!(restored.read_at(b"k", 2).unwrap(), Some(&b"later"[..]));
    assert_eq!(restored.read_at(b"k", 3).unwrap(), None);
    assert_eq!(restored.read_latest(b"other"), Some(&b"present"[..]));
    assert_eq!(
        restored.commit_idempotent(id, put(b"k", b"later")).unwrap(),
        receipt
    );
    assert_eq!(restored.current_sequence(), 4);
    drop(restored);
    drop(db);
}

#[test]
fn checkpoint_published_with_full_wal_captures_and_restores() {
    checkpoint_backup_case(false);
}

#[test]
fn reclaimed_wal_captures_and_restores_identified_transactions() {
    checkpoint_backup_case(true);
}

#[test]
fn wal_only_capture_and_restore_keep_latest_committed_sequence() {
    let sandbox = Sandbox::new();
    let mut db = WalBackedTablet::open(descriptor(), sandbox.wal()).unwrap();
    commit(&mut db, b"k", b"value");
    let destination = sandbox.target("wal-only");
    let report = db.capture_consistent_backup(&destination).unwrap();
    assert_eq!(report.sequence, 1);
    assert_eq!(report.checkpoint_bytes, None);
    assert!(!destination.join("tablet.checkpoint").exists());
    assert!(destination.join("COMPLETE.json").exists());
    let restored = WalBackedTablet::open(descriptor(), destination.join("tablet.wal")).unwrap();
    assert_eq!(restored.read_latest(b"k"), Some(&b"value"[..]));
}

#[test]
fn refuses_to_overwrite_an_existing_destination_without_mutating_source() {
    let sandbox = Sandbox::new();
    let mut db = WalBackedTablet::open(descriptor(), sandbox.wal()).unwrap();
    commit(&mut db, b"k", b"value");
    let destination = sandbox.target("existing");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("sentinel"), b"preserve").unwrap();
    let before = fs::read(sandbox.wal()).unwrap();
    assert!(db.capture_consistent_backup(&destination).is_err());
    assert_eq!(fs::read(destination.join("sentinel")).unwrap(), b"preserve");
    assert!(!destination.join("COMPLETE.json").exists());
    assert_eq!(fs::read(sandbox.wal()).unwrap(), before);
}

#[test]
fn corrupted_source_wal_never_publishes_completion() {
    let sandbox = Sandbox::new();
    let mut db = WalBackedTablet::open(descriptor(), sandbox.wal()).unwrap();
    commit(&mut db, b"k", b"value");
    let mut raw = fs::read(sandbox.wal()).unwrap();
    *raw.last_mut().unwrap() ^= 0x40;
    fs::write(sandbox.wal(), &raw).unwrap();
    let dest = sandbox.target("bad-source");
    assert!(db.capture_consistent_backup(&dest).is_err());
    assert!(!dest.join("COMPLETE.json").exists());
    assert_eq!(fs::read(sandbox.wal()).unwrap(), raw);
}

#[test]
fn checksum_valid_checkpoint_with_wrong_logical_values_fails_restore_proof() {
    let original = Sandbox::new();
    let alternate = Sandbox::new();

    let mut owner = WalBackedTablet::open(descriptor(), original.wal()).unwrap();
    commit(&mut owner, b"k", b"original");
    owner.publish_checkpoint().unwrap();

    {
        let mut other = WalBackedTablet::open(descriptor(), alternate.wal()).unwrap();
        commit(&mut other, b"k", b"forged-but-checksummed");
        other.publish_checkpoint().unwrap();
    }

    // The substituted checkpoint is individually checksum-valid and has the
    // same descriptor and sequence, but contradicts the owner's actual WAL.
    let checkpoint = original.wal().with_extension("checkpoint");
    fs::copy(alternate.wal().with_extension("checkpoint"), &checkpoint).unwrap();
    let source_checkpoint = fs::read(&checkpoint).unwrap();
    let source_wal = fs::read(original.wal()).unwrap();

    let destination = original.target("mismatch");
    let result = owner.capture_consistent_backup(&destination);
    assert!(
        matches!(
            result,
            Err(nulang::database::backup::BackupError::RestoreMismatch)
        ),
        "matching checksums are insufficient to prove logical state"
    );
    assert!(!destination.join("COMPLETE.json").exists());
    assert_eq!(fs::read(&checkpoint).unwrap(), source_checkpoint);
    assert_eq!(fs::read(original.wal()).unwrap(), source_wal);
}

#[test]
fn quota_protected_backup_preserves_enforced_wal_quota_after_restore() {
    let sandbox = Sandbox::new();
    let mut db = WalBackedTablet::open(descriptor(), sandbox.wal()).unwrap();
    commit(&mut db, b"k", b"value");
    db.activate_durable_wal_quota(4096).unwrap();
    let destination = sandbox.target("quota-protected-backup");

    let receipt = db.capture_consistent_backup(&destination).unwrap();
    assert_eq!(receipt.sequence, 1);
    assert!(
        destination.join("tablet.wal.quota").is_file(),
        "captured backup must not silently drop the durable admission policy"
    );
    let mut restored = WalBackedTablet::open(descriptor(), destination.join("tablet.wal")).unwrap();
    assert_eq!(restored.durable_wal_quota(), Some(4096));
    let write = restored
        .prepare_write(
            4,
            restored.current_sequence(),
            put(b"large", &vec![1; 8192]),
        )
        .unwrap();
    assert!(matches!(
        restored.commit(write),
        Err(nulang::database::store::WalBackedError::Wal(
            nulang::database::wal::WalError::BudgetExceeded { .. }
        ))
    ));
    assert_eq!(restored.current_sequence(), 1);
}
