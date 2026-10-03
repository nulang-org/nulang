use super::sstable::SstableError;
use super::sstable_indexed::IndexedSstable;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_indexed_sstable_hardening_{name}_{}_{}.sst",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn test_declared_payload_length_is_rejected_before_read_allocation_when_file_is_truncated() {
    let path = temp_path("declared-length");
    let _ = fs::remove_file(&path);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"NUDBSST1");
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&4096_u32.to_le_bytes());
    fs::write(&path, bytes).unwrap();

    assert_eq!(
        IndexedSstable::open(&path).unwrap_err(),
        SstableError::InvalidLength
    );

    let _ = fs::remove_file(path);
}
