use nulang::database::indexed_sst::{DiskLookup, IndexedSst};
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nudb-indexed-sst-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("table.sst")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

type Row<'a> = (&'a [u8], Vec<(u64, Option<&'a [u8]>)>);
fn encode(rows: &[Row<'_>]) -> Vec<u8> {
    let mut bytes = b"NUDBSST1".to_vec();
    bytes.extend_from_slice(&1u16.to_le_bytes());
    for n in [
        1u64,
        1,
        10,
        rows.len() as u64,
        rows.iter().map(|r| r.1.len() as u64).sum(),
    ] {
        bytes.extend_from_slice(&n.to_le_bytes());
    }
    for (key, versions) in rows {
        bytes.extend_from_slice(&(key.len() as u32).to_le_bytes());
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&(versions.len() as u32).to_le_bytes());
        for (seq, value) in versions {
            bytes.extend_from_slice(&seq.to_le_bytes());
            match value {
                None => bytes.push(0),
                Some(value) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
                    bytes.extend_from_slice(value);
                }
            }
        }
    }
    let checksum = blake3::hash(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    bytes
}
fn rehash(bytes: &mut Vec<u8>) {
    bytes.truncate(bytes.len() - 32);
    let digest = blake3::hash(bytes);
    bytes.extend_from_slice(digest.as_bytes());
}
#[test]
fn sparse_snapshots_distinguish_tombstone_missing_and_empty_value() {
    let f = Fixture::new();
    fs::write(
        f.path(),
        encode(&[
            (b"a", vec![(2, Some(b"first")), (4, None), (7, Some(b""))]),
            (b"b", vec![(9, Some(b"second"))]),
        ]),
    )
    .unwrap();
    let mut table = IndexedSst::open(f.path()).unwrap();
    assert_eq!(table.sequence_interval(), (1, 10));
    assert_eq!(table.generation(), 1);
    assert_eq!(table.read_at(b"a", 1).unwrap(), DiskLookup::Missing);
    assert_eq!(
        table.read_at(b"a", 3).unwrap(),
        DiskLookup::Value {
            sequence: 2,
            value: b"first".to_vec()
        }
    );
    assert_eq!(
        table.read_at(b"a", 6).unwrap(),
        DiskLookup::Deleted { sequence: 4 }
    );
    assert_eq!(
        table.read_at(b"a", u64::MAX).unwrap(),
        DiskLookup::Value {
            sequence: 7,
            value: vec![]
        }
    );
    assert_eq!(table.read_at(b"missing", 10).unwrap(), DiskLookup::Missing);
}
#[test]
fn metadata_only_table_is_valid() {
    let f = Fixture::new();
    fs::write(f.path(), encode(&[])).unwrap();
    let mut table = IndexedSst::open(f.path()).unwrap();
    assert_eq!(table.sequence_interval(), (1, 10));
    assert_eq!(table.read_at(b"a", 10).unwrap(), DiskLookup::Missing);
}
#[test]
fn large_values_are_not_retained_in_index_and_are_verified_on_read() {
    let f = Fixture::new();
    let value = vec![42; 4 * 1024 * 1024];
    fs::write(f.path(), encode(&[(b"a", vec![(2, Some(&value))])])).unwrap();
    let mut table = IndexedSst::open(f.path()).unwrap();
    assert_eq!(table.indexed_versions(), 1);
    assert_eq!(table.indexed_key_bytes(), 1);
    assert_eq!(
        table.read_at(b"a", 2).unwrap(),
        DiskLookup::Value { sequence: 2, value }
    );
    let mut file = fs::OpenOptions::new().write(true).open(f.path()).unwrap();
    // 50-byte fixed header + 4-byte key length + key + 4-byte count +
    // 8-byte sequence + 1-byte tag + 4-byte length.
    file.seek(SeekFrom::Start(72)).unwrap();
    file.write_all(&[0]).unwrap();
    assert_eq!(
        table.read_at(b"a", 2).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}
#[test]
fn invalid_tables_fail_closed_even_with_recomputed_checksum() {
    let f = Fixture::new();
    for rows in [
        vec![
            (b"b".as_slice(), vec![(2, Some(b"x".as_slice()))]),
            (b"a".as_slice(), vec![(3, None)]),
        ],
        vec![(b"a".as_slice(), vec![(2, None), (2, Some(b"x".as_slice()))])],
        vec![(b"a".as_slice(), vec![(11, None)])],
        vec![(b"a".as_slice(), vec![])],
    ] {
        fs::write(f.path(), encode(&rows)).unwrap();
        assert!(IndexedSst::open(f.path()).is_err());
    }
    let mut bytes = encode(&[(b"a", vec![(2, None)])]);
    bytes[34..42].copy_from_slice(&u64::MAX.to_le_bytes());
    rehash(&mut bytes);
    fs::write(f.path(), bytes).unwrap();
    assert!(IndexedSst::open(f.path()).is_err());
    let mut bytes = encode(&[(b"a", vec![(2, Some(b"x"))])]);
    bytes[72] ^= 1;
    fs::write(f.path(), bytes).unwrap();
    assert!(IndexedSst::open(f.path()).is_err());
}
