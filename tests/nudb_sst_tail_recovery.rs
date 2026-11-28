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

#[test]
fn checkpoint_and_sealed_tail_recovery_preserve_equivalent_history_and_receipts() {
    use nulang::database::indexed_sst::DiskLookup;
    let a = Fixture::new();
    let b = Fixture::new();
    let mut disk = a.open();
    let mut checkpoint = b.open();
    let mutations = [
        vec![put(b"first"), put(b"last")],
        vec![],
        vec![TabletMutation::Delete { key: b"k".to_vec() }],
        vec![put(b"")],
        vec![put(b"tail")],
    ];
    let mut receipts = Vec::new();
    for (i, writes) in mutations.iter().enumerate() {
        let tx = id(i as u8 + 1);
        let r = disk.commit_idempotent(tx, writes.clone()).unwrap();
        assert_eq!(checkpoint.commit_idempotent(tx, writes.clone()).unwrap(), r);
        receipts.push(r);
        if i == 2 {
            drop(seal(&a, &disk));
            checkpoint.checkpoint().unwrap();
        }
    }
    drop(disk);
    drop(checkpoint);
    let before = fs::read(a.0.join("tablet.wal")).unwrap();
    let mut disk = recover(&a).unwrap();
    let mut checkpoint = b.open();
    let manifest = ManifestStore::open(a.0.join("sst"), 7, 3).unwrap();
    for snapshot in 0..=5 {
        for key in [b"k", b"x"] {
            assert_eq!(
                disk.read_at(key, snapshot).unwrap(),
                checkpoint.read_at(key, snapshot).unwrap()
            );
        }
    }
    assert_eq!(
        disk.read_sst_shadow_at(&manifest, b"k", 2).unwrap(),
        DiskLookup::Value {
            sequence: 1,
            value: b"last".to_vec()
        }
    );
    assert_eq!(
        disk.read_sst_shadow_at(&manifest, b"k", 3).unwrap(),
        DiskLookup::Deleted { sequence: 3 }
    );
    assert!(disk.read_sst_shadow_at(&manifest, b"k", 4).is_err());
    for (i, writes) in mutations.iter().enumerate() {
        let tx = id(i as u8 + 1);
        assert_eq!(
            disk.commit_idempotent(tx, writes.clone()).unwrap(),
            receipts[i]
        );
        assert_eq!(
            checkpoint.commit_idempotent(tx, writes.clone()).unwrap(),
            receipts[i]
        );
    }
    assert_eq!(fs::read(a.0.join("tablet.wal")).unwrap(), before);
    assert_eq!(disk.current_sequence(), 5);
    assert_eq!(checkpoint.current_sequence(), 5);
}

// Independent on-disk fixture: no production writer or reclamation API.
fn bound_fixture(f: &Fixture, db: &mut WalBackedTablet) -> Vec<u8> {
    db.commit_idempotent(id(1), vec![put(b"sealed")]).unwrap();
    drop(seal(f, db));
    let prefix_len = fs::metadata(f.0.join("tablet.wal")).unwrap().len() as usize;
    db.commit_idempotent(id(2), vec![put(b"tail")]).unwrap();
    let full = fs::read(f.0.join("tablet.wal")).unwrap();
    let manifest = fs::read(f.0.join("sst/MANIFEST")).unwrap();
    let mut bound = b"NUDBWAL4".to_vec();
    for n in [1u64, 7, 3] {
        bound.extend_from_slice(&n.to_le_bytes());
    }
    bound.extend_from_slice(&manifest[manifest.len() - 32..]);
    let checksum = blake3::hash(&bound);
    bound.extend_from_slice(checksum.as_bytes());
    bound.extend_from_slice(&full[prefix_len..]);
    bound
}

#[test]
fn bound_wal_recovers_only_with_exact_seal_and_preserves_receipts() {
    let f = Fixture::new();
    let mut db = f.open();
    let bytes = bound_fixture(&f, &mut db);
    drop(db);
    let path = f.0.join("tablet.wal");
    fs::write(&path, &bytes).unwrap();
    assert!(WalBackedTablet::open(f.descriptor(), &path).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let mut db = recover(&f).unwrap();
    assert_eq!(db.current_sequence(), 2);
    assert_eq!(db.read_at(b"k", 1).unwrap(), Some(b"sealed".as_slice()));
    assert_eq!(db.read_latest(b"k"), Some(b"tail".as_slice()));
    assert_eq!(
        db.commit_idempotent(id(1), vec![put(b"sealed")])
            .unwrap()
            .sequence(),
        1
    );
    assert_eq!(
        db.commit_idempotent(id(2), vec![put(b"tail")])
            .unwrap()
            .sequence(),
        2
    );
    assert!(db.commit_idempotent(id(1), vec![put(b"wrong")]).is_err());
    assert!(db.publish_checkpoint().is_err());
    assert!(db.checkpoint().is_err());
    assert!(db.reclaim_history_before(1).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(!f.0.join("tablet.checkpoint").exists());
    db.commit_idempotent(id(3), vec![put(b"new")]).unwrap();
    drop(db);
    let db = recover(&f).unwrap();
    assert_eq!(db.current_sequence(), 3);
    assert_eq!(db.read_latest(b"k"), Some(b"new".as_slice()));
}

#[test]
fn mismatched_bound_headers_reject_before_tail_repair() {
    let f = Fixture::new();
    let mut db = f.open();
    let bytes = bound_fixture(&f, &mut db);
    drop(db);
    let path = f.0.join("tablet.wal");
    for offset in [8, 16, 24, 32] {
        let mut wrong = bytes.clone();
        wrong[offset] ^= 1;
        let hash = blake3::hash(&wrong[..64]);
        wrong[64..96].copy_from_slice(hash.as_bytes());
        wrong.extend_from_slice(b"partial");
        fs::write(&path, &wrong).unwrap();
        assert!(recover(&f).is_err(), "offset {offset}");
        assert_eq!(fs::read(&path).unwrap(), wrong);
    }
    let mut wrong = bytes.clone();
    wrong[64] ^= 1;
    fs::write(&path, &wrong).unwrap();
    assert!(recover(&f).is_err());
    assert_eq!(fs::read(&path).unwrap(), wrong);
    let mut torn = bytes.clone();
    torn.extend_from_slice(b"partial");
    fs::write(&path, &torn).unwrap();
    let db = recover(&f).unwrap();
    assert_eq!(db.current_sequence(), 2);
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn valid_but_different_seal_with_same_identity_cannot_open_bound_wal() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut bytes = bound_fixture(&f, &mut db);
    drop(db);
    bytes.extend_from_slice(b"partial");
    let path = f.0.join("tablet.wal");
    fs::write(&path, &bytes).unwrap();
    let other = Fixture::new();
    let mut db = other.open();
    // Same value, sequence, tablet and epoch; different durable receipt.
    db.commit_idempotent(id(9), vec![put(b"sealed")]).unwrap();
    drop(seal(&other, &db));
    drop(db);
    assert!(
        WalBackedTablet::open_from_sealed_sst(f.descriptor(), &path, other.0.join("sst")).is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn bound_header_without_tail_recovers_at_sealed_sequence() {
    let f = Fixture::new();
    let mut db = f.open();
    let bytes = bound_fixture(&f, &mut db);
    drop(db);
    let path = f.0.join("tablet.wal");
    fs::write(&path, &bytes[..96]).unwrap();
    let mut db = recover(&f).unwrap();
    assert_eq!(db.current_sequence(), 1);
    assert_eq!(db.read_latest(b"k"), Some(b"sealed".as_slice()));
    assert_eq!(
        db.commit_idempotent(id(2), vec![put(b"fresh")])
            .unwrap()
            .sequence(),
        2
    );
    drop(db);
    let db = recover(&f).unwrap();
    assert_eq!(db.read_latest(b"k"), Some(b"fresh".as_slice()));
}

#[test]
fn explicit_reclaim_publishes_bound_header_and_recovers_receipts() {
    let f = Fixture::new();
    let mut db = f.open();
    let first = db
        .commit_idempotent(id(1), vec![put(b"first"), put(b"last")])
        .unwrap();
    db.commit_idempotent(id(2), vec![TabletMutation::Delete { key: b"k".to_vec() }])
        .unwrap();
    db.commit_idempotent(id(3), vec![]).unwrap();
    let manifest = seal(&f, &db);
    let path = f.0.join("tablet.wal");
    db.reclaim_to_sealed_sst(&manifest).unwrap();
    let bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.len(), 96);
    assert_eq!(&bytes[..8], b"NUDBWAL4");
    assert!(WalBackedTablet::open(f.descriptor(), &path).is_err());
    assert!(ManifestStore::open(f.0.join("sst"), 7, 3).is_err());
    assert_eq!(
        db.commit_idempotent(id(1), vec![put(b"first"), put(b"last")])
            .unwrap(),
        first
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(db.read_at(b"k", 1).unwrap(), Some(b"last".as_slice()));
    assert_eq!(db.read_at(b"k", 3).unwrap(), None);
    assert!(db.checkpoint().is_err());
    assert!(db.reclaim_to_sealed_sst(&manifest).is_err());
    db.commit_idempotent(id(4), vec![put(b"tail")]).unwrap();
    drop(db);
    drop(manifest);
    assert!(WalBackedTablet::open(f.descriptor(), &path).is_err());
    let mut db = recover(&f).unwrap();
    assert_eq!(db.current_sequence(), 4);
    assert_eq!(db.read_latest(b"k"), Some(b"tail".as_slice()));
    assert_eq!(
        db.commit_idempotent(id(1), vec![put(b"first"), put(b"last")])
            .unwrap(),
        first
    );
}

#[test]
fn reclaim_rejects_stale_seal_checkpoint_and_foreign_receipts_without_mutation() {
    for scenario in 0..3 {
        let f = Fixture::new();
        let mut db = f.open();
        db.commit_idempotent(id(1), vec![put(b"v")]).unwrap();
        let other = Fixture::new();
        let m = if scenario == 2 {
            let mut foreign = other.open();
            foreign.commit_idempotent(id(9), vec![put(b"v")]).unwrap();
            seal(&other, &foreign)
        } else {
            seal(&f, &db)
        };
        if scenario == 0 {
            db.commit_idempotent(id(2), vec![put(b"tail")]).unwrap();
        }
        if scenario == 1 {
            db.publish_checkpoint().unwrap();
        }
        let path = f.0.join("tablet.wal");
        let before = fs::read(&path).unwrap();
        assert!(db.reclaim_to_sealed_sst(&m).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        db.commit_idempotent(id(3), vec![put(b"still writable")])
            .unwrap();
    }
}

#[test]
fn reclaim_revalidates_sealed_manifest_on_disk_before_discarding_wal() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"v")]).unwrap();
    let m = seal(&f, &db);
    let manifest_path = f.0.join("sst/MANIFEST");
    let mut corrupt = fs::read(&manifest_path).unwrap();
    corrupt[12] ^= 1;
    fs::write(&manifest_path, corrupt).unwrap();
    let path = f.0.join("tablet.wal");
    let before = fs::read(&path).unwrap();
    assert!(db.reclaim_to_sealed_sst(&m).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    db.commit_idempotent(id(2), vec![put(b"still writable")])
        .unwrap();
}

#[test]
fn empty_seal_does_not_reclaim_empty_wal() {
    let f = Fixture::new();
    let mut db = f.open();
    let mut manifest = f.manifest();
    db.seal_sst_recovery(&mut manifest).unwrap();
    let path = f.0.join("tablet.wal");
    let before = fs::read(&path).unwrap();
    assert!(db.reclaim_to_sealed_sst(&manifest).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    db.commit_idempotent(id(1), vec![]).unwrap();
}

#[test]
fn sealed_floor_takes_effect_only_on_bound_cutover_and_preserves_receipts() {
    let f = Fixture::new();
    let mut db = f.open();
    let r1 = db.commit_idempotent(id(1), vec![put(b"old")]).unwrap();
    db.commit_idempotent(id(2), vec![put(b"anchor")]).unwrap();
    db.commit_idempotent(id(3), vec![]).unwrap();
    db.commit_idempotent(id(4), vec![TabletMutation::Delete { key: b"k".to_vec() }])
        .unwrap();
    db.commit_idempotent(id(5), vec![put(b"new")]).unwrap();
    let mut m = f.manifest();
    while db.flush_sst_shadow(&mut m, 2).unwrap().is_some() {}
    let pin = db.pin_snapshot(1).unwrap();
    assert!(db.seal_sst_recovery_at_floor(&mut m, 3).is_err());
    assert!(db.seal_sst_recovery_at_floor(&mut m, 6).is_err());
    assert!(!m.current().is_recovery_sealed());
    drop(pin);
    db.seal_sst_recovery_at_floor(&mut m, 3).unwrap();
    assert_eq!(db.oldest_retained_snapshot(), 0);
    assert_eq!(m.current().recovery_floor(), Some(3));
    assert!(m.recovered_read_at(b"k", 2).is_err());
    assert_eq!(
        m.recovered_read_at(b"k", 3).unwrap(),
        Some(b"anchor".to_vec())
    );
    assert_eq!(m.recovered_read_at(b"k", 4).unwrap(), None);
    assert_eq!(m.recovered_receipt(id(1)).unwrap(), Some(r1));
    let path = f.0.join("tablet.wal");
    let before = fs::read(&path).unwrap();
    let pin = db.pin_snapshot(2).unwrap();
    assert!(db.reclaim_to_sealed_sst(&m).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(pin);
    drop(db);
    drop(m);
    assert!(recover(&f).is_err());
    let mut db = f.open();
    assert_eq!(db.oldest_retained_snapshot(), 0);
    let m = ManifestStore::open(f.0.join("sst"), 7, 3).unwrap();
    db.reclaim_to_sealed_sst(&m).unwrap();
    assert_eq!(db.oldest_retained_snapshot(), 3);
    assert!(db.read_at(b"k", 2).is_err());
    assert_eq!(db.read_at(b"k", 3).unwrap(), Some(b"anchor".as_slice()));
    assert_eq!(db.read_at(b"k", 4).unwrap(), None);
    assert_eq!(db.commit_idempotent(id(1), vec![put(b"old")]).unwrap(), r1);
    db.commit_idempotent(id(6), vec![put(b"tail")]).unwrap();
    drop(db);
    drop(m);
    let mut db = recover(&f).unwrap();
    assert_eq!(db.oldest_retained_snapshot(), 3);
    assert!(db.read_at(b"k", 2).is_err());
    assert_eq!(db.read_at(b"k", 3).unwrap(), Some(b"anchor".as_slice()));
    assert_eq!(db.read_at(b"k", 4).unwrap(), None);
    assert_eq!(db.read_latest(b"k"), Some(b"tail".as_slice()));
    assert_eq!(db.commit_idempotent(id(1), vec![put(b"old")]).unwrap(), r1);
    assert!(db.pin_snapshot(2).is_err());
}

#[test]
fn nonzero_floor_format_rejects_zero_future_and_version_downgrade() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"v")]).unwrap();
    let mut m = f.manifest();
    db.flush_sst_shadow(&mut m, 1).unwrap();
    db.seal_sst_recovery_at_floor(&mut m, 1).unwrap();
    drop(m);
    drop(db);
    let p = f.0.join("sst/MANIFEST");
    let original = fs::read(&p).unwrap();
    assert_eq!(&original[8..10], &3u16.to_le_bytes());
    for variant in 0..3 {
        let mut bytes = original[..original.len() - 32].to_vec();
        match variant {
            0 => bytes[98..106].copy_from_slice(&0u64.to_le_bytes()),
            1 => bytes[98..106].copy_from_slice(&2u64.to_le_bytes()),
            _ => bytes[8..10].copy_from_slice(&2u16.to_le_bytes()),
        }
        let hash = blake3::hash(&bytes);
        bytes.extend_from_slice(hash.as_bytes());
        fs::write(&p, &bytes).unwrap();
        assert!(ManifestStore::open(f.0.join("sst"), 7, 3).is_err());
    }
    fs::write(&p, original).unwrap();
    assert_eq!(
        ManifestStore::open(f.0.join("sst"), 7, 3)
            .unwrap()
            .current()
            .recovery_floor(),
        Some(1)
    );
}

#[test]
fn successor_seals_repeat_reclamation_without_losing_history_or_receipts() {
    let f = Fixture::new();
    let mut db = f.open();
    let old = db.commit_idempotent(id(1), vec![put(b"old")]).unwrap();
    db.commit_idempotent(id(2), vec![put(b"anchor")]).unwrap();
    let mut first = f.manifest();
    db.flush_sst_shadow(&mut first, 2).unwrap();
    db.seal_sst_recovery_at_floor(&mut first, 1).unwrap();
    db.reclaim_to_sealed_sst(&first).unwrap();
    let mut previous = first;
    for cycle in 0..2u8 {
        let base = db.current_sequence();
        let tx = 3 + cycle * 2;
        db.commit_idempotent(id(tx), vec![TabletMutation::Delete { key: b"k".to_vec() }])
            .unwrap();
        db.commit_idempotent(id(tx + 1), vec![put(b"new")]).unwrap();
        let path = f.0.join(format!("successor-{cycle}"));
        fs::create_dir(&path).unwrap();
        let mut next = ManifestStore::create(&path, 7, 3).unwrap();
        let wal = f.0.join("tablet.wal");
        let before = fs::read(&wal).unwrap();
        let pin = db.pin_snapshot(base - 1).unwrap();
        assert!(db
            .prepare_sst_successor(&previous, &mut next, base)
            .is_err());
        drop(pin);
        db.prepare_sst_successor(&previous, &mut next, base)
            .unwrap();
        assert_eq!(fs::read(&wal).unwrap(), before);
        assert_eq!(
            db.oldest_retained_snapshot(),
            previous.current().recovery_floor().unwrap()
        );
        assert_eq!(next.current().recovery_floor(), Some(base));
        assert_eq!(next.recovered_read_at(b"k", base + 1).unwrap(), None);
        let pin = db.pin_snapshot(base - 1).unwrap();
        assert!(db.reclaim_to_sst_successor(&previous, &next).is_err());
        drop(pin);
        db.reclaim_to_sst_successor(&previous, &next).unwrap();
        assert_eq!(fs::metadata(&wal).unwrap().len(), 96);
        assert_eq!(db.oldest_retained_snapshot(), base);
        assert_eq!(db.commit_idempotent(id(1), vec![put(b"old")]).unwrap(), old);
        drop(previous);
        drop(db);
        drop(next);
        db = WalBackedTablet::open_from_sealed_sst(f.descriptor(), &wal, &path).unwrap();
        assert_eq!(db.current_sequence(), base + 2);
        assert!(db.read_at(b"k", base - 1).is_err());
        assert_eq!(db.read_at(b"k", base + 1).unwrap(), None);
        assert_eq!(db.read_latest(b"k"), Some(b"new".as_slice()));
        previous = ManifestStore::open(&path, 7, 3).unwrap();
    }
}

#[test]
fn successor_preparation_preserves_original_binding_and_stale_successor_is_rejected() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let first = seal(&f, &db);
    db.reclaim_to_sealed_sst(&first).unwrap();
    db.commit_idempotent(id(2), vec![put(b"tail")]).unwrap();
    let next_path = f.0.join("next");
    fs::create_dir(&next_path).unwrap();
    let mut next = ManifestStore::create(&next_path, 7, 3).unwrap();
    db.prepare_sst_successor(&first, &mut next, 0).unwrap();
    drop(db);
    drop(first);
    drop(next);
    let wal = f.0.join("tablet.wal");
    let before = fs::read(&wal).unwrap();
    assert!(WalBackedTablet::open_from_sealed_sst(f.descriptor(), &wal, &next_path).is_err());
    assert_eq!(fs::read(&wal).unwrap(), before);
    let mut db = recover(&f).unwrap();
    let first = ManifestStore::open(f.0.join("sst"), 7, 3).unwrap();
    let next = ManifestStore::open(&next_path, 7, 3).unwrap();
    db.commit_idempotent(id(3), vec![]).unwrap();
    let before = fs::read(&wal).unwrap();
    assert!(db.reclaim_to_sst_successor(&first, &next).is_err());
    assert_eq!(fs::read(&wal).unwrap(), before);
    db.commit_idempotent(id(4), vec![]).unwrap();
}

#[test]
fn backup_restores_independent_bound_snapshot_with_tail_and_receipts() {
    let f = Fixture::new();
    let mut db = f.open();
    let r1 = db.commit_idempotent(id(1), vec![put(b"old")]).unwrap();
    db.commit_idempotent(id(2), vec![put(b"anchor")]).unwrap();
    let mut m = f.manifest();
    db.flush_sst_shadow(&mut m, 2).unwrap();
    db.seal_sst_recovery_at_floor(&mut m, 2).unwrap();
    db.reclaim_to_sealed_sst(&m).unwrap();
    db.commit_idempotent(id(3), vec![TabletMutation::Delete { key: b"k".to_vec() }])
        .unwrap();
    db.commit_idempotent(id(4), vec![put(b"tail")]).unwrap();
    let backup = f.0.join("backup");
    let restore = f.0.join("restored");
    let live_wal = f.0.join("tablet.wal");
    let before = fs::read(&live_wal).unwrap();
    db.backup_sealed(&m, &backup).unwrap();
    assert_eq!(fs::read(&live_wal).unwrap(), before);
    assert!(db.backup_sealed(&m, &backup).is_err());
    db.commit_idempotent(id(5), vec![put(b"later")]).unwrap();
    let mut recovered =
        WalBackedTablet::restore_sealed_backup(f.descriptor(), &backup, &restore).unwrap();
    assert_eq!(recovered.current_sequence(), 4);
    assert_eq!(recovered.oldest_retained_snapshot(), 2);
    assert!(recovered.read_at(b"k", 1).is_err());
    assert_eq!(
        recovered.read_at(b"k", 2).unwrap(),
        Some(b"anchor".as_slice())
    );
    assert_eq!(recovered.read_at(b"k", 3).unwrap(), None);
    assert_eq!(recovered.read_latest(b"k"), Some(b"tail".as_slice()));
    assert_eq!(
        recovered
            .commit_idempotent(id(1), vec![put(b"old")])
            .unwrap(),
        r1
    );
    recovered
        .commit_idempotent(id(5), vec![put(b"restored")])
        .unwrap();
    assert_eq!(db.read_latest(b"k"), Some(b"later".as_slice()));
    drop(recovered);
    let second = f.0.join("restored-again");
    let recovered =
        WalBackedTablet::restore_sealed_backup(f.descriptor(), &backup, &second).unwrap();
    assert_eq!(recovered.current_sequence(), 4);
    assert_eq!(recovered.read_latest(b"k"), Some(b"tail".as_slice()));
}

#[test]
fn backup_restore_rejects_missing_completion_and_wal_truncation_without_repair() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let m = seal(&f, &db);
    db.reclaim_to_sealed_sst(&m).unwrap();
    db.commit_idempotent(id(2), vec![put(b"tail")]).unwrap();
    let backup = f.0.join("backup");
    db.backup_sealed(&m, &backup).unwrap();
    let marker = backup.join("COMPLETE");
    let complete = fs::read(&marker).unwrap();
    fs::remove_file(&marker).unwrap();
    assert!(
        WalBackedTablet::restore_sealed_backup(f.descriptor(), &backup, f.0.join("missing"))
            .is_err()
    );
    fs::write(&marker, complete).unwrap();
    let path = backup.join("tablet.wal");
    let full = fs::read(&path).unwrap();
    for (i, damaged) in [full[..full.len() - 1].to_vec(), full[..96].to_vec(), {
        let mut v = full.clone();
        v[100] ^= 1;
        v
    }]
    .into_iter()
    .enumerate()
    {
        fs::write(&path, &damaged).unwrap();
        assert!(WalBackedTablet::restore_sealed_backup(
            f.descriptor(),
            &backup,
            f.0.join(format!("bad-{i}"))
        )
        .is_err());
        assert_eq!(fs::read(&path).unwrap(), damaged);
    }
    fs::write(&path, full).unwrap();
    assert!(
        WalBackedTablet::restore_sealed_backup(f.descriptor(), &backup, f.0.join("good")).is_ok()
    );
}

#[test]
fn backup_rejects_wrong_generation_corrupt_seal_and_marker() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let first = seal(&f, &db);
    db.reclaim_to_sealed_sst(&first).unwrap();
    db.commit_idempotent(id(2), vec![put(b"tail")]).unwrap();
    let next_path = f.0.join("next");
    fs::create_dir(&next_path).unwrap();
    let mut next = ManifestStore::create(&next_path, 7, 3).unwrap();
    db.prepare_sst_successor(&first, &mut next, 1).unwrap();
    db.reclaim_to_sst_successor(&first, &next).unwrap();
    let wrong = f.0.join("wrong");
    assert!(db.backup_sealed(&first, &wrong).is_err());
    assert!(!wrong.exists());
    let backup = f.0.join("backup");
    db.backup_sealed(&next, &backup).unwrap();
    let marker = backup.join("COMPLETE");
    let complete = fs::read(&marker).unwrap();
    let mut bad = complete.clone();
    bad[32] ^= 1;
    fs::write(&marker, bad).unwrap();
    assert!(WalBackedTablet::restore_sealed_backup(
        f.descriptor(),
        &backup,
        f.0.join("bad-marker")
    )
    .is_err());
    fs::write(&marker, complete).unwrap();
    let table = backup.join("sst/nudb-sst-00000000000000000001.sst");
    let original = fs::read(&table).unwrap();
    let mut bad = original.clone();
    bad[55] ^= 1;
    fs::write(&table, &bad).unwrap();
    assert!(
        WalBackedTablet::restore_sealed_backup(f.descriptor(), &backup, f.0.join("bad-table"))
            .is_err()
    );
    assert_eq!(fs::read(&table).unwrap(), bad);
    fs::write(&table, original).unwrap();
    let restored =
        WalBackedTablet::restore_sealed_backup(f.descriptor(), &backup, f.0.join("good")).unwrap();
    assert_eq!(restored.current_sequence(), 2);
    assert_eq!(restored.oldest_retained_snapshot(), 1);
}

#[test]
fn quarantine_requires_current_backup_and_preserves_live_and_archived_generations() {
    let f = Fixture::new();
    let mut db = f.open();
    let receipt = db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let old = seal(&f, &db);
    db.reclaim_to_sealed_sst(&old).unwrap();
    db.commit_idempotent(id(2), vec![put(b"tail")]).unwrap();
    let next_dir = f.0.join("next");
    fs::create_dir(&next_dir).unwrap();
    let mut next = ManifestStore::create(&next_dir, 7, 3).unwrap();
    db.prepare_sst_successor(&old, &mut next, 1).unwrap();
    db.reclaim_to_sst_successor(&old, &next).unwrap();
    let backup = f.0.join("backup");
    db.backup_sealed(&next, &backup).unwrap();
    db.commit_idempotent(id(3), vec![put(b"latest")]).unwrap();
    let quarantine = f.0.join("quarantine");
    let old_dir = f.0.join("sst");
    let live = f.0.join("tablet.wal");
    let before = fs::read(&live).unwrap();
    assert!(db
        .quarantine_obsolete_sst(&next, old, &backup, &quarantine)
        .is_err());
    assert!(old_dir.exists());
    assert!(!quarantine.exists());
    assert_eq!(fs::read(&live).unwrap(), before);
    let backup = f.0.join("fresh-backup");
    db.backup_sealed(&next, &backup).unwrap();
    let old = ManifestStore::open(&old_dir, 7, 3).unwrap();
    db.quarantine_obsolete_sst(&next, old, &backup, &quarantine)
        .unwrap();
    assert!(!old_dir.exists());
    assert!(quarantine.join("MANIFEST").exists());
    assert_eq!(fs::read(&live).unwrap(), before);
    assert_eq!(
        db.commit_idempotent(id(1), vec![put(b"first")]).unwrap(),
        receipt
    );
    assert_eq!(db.read_latest(b"k"), Some(b"latest".as_slice()));
    drop(db);
    drop(next);
    let db = WalBackedTablet::open_from_sealed_sst(f.descriptor(), &live, &next_dir).unwrap();
    assert_eq!(db.current_sequence(), 3);
    assert_eq!(
        ManifestStore::open(&quarantine, 7, 3)
            .unwrap()
            .recovered_read_at(b"k", 1)
            .unwrap(),
        Some(b"first".to_vec())
    );
    assert_eq!(
        WalBackedTablet::restore_sealed_backup(f.descriptor(), &backup, f.0.join("restore"))
            .unwrap()
            .current_sequence(),
        3
    );
}

#[test]
fn quarantine_refuses_corrupt_backup_and_unrelated_files_or_existing_target() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let old = seal(&f, &db);
    db.reclaim_to_sealed_sst(&old).unwrap();
    db.commit_idempotent(id(2), vec![put(b"tail")]).unwrap();
    let next_dir = f.0.join("next");
    fs::create_dir(&next_dir).unwrap();
    let mut next = ManifestStore::create(&next_dir, 7, 3).unwrap();
    db.prepare_sst_successor(&old, &mut next, 0).unwrap();
    db.reclaim_to_sst_successor(&old, &next).unwrap();
    let backup = f.0.join("backup");
    db.backup_sealed(&next, &backup).unwrap();
    let old_dir = f.0.join("sst");
    let quarantine = f.0.join("quarantine");
    let file = backup.join("tablet.wal");
    let valid = fs::read(&file).unwrap();
    let mut bad = valid.clone();
    bad[32] ^= 1;
    fs::write(&file, bad).unwrap();
    assert!(db
        .quarantine_obsolete_sst(&next, old, &backup, &quarantine)
        .is_err());
    fs::write(&file, valid).unwrap();
    let unknown = old_dir.join("other-user-data");
    fs::write(&unknown, b"keep").unwrap();
    assert!(db
        .quarantine_obsolete_sst(
            &next,
            ManifestStore::open(&old_dir, 7, 3).unwrap(),
            &backup,
            &quarantine
        )
        .is_err());
    assert_eq!(fs::read(&unknown).unwrap(), b"keep");
    fs::remove_file(&unknown).unwrap();
    fs::create_dir(&quarantine).unwrap();
    assert!(db
        .quarantine_obsolete_sst(
            &next,
            ManifestStore::open(&old_dir, 7, 3).unwrap(),
            &backup,
            &quarantine
        )
        .is_err());
    assert!(old_dir.exists());
    assert!(quarantine.read_dir().unwrap().next().is_none());
}

#[test]
fn durable_registry_keeps_old_and_new_references_until_exact_wal_reconciliation() {
    use nulang::database::generation_registry::{
        GenerationRegistry, GenerationStatus, ReferenceKind,
    };
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let old = seal(&f, &db);
    db.reclaim_to_sealed_sst(&old).unwrap();
    let registry_dir = f.0.join("registry");
    fs::create_dir(&registry_dir).unwrap();
    let mut registry = GenerationRegistry::create(&registry_dir, 7, 3).unwrap();
    assert_eq!(registry.status(&old).unwrap(), GenerationStatus::Unknown);
    db.register_generation_wal(&mut registry, &old).unwrap();
    registry
        .register_reference(ReferenceKind::Reader, [1; 16], &old)
        .unwrap();
    assert_eq!(registry.status(&old).unwrap(), GenerationStatus::Referenced);
    db.commit_idempotent(id(2), vec![put(b"tail")]).unwrap();
    let next_dir = f.0.join("next");
    fs::create_dir(&next_dir).unwrap();
    let mut next = ManifestStore::create(&next_dir, 7, 3).unwrap();
    db.prepare_sst_successor(&old, &mut next, 1).unwrap();
    db.reserve_generation_rotation(&mut registry, &old, &next)
        .unwrap();
    assert_eq!(registry.status(&old).unwrap(), GenerationStatus::Referenced);
    assert_eq!(
        registry.status(&next).unwrap(),
        GenerationStatus::Referenced
    );
    drop(registry);
    let mut registry = GenerationRegistry::open(&registry_dir, 7, 3).unwrap();
    assert_eq!(
        registry.status(&next).unwrap(),
        GenerationStatus::Referenced
    );
    db.reclaim_to_sst_successor(&old, &next).unwrap();
    db.reconcile_generation_wal(&mut registry, &next).unwrap();
    assert_eq!(registry.status(&old).unwrap(), GenerationStatus::Referenced);
    registry
        .release_reference(ReferenceKind::Reader, [1; 16], &old)
        .unwrap();
    assert_eq!(
        registry.status(&old).unwrap(),
        GenerationStatus::Unreferenced
    );
    assert_eq!(
        registry.status(&next).unwrap(),
        GenerationStatus::Referenced
    );
    drop(registry);
    let registry = GenerationRegistry::open(&registry_dir, 7, 3).unwrap();
    assert_eq!(
        registry.status(&old).unwrap(),
        GenerationStatus::Unreferenced
    );
    assert_eq!(
        registry.status(&next).unwrap(),
        GenerationStatus::Referenced
    );
}

#[test]
fn registry_rejects_wrong_identity_duplicate_reference_and_corruption() {
    use nulang::database::generation_registry::{
        GenerationRegistry, GenerationStatus, ReferenceKind,
    };
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let old = seal(&f, &db);
    db.reclaim_to_sealed_sst(&old).unwrap();
    let dir = f.0.join("registry");
    fs::create_dir(&dir).unwrap();
    let mut r = GenerationRegistry::create(&dir, 7, 3).unwrap();
    assert!(GenerationRegistry::open(&dir, 7, 3).is_err());
    assert!(GenerationRegistry::create(&dir, 7, 3).is_err());
    r.register_reference(ReferenceKind::Backup, [1; 16], &old)
        .unwrap();
    assert!(r
        .release_reference(ReferenceKind::Reader, [1; 16], &old)
        .is_err());
    assert_eq!(r.status(&old).unwrap(), GenerationStatus::Referenced);
    drop(r);
    assert!(GenerationRegistry::open(&dir, 7, 4).is_err());
    let p = dir.join("GENERATIONS");
    let mut bytes = fs::read(&p).unwrap();
    bytes[10] ^= 1;
    fs::write(&p, bytes).unwrap();
    assert!(GenerationRegistry::open(&dir, 7, 3).is_err());
    assert!(GenerationRegistry::create(&dir, 7, 3).is_err());
}

#[test]
fn managed_rotation_automatically_reconciles_and_quarantine_respects_references() {
    use nulang::database::generation_registry::{
        GenerationRegistry, GenerationStatus, ReferenceKind,
    };
    use nulang::database::managed_storage::ManagedTablet;
    let f = Fixture::new();
    let mut db = f.open();
    let receipt = db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let old = seal(&f, &db);
    db.reclaim_to_sealed_sst(&old).unwrap();
    let reg_dir = f.0.join("registry");
    fs::create_dir(&reg_dir).unwrap();
    let registry = GenerationRegistry::create(&reg_dir, 7, 3).unwrap();
    let mut managed = ManagedTablet::adopt(db, registry, old).unwrap();
    managed
        .register_reference(ReferenceKind::Reader, [1; 16])
        .unwrap();
    managed
        .commit_idempotent(id(2), vec![put(b"tail")])
        .unwrap();
    let next_dir = f.0.join("next");
    fs::create_dir(&next_dir).unwrap();
    let next = ManifestStore::create(&next_dir, 7, 3).unwrap();
    managed.rotate(next, 1).unwrap();
    let old_dir = f.0.join("sst");
    let old = ManifestStore::open(&old_dir, 7, 3).unwrap();
    assert_eq!(
        managed.generation_status(&old).unwrap(),
        GenerationStatus::Referenced
    );
    let backup = f.0.join("backup");
    managed.backup([2; 16], &backup).unwrap();
    let quarantine = f.0.join("quarantine");
    assert!(managed.quarantine(old, &backup, &quarantine).is_err());
    let old = ManifestStore::open(&old_dir, 7, 3).unwrap();
    managed
        .release_reference(ReferenceKind::Reader, [1; 16], &old)
        .unwrap();
    assert_eq!(
        managed.generation_status(&old).unwrap(),
        GenerationStatus::Unreferenced
    );
    managed.quarantine(old, &backup, &quarantine).unwrap();
    assert!(!old_dir.exists());
    assert_eq!(
        managed
            .commit_idempotent(id(1), vec![put(b"first")])
            .unwrap(),
        receipt
    );
    drop(managed);
    let registry = GenerationRegistry::open(&reg_dir, 7, 3).unwrap();
    let managed =
        ManagedTablet::open(f.descriptor(), f.0.join("tablet.wal"), &next_dir, registry).unwrap();
    assert_eq!(managed.current_sequence(), 2);
    assert_eq!(managed.read_at(b"k", 2).unwrap(), Some(b"tail".as_slice()));
}

#[test]
fn managed_open_requires_registered_wal_and_backup_reserves_reference() {
    use nulang::database::generation_registry::{
        GenerationRegistry, GenerationStatus, ReferenceKind,
    };
    use nulang::database::managed_storage::ManagedTablet;
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let old = seal(&f, &db);
    db.reclaim_to_sealed_sst(&old).unwrap();
    drop(db);
    drop(old);
    let reg_dir = f.0.join("registry");
    fs::create_dir(&reg_dir).unwrap();
    let registry = GenerationRegistry::create(&reg_dir, 7, 3).unwrap();
    assert!(ManagedTablet::open(
        f.descriptor(),
        f.0.join("tablet.wal"),
        f.0.join("sst"),
        registry
    )
    .is_err());
    let db = recover(&f).unwrap();
    let old = ManifestStore::open(f.0.join("sst"), 7, 3).unwrap();
    let registry = GenerationRegistry::open(&reg_dir, 7, 3).unwrap();
    let mut managed = ManagedTablet::adopt(db, registry, old).unwrap();
    let existing = f.0.join("existing");
    fs::create_dir(&existing).unwrap();
    assert!(managed.backup([3; 16], &existing).is_err());
    managed
        .commit_idempotent(id(2), vec![put(b"tail")])
        .unwrap();
    let next_dir = f.0.join("next");
    fs::create_dir(&next_dir).unwrap();
    managed
        .rotate(ManifestStore::create(&next_dir, 7, 3).unwrap(), 0)
        .unwrap();
    let old = ManifestStore::open(f.0.join("sst"), 7, 3).unwrap();
    assert_eq!(
        managed.generation_status(&old).unwrap(),
        GenerationStatus::Referenced
    );
    managed
        .release_reference(ReferenceKind::Backup, [3; 16], &old)
        .unwrap();
    assert_eq!(
        managed.generation_status(&old).unwrap(),
        GenerationStatus::Unreferenced
    );
}

#[test]
fn unregistered_managed_open_leaves_torn_wal_unchanged() {
    use nulang::database::{
        generation_registry::GenerationRegistry, managed_storage::ManagedTablet,
    };
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let m = seal(&f, &db);
    db.reclaim_to_sealed_sst(&m).unwrap();
    drop(db);
    drop(m);
    let wal = f.0.join("tablet.wal");
    let mut torn = fs::read(&wal).unwrap();
    torn.extend_from_slice(b"partial");
    fs::write(&wal, &torn).unwrap();
    let reg_dir = f.0.join("registry");
    fs::create_dir(&reg_dir).unwrap();
    let registry = GenerationRegistry::create(&reg_dir, 7, 3).unwrap();
    assert!(ManagedTablet::open(f.descriptor(), &wal, f.0.join("sst"), registry).is_err());
    assert_eq!(fs::read(&wal).unwrap(), torn);
}

#[test]
fn sealed_recovery_rejects_noncanonical_wal_name_before_lock_creation() {
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"original")]).unwrap();
    let manifest = seal(&f, &db);
    db.reclaim_to_sealed_sst(&manifest).unwrap();
    drop(manifest);
    drop(db);
    let path = f.0.join("tablet.wal");
    let alternate = f.0.join("tablet.other");
    let original = fs::read(&path).unwrap();
    fs::copy(&path, &alternate).unwrap();
    assert!(
        WalBackedTablet::open_from_sealed_sst(f.descriptor(), &alternate, f.0.join("sst")).is_err()
    );
    assert!(!f.0.join("tablet.other.lock").exists());
    assert_eq!(fs::read(&alternate).unwrap(), original);
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(
        recover(&f).unwrap().read_latest(b"k"),
        Some(b"original".as_slice())
    );
}

#[test]
fn durable_enrollment_rejects_raw_recovery_and_alternate_registry() {
    use nulang::database::{
        generation_registry::GenerationRegistry, managed_storage::ManagedTablet,
    };
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let old = seal(&f, &db);
    db.reclaim_to_sealed_sst(&old).unwrap();
    let dir = f.0.join("registry");
    let other = f.0.join("alternate");
    fs::create_dir(&dir).unwrap();
    fs::create_dir(&other).unwrap();
    let mut alt = GenerationRegistry::create(&other, 7, 3).unwrap();
    db.register_generation_wal(&mut alt, &old).unwrap();
    let registry = GenerationRegistry::create(&dir, 7, 3).unwrap();
    let mut managed = ManagedTablet::adopt(db, registry, old).unwrap();
    managed
        .commit_idempotent(id(2), vec![put(b"tail")])
        .unwrap();
    drop(managed);
    let wal = f.0.join("tablet.wal");
    let mut torn = fs::read(&wal).unwrap();
    torn.extend_from_slice(b"partial");
    fs::write(&wal, &torn).unwrap();
    assert!(recover(&f).is_err());
    assert_eq!(fs::read(&wal).unwrap(), torn);
    assert!(ManagedTablet::open(f.descriptor(), &wal, f.0.join("sst"), alt).is_err());
    assert_eq!(fs::read(&wal).unwrap(), torn);
    let registry = GenerationRegistry::open(&dir, 7, 3).unwrap();
    let managed = ManagedTablet::open(f.descriptor(), &wal, f.0.join("sst"), registry).unwrap();
    assert_eq!(managed.current_sequence(), 2);
    assert_eq!(managed.read_at(b"k", 2).unwrap(), Some(b"tail".as_slice()));
    drop(managed);
    assert!(recover(&f).is_err());
}

#[test]
fn malformed_enrollment_marker_blocks_raw_and_managed_recovery_before_wal_repair() {
    use nulang::database::{
        generation_registry::GenerationRegistry, managed_storage::ManagedTablet,
    };
    let f = Fixture::new();
    let mut db = f.open();
    db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let m = seal(&f, &db);
    db.reclaim_to_sealed_sst(&m).unwrap();
    let reg = f.0.join("registry");
    fs::create_dir(&reg).unwrap();
    let registry = GenerationRegistry::create(&reg, 7, 3).unwrap();
    drop(ManagedTablet::adopt(db, registry, m).unwrap());
    let wal = f.0.join("tablet.wal");
    let lock = f.0.join("tablet.wal.lock");
    let original = fs::read(&lock).unwrap();
    assert_eq!(original.len(), 72);
    let mut torn = fs::read(&wal).unwrap();
    torn.extend_from_slice(b"partial");
    fs::write(&wal, &torn).unwrap();
    let mut corrupt = original.clone();
    corrupt[20] ^= 1;
    for marker in [original[..13].to_vec(), corrupt] {
        fs::write(&lock, &marker).unwrap();
        assert!(recover(&f).is_err());
        let registry = GenerationRegistry::open(&reg, 7, 3).unwrap();
        assert!(ManagedTablet::open(f.descriptor(), &wal, f.0.join("sst"), registry).is_err());
        assert_eq!(fs::read(&wal).unwrap(), torn);
        assert_eq!(fs::read(&lock).unwrap(), marker);
    }
    fs::write(&lock, original).unwrap();
    let registry = GenerationRegistry::open(&reg, 7, 3).unwrap();
    let managed = ManagedTablet::open(f.descriptor(), &wal, f.0.join("sst"), registry).unwrap();
    assert_eq!(managed.current_sequence(), 1);
}

#[test]
fn wal5_self_identifies_even_if_enrollment_sidecar_is_empty_and_backups_are_portable() {
    use nulang::database::{
        generation_registry::GenerationRegistry, managed_storage::ManagedTablet,
    };
    let f = Fixture::new();
    let mut db = f.open();
    let receipt = db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    let old = seal(&f, &db);
    db.reclaim_to_sealed_sst(&old).unwrap();
    db.commit_idempotent(id(2), vec![put(b"tail")]).unwrap();
    let wal = f.0.join("tablet.wal");
    let lock = f.0.join("tablet.wal.lock");
    let before = fs::read(&wal).unwrap();
    let reg = f.0.join("registry");
    fs::create_dir(&reg).unwrap();
    let registry = GenerationRegistry::create(&reg, 7, 3).unwrap();
    let mut managed = ManagedTablet::adopt(db, registry, old).unwrap();
    let managed_bytes = fs::read(&wal).unwrap();
    assert_eq!(&managed_bytes[..8], b"NUDBWAL5");
    assert_eq!(&managed_bytes[128..], &before[96..]);
    let backup = f.0.join("backup");
    managed.backup([1; 16], &backup).unwrap();
    assert_eq!(fs::read(backup.join("tablet.wal")).unwrap(), before);
    let restored =
        WalBackedTablet::restore_sealed_backup(f.descriptor(), &backup, f.0.join("restored"))
            .unwrap();
    assert_eq!(restored.current_sequence(), 2);
    drop(restored);
    drop(managed);
    let marker = fs::read(&lock).unwrap();
    fs::write(&lock, []).unwrap();
    assert!(recover(&f).is_err());
    assert_eq!(fs::read(&wal).unwrap(), managed_bytes);
    let registry = GenerationRegistry::open(&reg, 7, 3).unwrap();
    assert!(ManagedTablet::open(f.descriptor(), &wal, f.0.join("sst"), registry).is_err());
    fs::write(&lock, marker).unwrap();
    let registry = GenerationRegistry::open(&reg, 7, 3).unwrap();
    let mut managed = ManagedTablet::open(f.descriptor(), &wal, f.0.join("sst"), registry).unwrap();
    assert_eq!(
        managed
            .commit_idempotent(id(1), vec![put(b"first")])
            .unwrap(),
        receipt
    );
    let next = f.0.join("next");
    fs::create_dir(&next).unwrap();
    managed
        .rotate(ManifestStore::create(&next, 7, 3).unwrap(), 1)
        .unwrap();
    let bytes = fs::read(&wal).unwrap();
    assert_eq!(bytes.len(), 128);
    assert_eq!(&bytes[..8], b"NUDBWAL5");
    drop(managed);
    fs::remove_file(&lock).unwrap();
    assert!(WalBackedTablet::open_from_sealed_sst(f.descriptor(), &wal, &next).is_err());
    assert_eq!(fs::read(&wal).unwrap(), bytes);
}

#[test]
fn wal5_migration_refuses_valid_but_changed_tail_before_replacing_live_wal() {
    use nulang::database::{
        generation_registry::GenerationRegistry, managed_storage::ManagedTablet,
    };
    let f = Fixture::new();
    let other = Fixture::new();
    let mut db = f.open();
    let mut other_db = other.open();
    for db in [&mut db, &mut other_db] {
        db.commit_idempotent(id(1), vec![put(b"first")]).unwrap();
    }
    let m = seal(&f, &db);
    let om = seal(&other, &other_db);
    db.reclaim_to_sealed_sst(&m).unwrap();
    other_db.reclaim_to_sealed_sst(&om).unwrap();
    let write = db.prepare_write(3, 1, vec![put(b"tail")]).unwrap();
    db.commit(write).unwrap();
    let write = other_db.prepare_write(3, 1, vec![put(b"tall")]).unwrap();
    other_db.commit(write).unwrap();
    let wal = f.0.join("tablet.wal");
    let other_bytes = fs::read(other.0.join("tablet.wal")).unwrap();
    // A valid alternate frame with the same sequence and length must not
    // silently replace the acknowledged logical state during migration.
    assert_eq!(fs::metadata(&wal).unwrap().len(), other_bytes.len() as u64);
    fs::write(&wal, &other_bytes).unwrap();
    let reg = f.0.join("registry");
    fs::create_dir(&reg).unwrap();
    let registry = GenerationRegistry::create(&reg, 7, 3).unwrap();
    assert!(ManagedTablet::adopt(db, registry, m).is_err());
    assert_eq!(fs::read(&wal).unwrap(), other_bytes);
}
