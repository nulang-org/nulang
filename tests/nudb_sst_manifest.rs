#![cfg(unix)]
use nulang::database::indexed_sst::{write_immutable_table, SstHeader};
use nulang::database::sst_manifest::{Manifest, ManifestStore, TableEntry};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "nudb-manifest-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn table(dir: &Path, generation: u64, min: u64, max: u64, value: &[u8]) -> TableEntry {
    let path = dir.join(format!("nudb-sst-{generation:020}.sst"));
    let versions = [(min, Some(value))];
    write_immutable_table(
        &path,
        SstHeader {
            generation,
            min_sequence: min,
            max_sequence: max,
            key_count: 1,
            version_count: 1,
        },
        [(b"k".as_slice(), versions.as_slice())],
    )
    .unwrap();
    TableEntry::inspect(dir, generation).unwrap()
}
#[test]
fn manifest_append_reopens_and_ignores_unpublished_tables() {
    let f = Fixture::new();
    let mut store = ManifestStore::create(&f.0, 7, 3).unwrap();
    assert_eq!(store.current().durable_sequence(), 0);
    let a = table(&f.0, 1, 1, 3, b"first");
    store.append(a.clone()).unwrap();
    let b = table(&f.0, 2, 4, 6, b"second");
    drop(store);
    let mut store = ManifestStore::open(&f.0, 7, 3).unwrap();
    assert_eq!(store.current().entries(), &[a.clone()]);
    assert_eq!(store.current().durable_sequence(), 3);
    store.append(b.clone()).unwrap();
    drop(store);
    let store = ManifestStore::open(&f.0, 7, 3).unwrap();
    assert_eq!(store.current().entries(), &[a, b]);
    assert_eq!(store.current().durable_sequence(), 6);
}
#[test]
fn invalid_append_preserves_manifest_and_fails_closed() {
    let f = Fixture::new();
    let mut store = ManifestStore::create(&f.0, 7, 3).unwrap();
    let a = table(&f.0, 1, 1, 2, b"a");
    store.append(a.clone()).unwrap();
    let original = fs::read(f.0.join("MANIFEST")).unwrap();
    let gap = table(&f.0, 2, 4, 5, b"gap");
    assert!(store.append(gap).is_err());
    assert!(store.append(a).is_err());
    assert_eq!(store.current().durable_sequence(), 2);
    assert_eq!(fs::read(f.0.join("MANIFEST")).unwrap(), original);
    drop(store);
    assert!(ManifestStore::open(&f.0, 8, 3).is_err());
    assert!(ManifestStore::open(&f.0, 7, 4).is_err());
}
#[test]
fn missing_or_replaced_referenced_table_rejects_recovery() {
    let f = Fixture::new();
    let mut store = ManifestStore::create(&f.0, 7, 3).unwrap();
    let a = table(&f.0, 1, 1, 2, b"original");
    store.append(a).unwrap();
    drop(store);
    let path = f.0.join(format!("nudb-sst-{:020}.sst", 1));
    fs::remove_file(&path).unwrap();
    assert!(ManifestStore::open(&f.0, 7, 3).is_err());
    table(&f.0, 1, 1, 2, b"replaced");
    assert!(ManifestStore::open(&f.0, 7, 3).is_err());
}
#[test]
fn exclusive_writer_and_explicit_creation() {
    let f = Fixture::new();
    assert!(ManifestStore::open(&f.0, 7, 3).is_err());
    let store = ManifestStore::create(&f.0, 7, 3).unwrap();
    assert!(ManifestStore::open(&f.0, 7, 3).is_err());
    assert!(ManifestStore::create(&f.0, 7, 3).is_err());
    drop(store);
    assert!(ManifestStore::create(&f.0, 7, 3).is_err());
    assert!(ManifestStore::open(&f.0, 7, 3).is_ok());
}
#[test]
fn corrupt_manifest_is_not_treated_as_empty() {
    let f = Fixture::new();
    drop(ManifestStore::create(&f.0, 7, 3).unwrap());
    let p = f.0.join("MANIFEST");
    let mut bytes = fs::read(&p).unwrap();
    bytes[12] ^= 1;
    fs::write(&p, bytes).unwrap();
    assert!(ManifestStore::open(&f.0, 7, 3).is_err());
    assert!(ManifestStore::create(&f.0, 7, 3).is_err());
}
#[test]
fn abandoned_staging_file_is_not_authoritative() {
    let f = Fixture::new();
    drop(ManifestStore::create(&f.0, 7, 3).unwrap());
    fs::write(f.0.join(".MANIFEST-abandoned.tmp"), b"partial").unwrap();
    let store = ManifestStore::open(&f.0, 7, 3).unwrap();
    assert_eq!(store.current(), &Manifest::empty(7, 3).unwrap());
}

#[test]
fn missing_manifest_cannot_reinitialize_existing_table_directory() {
    let f = Fixture::new();
    table(&f.0, 1, 1, 2, b"existing");
    assert!(ManifestStore::create(&f.0, 7, 3).is_err());
    assert!(!f.0.join("MANIFEST").exists());
}

#[test]
fn manifest_rejects_table_symlink_alias() {
    let f = Fixture::new();
    let mut store = ManifestStore::create(&f.0, 7, 3).unwrap();
    let a = table(&f.0, 1, 1, 2, b"original");
    let path = f.0.join(format!("nudb-sst-{:020}.sst", 1));
    let other = f.0.join("other.sst");
    fs::rename(&path, &other).unwrap();
    std::os::unix::fs::symlink(&other, &path).unwrap();
    assert!(store.append(a).is_err());
    assert_eq!(store.current().durable_sequence(), 0);
}
