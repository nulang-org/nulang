use super::sstable::write_sstable;
use super::sstable_indexed::{IndexedSstable, IndexedVersion};
use super::tablet::{TabletSnapshotRow, VersionedValue};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_mmap_sstable_{name}_{}_{}.sst",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(unix)]
#[test]
fn test_indexed_sstable_mmap_survives_unlink_and_preserves_borrowed_reads() {
    let path = temp_path("backing");
    let _ = fs::remove_file(&path);
    let rows = vec![TabletSnapshotRow {
        key: b"alpha".to_vec(),
        versions: vec![VersionedValue {
            sequence: 1,
            value: Some(vec![b'x'; 64 * 1024]),
        }],
    }];
    write_sstable(&path, 42, 7, &rows).unwrap();

    let indexed = IndexedSstable::open(&path).unwrap();
    assert!(indexed.is_memory_mapped_for_test());
    fs::remove_file(&path).unwrap();
    assert_eq!(
        indexed.version_at(b"alpha", 1),
        Some(IndexedVersion {
            sequence: 1,
            value: Some(rows[0].versions[0].value.as_deref().unwrap()),
        })
    );
}
