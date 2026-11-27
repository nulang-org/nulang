#![cfg(unix)]
use nulang::database::{
    indexed_sst::{write_immutable_table, DiskLookup, IndexedSst, SstHeader},
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
            "nudb-shadow-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        fs::create_dir(p.join("sst")).unwrap();
        Self(p)
    }
    fn open(&self) -> WalBackedTablet {
        WalBackedTablet::open(descriptor(), self.0.join("tablet.wal")).unwrap()
    }
    fn manifest(&self) -> ManifestStore {
        ManifestStore::create(self.0.join("sst"), 7, 3).unwrap()
    }
    fn path(&self, g: u64) -> PathBuf {
        self.0.join("sst").join(format!("nudb-sst-{g:020}.sst"))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(7).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap()
}
fn put(k: &[u8], v: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: k.to_vec(),
        value: v.to_vec(),
    }
}
fn id(n: u8) -> TransactionId {
    TransactionId::new([n; 16])
}

#[test]
fn bounded_shadow_flush_preserves_history_receipts_and_wal() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut m = f.manifest();
    let first = db
        .commit_idempotent(id(1), vec![put(b"k", b"old"), put(b"k", b"new")])
        .unwrap();
    db.commit_idempotent(id(2), vec![TabletMutation::Delete { key: b"k".to_vec() }])
        .unwrap();
    db.commit_idempotent(id(3), vec![]).unwrap();
    db.commit_idempotent(id(4), vec![put(b"k", b"again")])
        .unwrap();
    let before = fs::read(f.0.join("tablet.wal")).unwrap();
    assert_eq!(
        db.flush_sst_shadow(&mut m, 2)
            .unwrap()
            .unwrap()
            .sequence_interval(),
        (1, 2)
    );
    assert_eq!(
        db.flush_sst_shadow(&mut m, 2)
            .unwrap()
            .unwrap()
            .sequence_interval(),
        (3, 4)
    );
    assert!(db.flush_sst_shadow(&mut m, 2).unwrap().is_none());
    assert_eq!(fs::read(f.0.join("tablet.wal")).unwrap(), before);
    let mut t = IndexedSst::open(f.path(1)).unwrap();
    assert_eq!(
        t.read_at(b"k", 1).unwrap(),
        DiskLookup::Value {
            sequence: 1,
            value: b"new".to_vec()
        }
    );
    assert_eq!(
        t.read_at(b"k", 2).unwrap(),
        DiskLookup::Deleted { sequence: 2 }
    );
    drop(db);
    drop(m);
    let mut db = f.open();
    let mut m = ManifestStore::open(f.0.join("sst"), 7, 3).unwrap();
    assert!(db.flush_sst_shadow(&mut m, 2).unwrap().is_none());
    assert_eq!(
        db.commit_idempotent(id(1), vec![put(b"k", b"old"), put(b"k", b"new")])
            .unwrap(),
        first
    );
    assert_eq!(db.current_sequence(), 4);
}

#[test]
fn valid_unpublished_table_is_reconciled_but_mismatch_is_rejected() {
    for correct in [true, false] {
        let f = Fixture::new();
        let mut db = f.open();
        let mut m = f.manifest();
        db.commit_idempotent(id(1), vec![put(b"k", b"actual")])
            .unwrap();
        let value = if correct {
            b"actual".as_slice()
        } else {
            b"wrong".as_slice()
        };
        let history = [(1, Some(value))];
        write_immutable_table(
            f.path(1),
            SstHeader {
                generation: 1,
                min_sequence: 1,
                max_sequence: 1,
                key_count: 1,
                version_count: 1,
            },
            [(b"k".as_slice(), history.as_slice())],
        )
        .unwrap();
        let bytes = fs::read(f.path(1)).unwrap();
        let result = db.flush_sst_shadow(&mut m, 1);
        assert_eq!(result.is_ok(), correct);
        assert_eq!(m.current().durable_sequence(), u64::from(correct));
        assert_eq!(fs::read(f.path(1)).unwrap(), bytes);
    }
}

#[test]
fn already_published_prefix_must_match_current_wal() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut m = f.manifest();
    db.commit_idempotent(id(1), vec![put(b"k", b"actual")])
        .unwrap();
    let history = [(1, Some(b"wrong".as_slice()))];
    write_immutable_table(
        f.path(1),
        SstHeader {
            generation: 1,
            min_sequence: 1,
            max_sequence: 1,
            key_count: 1,
            version_count: 1,
        },
        [(b"k".as_slice(), history.as_slice())],
    )
    .unwrap();
    m.append(nulang::database::sst_manifest::TableEntry::inspect(f.0.join("sst"), 1).unwrap())
        .unwrap();
    assert!(db.flush_sst_shadow(&mut m, 1).is_err());
}

#[test]
fn reclaimed_wal_and_foreign_manifest_are_rejected() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"k", b"value")])
        .unwrap();
    let mut foreign = ManifestStore::create(f.0.join("sst"), 8, 3).unwrap();
    assert!(db.flush_sst_shadow(&mut foreign, 1).is_err());
    drop(foreign);
    fs::create_dir(f.0.join("other")).unwrap();
    let mut m = ManifestStore::create(f.0.join("other"), 7, 3).unwrap();
    db.checkpoint().unwrap();
    assert!(db.flush_sst_shadow(&mut m, 1).is_err());
}

#[test]
fn verified_shadow_reads_match_every_snapshot_across_intervals() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut m = f.manifest();
    let commits = [
        vec![put(b"k", b"first"), put(b"k", b"last")],
        vec![put(b"j", b"")],
        vec![TabletMutation::Delete { key: b"k".to_vec() }],
        vec![],
        vec![put(b"k", b"again")],
        vec![TabletMutation::Delete { key: b"q".to_vec() }],
    ];
    for (i, mutations) in commits.into_iter().enumerate() {
        db.commit_idempotent(id(i as u8 + 1), mutations).unwrap();
    }
    db.flush_sst_shadow(&mut m, 2).unwrap();
    assert!(db.read_sst_shadow_at(&m, b"k", 3).is_err());
    while db.flush_sst_shadow(&mut m, 2).unwrap().is_some() {}
    let before = fs::read(f.0.join("tablet.wal")).unwrap();
    let manifest_before = fs::read(f.0.join("sst/MANIFEST")).unwrap();
    let check = |db: &WalBackedTablet, m: &ManifestStore| {
        for snapshot in 0..=6 {
            for key in [b"j", b"k", b"q", b"x"] {
                let got = db.read_sst_shadow_at(m, key, snapshot).unwrap();
                let want = match (key, snapshot) {
                    (b"j", 2..=6) => DiskLookup::Value {
                        sequence: 2,
                        value: vec![],
                    },
                    (b"k", 1..=2) => DiskLookup::Value {
                        sequence: 1,
                        value: b"last".to_vec(),
                    },
                    (b"k", 3..=4) => DiskLookup::Deleted { sequence: 3 },
                    (b"k", 5..=6) => DiskLookup::Value {
                        sequence: 5,
                        value: b"again".to_vec(),
                    },
                    (b"q", 6) => DiskLookup::Deleted { sequence: 6 },
                    _ => DiskLookup::Missing,
                };
                assert_eq!(got, want, "key={key:?} snapshot={snapshot}");
            }
        }
        assert!(db.read_sst_shadow_at(m, b"k", 7).is_err());
        assert!(db.read_sst_shadow_at(m, b"z", 1).is_err());
    };
    check(&db, &m);
    drop(db);
    drop(m);
    let db = f.open();
    let m = ManifestStore::open(f.0.join("sst"), 7, 3).unwrap();
    check(&db, &m);
    assert_eq!(fs::read(f.0.join("tablet.wal")).unwrap(), before);
    assert_eq!(fs::read(f.0.join("sst/MANIFEST")).unwrap(), manifest_before);
}

#[test]
fn shadow_read_rejects_wrong_sequence_even_when_value_matches() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut m = f.manifest();
    db.commit_idempotent(id(1), vec![put(b"k", b"same")])
        .unwrap();
    db.commit_idempotent(id(2), vec![]).unwrap();
    let history = [(2, Some(b"same".as_slice()))];
    write_immutable_table(
        f.path(1),
        SstHeader {
            generation: 1,
            min_sequence: 1,
            max_sequence: 2,
            key_count: 1,
            version_count: 1,
        },
        [(b"k".as_slice(), history.as_slice())],
    )
    .unwrap();
    m.append(nulang::database::sst_manifest::TableEntry::inspect(f.0.join("sst"), 1).unwrap())
        .unwrap();
    assert!(db.read_sst_shadow_at(&m, b"k", 2).is_err());
    assert_eq!(db.read_at(b"k", 2).unwrap(), Some(b"same".as_slice()));
}

#[test]
fn shadow_read_fails_on_corrupt_table_instead_of_falling_back() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut m = f.manifest();
    db.commit_idempotent(id(1), vec![put(b"k", b"old")])
        .unwrap();
    db.commit_idempotent(id(2), vec![put(b"k", b"new")])
        .unwrap();
    db.flush_sst_shadow(&mut m, 1).unwrap();
    db.flush_sst_shadow(&mut m, 1).unwrap();
    let mut bytes = fs::read(f.path(2)).unwrap();
    bytes[72] ^= 1;
    fs::write(f.path(2), bytes).unwrap();
    assert!(db.read_sst_shadow_at(&m, b"k", 2).is_err());
    assert_eq!(
        db.read_sst_shadow_at(&m, b"k", 1).unwrap(),
        DiskLookup::Value {
            sequence: 1,
            value: b"old".to_vec()
        }
    );
    assert_eq!(db.read_at(b"k", 2).unwrap(), Some(b"new".as_slice()));
}

#[test]
fn shadow_read_refuses_foreign_manifest_and_reclaimed_wal() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut m = f.manifest();
    db.commit_idempotent(id(1), vec![put(b"k", b"v")]).unwrap();
    db.flush_sst_shadow(&mut m, 1).unwrap();
    fs::create_dir(f.0.join("foreign")).unwrap();
    let foreign = ManifestStore::create(f.0.join("foreign"), 8, 3).unwrap();
    assert!(db.read_sst_shadow_at(&foreign, b"k", 0).is_err());
    db.checkpoint().unwrap();
    assert!(db.read_sst_shadow_at(&m, b"k", 1).is_err());
}
