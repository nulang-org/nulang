//! Compact read-side index over the existing SSTable v1 encoded payload.
//!
//! This module is intentionally format-compatible with `sstable`: the writer
//! remains unchanged while the serving path stops requiring decoded row/value
//! allocations to stay resident.

use std::path::Path;

use super::sstable::{SstableError, SstableMetadata};

#[derive(Debug)]
pub(crate) struct IndexedSstable;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexedVersion<'a> {
    pub(crate) sequence: u64,
    pub(crate) value: Option<&'a [u8]>,
}

impl IndexedSstable {
    pub(crate) fn open(_path: &Path) -> Result<Self, SstableError> {
        todo!("indexed SSTable reader implementation")
    }

    pub(crate) fn metadata(&self) -> &SstableMetadata {
        todo!("indexed SSTable metadata")
    }

    pub(crate) fn version_at(&self, _key: &[u8], _snapshot: u64) -> Option<IndexedVersion<'_>> {
        todo!("indexed SSTable MVCC lookup")
    }

    pub(crate) fn has_contiguous_sequence_coverage_after(&self, _floor: u64) -> bool {
        todo!("indexed SSTable sequence coverage")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::sstable::{write_sstable, Sstable};
    use crate::database::tablet::{TabletSnapshotRow, VersionedValue};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_nudb_indexed_sstable_{name}_{}_{}.sst",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn rows() -> Vec<TabletSnapshotRow> {
        vec![
            TabletSnapshotRow {
                key: b"alpha".to_vec(),
                versions: vec![
                    VersionedValue {
                        sequence: 1,
                        value: Some(b"one".to_vec()),
                    },
                    VersionedValue {
                        sequence: 3,
                        value: Some(b"three".to_vec()),
                    },
                ],
            },
            TabletSnapshotRow {
                key: b"beta".to_vec(),
                versions: vec![
                    VersionedValue {
                        sequence: 2,
                        value: Some(b"two".to_vec()),
                    },
                    VersionedValue {
                        sequence: 4,
                        value: None,
                    },
                ],
            },
        ]
    }

    #[test]
    fn indexed_reader_matches_v1_metadata_and_preserves_mvcc_version_identity() {
        let path = temp_path("roundtrip");
        let _ = fs::remove_file(&path);
        write_sstable(&path, 42, 7, &rows()).unwrap();

        let decoded = Sstable::open(&path).unwrap();
        let indexed = IndexedSstable::open(&path).unwrap();
        assert_eq!(indexed.metadata(), decoded.metadata());

        assert_eq!(
            indexed.version_at(b"alpha", 1),
            Some(IndexedVersion {
                sequence: 1,
                value: Some(&b"one"[..]),
            })
        );
        assert_eq!(
            indexed.version_at(b"alpha", 3),
            Some(IndexedVersion {
                sequence: 3,
                value: Some(&b"three"[..]),
            })
        );
        assert_eq!(
            indexed.version_at(b"beta", 4),
            Some(IndexedVersion {
                sequence: 4,
                value: None,
            })
        );
        assert_eq!(indexed.version_at(b"missing", 4), None);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn indexed_reader_preserves_sequence_coverage_contract() {
        let path = temp_path("coverage");
        let _ = fs::remove_file(&path);
        write_sstable(&path, 42, 7, &rows()).unwrap();
        let indexed = IndexedSstable::open(&path).unwrap();

        assert!(indexed.has_contiguous_sequence_coverage_after(0));
        assert!(indexed.has_contiguous_sequence_coverage_after(2));
        assert!(indexed.has_contiguous_sequence_coverage_after(4));

        let _ = fs::remove_file(path);
    }
}
