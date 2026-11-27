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

#[test]
fn sealed_prefix_recovers_history_and_receipts_without_wal() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut m = f.manifest();
    let first = db
        .commit_idempotent(id(1), vec![put(b"a"), put(b"b")])
        .unwrap();
    let second = db
        .commit_idempotent(id(2), vec![TabletMutation::Delete { key: b"k".to_vec() }])
        .unwrap();
    let third = db.commit_idempotent(id(3), vec![]).unwrap();
    let w = db.prepare_write(3, 3, vec![put(b"c")]).unwrap();
    db.commit(w).unwrap();
    db.flush_sst_shadow(&mut m, 2).unwrap();
    db.flush_sst_shadow(&mut m, 2).unwrap();
    let wal = fs::read(f.0.join("tablet.wal")).unwrap();
    db.seal_sst_recovery(&mut m).unwrap();
    assert_eq!(fs::read(f.0.join("tablet.wal")).unwrap(), wal);
    assert!(m.current().is_recovery_sealed());
    drop(db);
    drop(m);
    // Only disposable fixture WAL is removed; no production reclaim API.
    fs::remove_file(f.0.join("tablet.wal")).unwrap();
    let m = ManifestStore::open(f.0.join("sst"), 7, 3).unwrap();
    assert_eq!(m.current().recovery_descriptor().unwrap(), &f.descriptor());
    assert_eq!(m.current().recovery_floor(), Some(0));
    assert_eq!(m.recovered_receipt(id(1)).unwrap(), Some(first));
    assert_eq!(m.recovered_receipt(id(2)).unwrap(), Some(second));
    assert_eq!(m.recovered_receipt(id(3)).unwrap(), Some(third));
    assert_eq!(m.recovered_receipt(id(4)).unwrap(), None);
    for (seq, value) in [
        (0, None),
        (1, Some(b"b".as_slice())),
        (2, None),
        (3, None),
        (4, Some(b"c".as_slice())),
    ] {
        assert_eq!(m.recovered_read_at(b"k", seq).unwrap().as_deref(), value);
    }
    assert!(m.recovered_read_at(b"k", 5).is_err());
    assert!(m.recovered_read_at(b"z", 4).is_err());
}
#[test]
fn unsealed_or_incomplete_prefix_cannot_claim_recovery() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut m = f.manifest();
    db.commit_idempotent(id(1), vec![put(b"a")]).unwrap();
    assert!(m.recovered_receipt(id(1)).is_err());
    assert!(m.recovered_read_at(b"k", 0).is_err());
    assert!(db.seal_sst_recovery(&mut m).is_err());
    db.flush_sst_shadow(&mut m, 1).unwrap();
    db.seal_sst_recovery(&mut m).unwrap();
    db.seal_sst_recovery(&mut m).unwrap();
    db.commit_idempotent(id(2), vec![put(b"b")]).unwrap();
    assert!(db.flush_sst_shadow(&mut m, 1).is_err());
    assert!(db.seal_sst_recovery(&mut m).is_err());
    assert_eq!(m.recovered_read_at(b"k", 1).unwrap(), Some(b"a".to_vec()));
}
#[test]
fn seal_refuses_pruned_or_foreign_history() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut m = f.manifest();
    db.commit_idempotent(id(1), vec![put(b"a")]).unwrap();
    db.flush_sst_shadow(&mut m, 1).unwrap();
    db.reclaim_history_before(1).unwrap();
    assert!(db.seal_sst_recovery(&mut m).is_err());
    assert!(!m.current().is_recovery_sealed());
}

#[test]
fn checksummed_sealed_manifest_rejects_invalid_metadata() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut m = f.manifest();
    db.commit_idempotent(id(1), vec![put(b"a")]).unwrap();
    db.commit_idempotent(id(2), vec![]).unwrap();
    db.flush_sst_shadow(&mut m, 2).unwrap();
    db.seal_sst_recovery(&mut m).unwrap();
    drop(db);
    drop(m);
    let p = f.0.join("sst/MANIFEST");
    let original = fs::read(&p).unwrap();
    // One table: 34-byte manifest header + 64-byte table entry.
    // Metadata: floor(8), start length(4), start(1), end tag(1),
    // end length(4), end(1), receipt count(8), 56-byte receipts.
    let meta = 98usize;
    let receipt = meta + 27;
    for variant in 0..6 {
        let mut body = original[..original.len() - 32].to_vec();
        match variant {
            0 => body.truncate(meta),
            1 => body[meta..meta + 8].copy_from_slice(&1u64.to_le_bytes()),
            2 => body[meta + 19..meta + 27].copy_from_slice(&u64::MAX.to_le_bytes()),
            3 => body[receipt + 16..receipt + 24].copy_from_slice(&0u64.to_le_bytes()),
            4 => body[receipt + 56 + 16..receipt + 56 + 24].copy_from_slice(&1u64.to_le_bytes()),
            5 => body[8..10].copy_from_slice(&1u16.to_le_bytes()),
            _ => unreachable!(),
        }
        let hash = blake3::hash(&body);
        body.extend_from_slice(hash.as_bytes());
        fs::write(&p, body).unwrap();
        assert!(
            ManifestStore::open(f.0.join("sst"), 7, 3).is_err(),
            "variant {variant}"
        );
    }
    fs::write(&p, original).unwrap();
    assert!(ManifestStore::open(f.0.join("sst"), 7, 3).is_ok());
}
