use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};
use nulang::database::wal::WalError;

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

struct Sandbox {
    dir: PathBuf,
    wal: PathBuf,
}
impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nudb_pair_audit_{name}_{}_{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let wal = dir.join("tablet.wal");
        Self { dir, wal }
    }

    fn checkpoint(&self) -> PathBuf {
        self.wal.with_extension("checkpoint")
    }

    fn copy_to(&self, other: &Sandbox) {
        fs::copy(&self.wal, &other.wal).unwrap();
        if self.checkpoint().exists() {
            fs::copy(self.checkpoint(), other.checkpoint()).unwrap();
        }
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
    }
}
fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(88).unwrap(),
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
fn delete(key: &[u8]) -> Vec<TabletMutation> {
    vec![TabletMutation::Delete { key: key.to_vec() }]
}
fn put_commit(db: &mut WalBackedTablet, key: &[u8], value: &[u8]) {
    let write = db
        .prepare_write(4, db.current_sequence(), put(key, value))
        .unwrap();
    db.commit(write).unwrap();
}
fn verify(path: &Path) -> Result<nulang::database::store::CheckpointWalReport, WalBackedError> {
    WalBackedTablet::verify_checkpoint_wal_read_only(descriptor(), path)
}

#[test]
fn full_wal_without_checkpoint_is_valid_and_restores_from_disposable_copy() {
    let original = Sandbox::new("full_wal");
    let restored = Sandbox::new("full_wal_copy");
    {
        let mut db = WalBackedTablet::open(descriptor(), &original.wal).unwrap();
        put_commit(&mut db, b"k", b"v1");
        put_commit(&mut db, b"k", b"v2");
    }
    let original_bytes = fs::read(&original.wal).unwrap();
    let report = verify(&original.wal).unwrap();
    assert_eq!((report.checkpoint_sequence, report.durable_tail), (None, 2));
    original.copy_to(&restored);
    let copy = WalBackedTablet::open(descriptor(), &restored.wal).unwrap();
    assert_eq!(copy.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(copy.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    assert_eq!(fs::read(&original.wal).unwrap(), original_bytes);
}

#[test]
fn published_checkpoint_plus_full_wal_restores_history_receipts_and_tombstone() {
    let original = Sandbox::new("published");
    let restored = Sandbox::new("published_copy");
    let id = TransactionId::new([77; 16]);
    let receipt;
    {
        let mut db = WalBackedTablet::open(descriptor(), &original.wal).unwrap();
        put_commit(&mut db, b"k", b"v1");
        receipt = db.commit_idempotent(id, put(b"k", b"v2")).unwrap();
        assert_eq!(receipt.sequence(), 2);
        db.publish_checkpoint().unwrap();
        let write = db.prepare_write(4, 2, delete(b"k")).unwrap();
        db.commit(write).unwrap();
        put_commit(&mut db, b"other", b"alive");
    }
    let wal_before = fs::read(&original.wal).unwrap();
    let cp_before = fs::read(original.checkpoint()).unwrap();
    let report = verify(&original.wal).unwrap();
    assert_eq!(
        (report.checkpoint_sequence, report.durable_tail),
        (Some(2), 4)
    );
    original.copy_to(&restored);
    let mut copy = WalBackedTablet::open(descriptor(), &restored.wal).unwrap();
    assert_eq!(copy.current_sequence(), 4);
    assert_eq!(copy.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(copy.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    assert_eq!(copy.read_at(b"k", 3).unwrap(), None);
    assert_eq!(copy.read_latest(b"other"), Some(&b"alive"[..]));
    assert_eq!(
        copy.commit_idempotent(id, put(b"k", b"v2")).unwrap(),
        receipt
    );
    assert_eq!(copy.current_sequence(), 4);
    assert_eq!(fs::read(&original.wal).unwrap(), wal_before);
    assert_eq!(fs::read(original.checkpoint()).unwrap(), cp_before);
}

#[test]
fn reclaimed_checkpoint_and_wal_pair_restores_retained_receipts() {
    let original = Sandbox::new("reclaimed");
    let copy = Sandbox::new("reclaimed_copy");
    let id = TransactionId::new([22; 16]);
    let receipt;
    {
        let mut db = WalBackedTablet::open(descriptor(), &original.wal).unwrap();
        put_commit(&mut db, b"k", b"old");
        receipt = db.commit_idempotent(id, put(b"k", b"new")).unwrap();
        db.checkpoint().unwrap();
    }
    let report = verify(&original.wal).unwrap();
    assert_eq!(
        (report.checkpoint_sequence, report.durable_tail),
        (Some(2), 2)
    );
    assert_eq!(report.wal.record_count, 0);
    original.copy_to(&copy);
    let mut recovered = WalBackedTablet::open(descriptor(), &copy.wal).unwrap();
    assert_eq!(recovered.read_at(b"k", 1).unwrap(), Some(&b"old"[..]));
    assert_eq!(recovered.read_latest(b"k"), Some(&b"new"[..]));
    assert_eq!(
        recovered.commit_idempotent(id, put(b"k", b"new")).unwrap(),
        receipt
    );
    assert_eq!(recovered.current_sequence(), 2);
}

#[test]
fn orphaned_reclaimed_wal_is_rejected_without_creating_checkpoint() {
    let original = Sandbox::new("orphaned");
    {
        let mut db = WalBackedTablet::open(descriptor(), &original.wal).unwrap();
        put_commit(&mut db, b"k", b"v");
        db.checkpoint().unwrap();
    }
    fs::remove_file(original.checkpoint()).unwrap();
    let before = fs::read(&original.wal).unwrap();
    assert!(matches!(
        verify(&original.wal),
        Err(WalBackedError::Wal(WalError::CheckpointRequired {
            base_sequence: 1
        }))
    ));
    assert_eq!(fs::read(&original.wal).unwrap(), before);
    assert!(!original.checkpoint().exists());
}

#[test]
fn older_checkpoint_than_reclaimed_wal_is_rejected_without_mutation() {
    let original = Sandbox::new("old_cp");
    let old_checkpoint;
    {
        let mut db = WalBackedTablet::open(descriptor(), &original.wal).unwrap();
        put_commit(&mut db, b"k", b"v1");
        db.publish_checkpoint().unwrap();
        old_checkpoint = fs::read(original.checkpoint()).unwrap();
        put_commit(&mut db, b"k", b"v2");
        db.checkpoint().unwrap();
    }
    fs::write(original.checkpoint(), &old_checkpoint).unwrap();
    let wal_before = fs::read(&original.wal).unwrap();
    assert!(matches!(
        verify(&original.wal),
        Err(WalBackedError::Wal(WalError::CheckpointTooOld {
            checkpoint: 1,
            wal_base: 2
        }))
    ));
    assert_eq!(fs::read(&original.wal).unwrap(), wal_before);
    assert_eq!(fs::read(original.checkpoint()).unwrap(), old_checkpoint);
}

#[test]
fn checkpoint_ahead_of_wal_tail_is_rejected_without_mutation() {
    let original = Sandbox::new("ahead");
    let other = Sandbox::new("empty_wal");
    {
        let mut db = WalBackedTablet::open(descriptor(), &original.wal).unwrap();
        put_commit(&mut db, b"k", b"value");
        db.publish_checkpoint().unwrap();
    }
    {
        let _ = WalBackedTablet::open(descriptor(), &other.wal).unwrap();
    }
    fs::copy(&other.wal, &original.wal).unwrap();
    let cp_before = fs::read(original.checkpoint()).unwrap();
    let wal_before = fs::read(&original.wal).unwrap();
    assert!(matches!(
        verify(&original.wal),
        Err(WalBackedError::Wal(WalError::CheckpointAheadOfWal {
            checkpoint: 1,
            wal_tail: 0
        }))
    ));
    assert_eq!(fs::read(&original.wal).unwrap(), wal_before);
    assert_eq!(fs::read(original.checkpoint()).unwrap(), cp_before);
}

#[test]
fn corrupted_checkpoint_and_torn_wal_fail_closed_without_repair() {
    let original = Sandbox::new("corrupt");
    {
        let mut db = WalBackedTablet::open(descriptor(), &original.wal).unwrap();
        put_commit(&mut db, b"k", b"v1");
        db.publish_checkpoint().unwrap();
    }
    let mut cp_bytes = fs::read(original.checkpoint()).unwrap();
    *cp_bytes.last_mut().unwrap() ^= 0x20;
    fs::write(original.checkpoint(), &cp_bytes).unwrap();
    let wal_before = fs::read(&original.wal).unwrap();
    assert!(matches!(
        verify(&original.wal),
        Err(WalBackedError::Checkpoint(_))
    ));
    assert_eq!(fs::read(original.checkpoint()).unwrap(), cp_bytes);
    assert_eq!(fs::read(&original.wal).unwrap(), wal_before);

    let mut wal = OpenOptions::new().append(true).open(&original.wal).unwrap();
    wal.write_all(b"NRE").unwrap();
    drop(wal);
    let wal_before = fs::read(&original.wal).unwrap();
    assert!(matches!(
        verify(&original.wal),
        Err(WalBackedError::Wal(WalError::IncompleteFrame { .. }))
    ));
    assert_eq!(fs::read(&original.wal).unwrap(), wal_before);
    assert_eq!(fs::read(original.checkpoint()).unwrap(), cp_bytes);
}

#[test]
fn incompatible_tablet_descriptor_is_rejected() {
    let original = Sandbox::new("mismatch");
    {
        let mut db = WalBackedTablet::open(descriptor(), &original.wal).unwrap();
        put_commit(&mut db, b"k", b"v1");
        db.publish_checkpoint().unwrap();
    }
    let other_descriptor = TabletDescriptor::new(
        TabletId::new(89).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        4,
    )
    .unwrap();
    assert!(
        WalBackedTablet::verify_checkpoint_wal_read_only(other_descriptor, &original.wal).is_err()
    );
}
