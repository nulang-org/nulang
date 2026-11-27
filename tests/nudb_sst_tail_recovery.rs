#![cfg(unix)]
use nulang::database::{
    sst_manifest::ManifestStore,
    store::WalBackedTablet,
    tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "nudb-sealed-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        fs::create_dir(p.join("sst")).unwrap();
        Self(p)
    }
    fn descriptor(&self) -> TabletDescriptor {
        TabletDescriptor::new(
            TabletId::new(7).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            3,
        )
        .unwrap()
    }
    fn open(&self) -> WalBackedTablet {
        WalBackedTablet::open(self.descriptor(), self.0.join("tablet.wal")).unwrap()
    }
    fn manifest(&self) -> ManifestStore {
        ManifestStore::create(self.0.join("sst"), 7, 3).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn id(n: u8) -> TransactionId {
    TransactionId::new([n; 16])
}
fn put(value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: b"k".to_vec(),
        value: value.to_vec(),
    }
}

fn seal(f: &Fixture, db: &WalBackedTablet) -> ManifestStore {
    let mut manifest = f.manifest();
    db.flush_sst_shadow(&mut manifest, 64).unwrap();
    db.seal_sst_recovery(&mut manifest).unwrap();
    manifest
}
fn recover(f: &Fixture) -> std::io::Result<WalBackedTablet> {
    WalBackedTablet::open_from_sealed_sst(f.descriptor(), f.0.join("tablet.wal"), f.0.join("sst"))
}
#[test]
fn sealed_prefix_and_wal_tail_recover_and_accept_new_writes() {
    let f = Fixture::new();
    let mut db = f.open();
    let r1 = db
        .commit_idempotent(id(1), vec![put(b"before"), put(b"first")])
        .unwrap();
    db.commit_idempotent(id(2), vec![]).unwrap();
    drop(seal(&f, &db));
    let r3 = db
        .commit_idempotent(id(3), vec![TabletMutation::Delete { key: b"k".to_vec() }])
        .unwrap();
    db.commit_idempotent(id(4), vec![put(b"tail")]).unwrap();
    drop(db);
    let bytes = fs::read(f.0.join("tablet.wal")).unwrap();
    let mut db = recover(&f).unwrap();
    assert_eq!(db.current_sequence(), 4);
    assert_eq!(db.read_at(b"k", 1).unwrap(), Some(b"first".as_slice()));
    assert_eq!(db.read_at(b"k", 2).unwrap(), Some(b"first".as_slice()));
    assert_eq!(db.read_at(b"k", 3).unwrap(), None);
    assert_eq!(db.read_latest(b"k"), Some(b"tail".as_slice()));
    assert_eq!(
        db.commit_idempotent(id(1), vec![put(b"before"), put(b"first")])
            .unwrap(),
        r1
    );
    assert_eq!(
        db.commit_idempotent(id(3), vec![TabletMutation::Delete { key: b"k".to_vec() }])
            .unwrap(),
        r3
    );
    assert!(db.commit_idempotent(id(1), vec![put(b"conflict")]).is_err());
    assert_eq!(fs::read(f.0.join("tablet.wal")).unwrap(), bytes);
    db.commit_idempotent(id(5), vec![put(b"new")]).unwrap();
    drop(db);
    let db = recover(&f).unwrap();
    assert_eq!(db.current_sequence(), 5);
    assert_eq!(db.read_latest(b"k"), Some(b"new".as_slice()));
}
#[test]
fn wrong_prefix_with_same_tablet_and_sequence_is_rejected() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"original")]).unwrap();
    drop(seal(&f, &db));
    drop(db);
    let other = f.0.join("other.wal");
    let mut db = WalBackedTablet::open(f.descriptor(), &other).unwrap();
    db.commit_idempotent(id(1), vec![put(b"different")])
        .unwrap();
    drop(db);
    let bytes = fs::read(&other).unwrap();
    assert!(
        WalBackedTablet::open_from_sealed_sst(f.descriptor(), &other, f.0.join("sst")).is_err()
    );
    assert_eq!(fs::read(other).unwrap(), bytes);
}
#[test]
fn receipt_mismatch_with_identical_values_is_rejected() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"same")]).unwrap();
    drop(seal(&f, &db));
    drop(db);
    let other = f.0.join("other.wal");
    let mut db = WalBackedTablet::open(f.descriptor(), &other).unwrap();
    db.commit_idempotent(id(2), vec![put(b"same")]).unwrap();
    drop(db);
    assert!(
        WalBackedTablet::open_from_sealed_sst(f.descriptor(), &other, f.0.join("sst")).is_err()
    );
}
#[test]
fn missing_wal_and_existing_checkpoint_are_not_silently_bypassed() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    drop(seal(&f, &db));
    db.publish_checkpoint().unwrap();
    drop(db);
    assert!(recover(&f).is_err());
    fs::remove_file(f.0.join("tablet.checkpoint")).unwrap();
    fs::remove_file(f.0.join("tablet.wal")).unwrap();
    assert!(recover(&f).is_err());
    assert!(!f.0.join("tablet.wal").exists());
}
#[test]
fn compacted_wal_has_no_seal_binding_and_is_rejected() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    drop(seal(&f, &db));
    db.checkpoint().unwrap();
    drop(db);
    fs::remove_file(f.0.join("tablet.checkpoint")).unwrap();
    assert!(recover(&f).is_err());
}

#[test]
fn torn_tail_recovery_preserves_valid_prefix() {
    use std::io::Write;
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    drop(seal(&f, &db));
    db.commit_idempotent(id(2), vec![put(b"tail")]).unwrap();
    drop(db);
    let path = f.0.join("tablet.wal");
    let expected = fs::read(&path).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"partial")
        .unwrap();
    let mut db = recover(&f).unwrap();
    assert_eq!(fs::read(&path).unwrap(), expected);
    assert_eq!(db.current_sequence(), 2);
    assert_eq!(db.read_latest(b"k"), Some(b"tail".as_slice()));
    assert_eq!(
        db.commit_idempotent(id(3), vec![put(b"next")])
            .unwrap()
            .sequence(),
        3
    );
}

#[test]
fn unsealed_manifest_and_changed_range_are_rejected() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let mut m = f.manifest();
    db.flush_sst_shadow(&mut m, 1).unwrap();
    drop(m);
    drop(db);
    assert!(recover(&f).is_err());
    let db = f.open();
    let mut m = ManifestStore::open(f.0.join("sst"), 7, 3).unwrap();
    db.seal_sst_recovery(&mut m).unwrap();
    drop(m);
    drop(db);
    let other = TabletDescriptor::new(
        TabletId::new(7).unwrap(),
        KeyRange::new(b"b".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap();
    assert!(
        WalBackedTablet::open_from_sealed_sst(other, f.0.join("tablet.wal"), f.0.join("sst"))
            .is_err()
    );
}
