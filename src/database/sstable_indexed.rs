//! Compact read-side index over the existing SSTable v1 encoded payload.
//!
//! This module is intentionally format-compatible with `sstable`: the writer
//! remains unchanged while the serving path stops requiring decoded row/value
//! allocations to stay resident.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use super::sstable::{SstableError, SstableMetadata};

// These constants intentionally pin the existing SSTable v1 reader contract.
// Metadata-parity tests below protect this read-side parser from drifting away
// from the canonical writer/decoder in `sstable`.
const SSTABLE_MAGIC: &[u8; 8] = b"NUDBSST1";
const SSTABLE_VERSION: u16 = 1;
const MAX_SSTABLE_BYTES: usize = 256 * 1024 * 1024;
const MAX_ROWS: usize = 4_000_000;
const MAX_VERSIONS_PER_ROW: usize = 65_536;
const MAX_KEY_BYTES: usize = 16 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
struct RowIndex {
    key_start: usize,
    key_end: usize,
    versions_start: usize,
    version_count: usize,
}

#[derive(Debug)]
pub(crate) struct IndexedSstable {
    metadata: SstableMetadata,
    payload: Vec<u8>,
    rows: Vec<RowIndex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IndexedVersion<'a> {
    pub(crate) sequence: u64,
    pub(crate) value: Option<&'a [u8]>,
}

impl IndexedSstable {
    pub(crate) fn open(path: &Path) -> Result<Self, SstableError> {
        let mut file = File::open(path)?;
        let mut magic = [0_u8; 8];
        file.read_exact(&mut magic)?;
        if &magic != SSTABLE_MAGIC {
            return Err(SstableError::InvalidHeader);
        }

        let version = read_u16(&mut file)?;
        if version != SSTABLE_VERSION {
            return Err(SstableError::UnsupportedVersion(version));
        }
        let payload_len = read_u32(&mut file)? as usize;
        if payload_len > MAX_SSTABLE_BYTES {
            return Err(SstableError::TooLarge(payload_len));
        }
        let mut payload = vec![0_u8; payload_len];
        file.read_exact(&mut payload)?;
        let mut checksum = [0_u8; 32];
        file.read_exact(&mut checksum)?;
        if checksum != *blake3::hash(&payload).as_bytes() {
            return Err(SstableError::ChecksumMismatch);
        }
        let mut trailing = [0_u8; 1];
        if file.read(&mut trailing)? != 0 {
            return Err(SstableError::TrailingBytes);
        }

        let indexed = index_payload(&payload)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(SstableError::InvalidFileName)?
            .to_owned();
        let min_key = indexed
            .rows
            .first()
            .map(|row| payload[row.key_start..row.key_end].to_vec())
            .unwrap_or_default();
        let max_key = indexed
            .rows
            .last()
            .map(|row| payload[row.key_start..row.key_end].to_vec())
            .unwrap_or_default();
        let row_count = u32::try_from(indexed.rows.len())
            .map_err(|_| SstableError::TooManyRows(indexed.rows.len()))?;

        Ok(Self {
            metadata: SstableMetadata {
                tablet_id: indexed.tablet_id,
                ownership_epoch: indexed.ownership_epoch,
                min_sequence: indexed.min_sequence,
                max_sequence: indexed.max_sequence,
                row_count,
                min_key,
                max_key,
                checksum,
                file_name,
            },
            payload,
            rows: indexed.rows,
        })
    }

    pub(crate) fn metadata(&self) -> &SstableMetadata {
        &self.metadata
    }

    pub(crate) fn version_at(&self, key: &[u8], snapshot: u64) -> Option<IndexedVersion<'_>> {
        if snapshot < self.metadata.min_sequence
            || key < self.metadata.min_key.as_slice()
            || key > self.metadata.max_key.as_slice()
        {
            return None;
        }

        let index = self
            .rows
            .binary_search_by(|row| self.key(row).cmp(key))
            .ok()?;
        let row = &self.rows[index];
        let mut cursor = Cursor::at(&self.payload, row.versions_start)?;
        let mut visible = None;
        for _ in 0..row.version_count {
            let version = read_version(&mut cursor).ok()?;
            if version.sequence > snapshot {
                break;
            }
            visible = Some(version);
        }
        visible
    }

    pub(crate) fn has_contiguous_sequence_coverage_after(&self, floor: u64) -> bool {
        if self.metadata.max_sequence <= floor {
            return true;
        }

        let mut sequences = BTreeSet::new();
        for row in &self.rows {
            let Some(mut cursor) = Cursor::at(&self.payload, row.versions_start) else {
                return false;
            };
            for _ in 0..row.version_count {
                let Ok(version) = read_version(&mut cursor) else {
                    return false;
                };
                if version.sequence > floor {
                    sequences.insert(version.sequence);
                }
            }
        }

        let Some(mut expected) = floor.checked_add(1) else {
            return false;
        };
        for sequence in sequences {
            if sequence != expected {
                return false;
            }
            if sequence == self.metadata.max_sequence {
                return true;
            }
            let Some(next) = expected.checked_add(1) else {
                return false;
            };
            expected = next;
        }
        false
    }

    fn key<'a>(&'a self, row: &RowIndex) -> &'a [u8] {
        &self.payload[row.key_start..row.key_end]
    }
}

struct IndexedPayload {
    tablet_id: u64,
    ownership_epoch: u64,
    min_sequence: u64,
    max_sequence: u64,
    rows: Vec<RowIndex>,
}

fn index_payload(payload: &[u8]) -> Result<IndexedPayload, SstableError> {
    let mut cursor = Cursor::new(payload);
    let tablet_id = cursor.u64()?;
    let ownership_epoch = cursor.u64()?;
    let row_count = cursor.u32()? as usize;
    if row_count == 0 {
        return Err(SstableError::EmptyTable);
    }
    if row_count > MAX_ROWS {
        return Err(SstableError::TooManyRows(row_count));
    }

    let mut rows = Vec::with_capacity(row_count);
    let mut previous_key: Option<(usize, usize)> = None;
    let mut min_sequence = u64::MAX;
    let mut max_sequence = 0_u64;

    for _ in 0..row_count {
        let key_len = cursor.u32()? as usize;
        if key_len == 0 || key_len > MAX_KEY_BYTES || key_len > cursor.remaining() {
            return Err(SstableError::InvalidKey);
        }
        let key_start = cursor.position();
        cursor.take(key_len)?;
        let key_end = cursor.position();
        let key = &payload[key_start..key_end];
        if let Some((start, end)) = previous_key {
            if &payload[start..end] >= key {
                return Err(SstableError::RowsNotStrictlySorted);
            }
        }
        previous_key = Some((key_start, key_end));

        let version_count = cursor.u32()? as usize;
        if version_count == 0 || version_count > MAX_VERSIONS_PER_ROW {
            return Err(SstableError::InvalidHistory);
        }
        if version_count > cursor.remaining() / 9 {
            return Err(SstableError::InvalidLength);
        }
        let versions_start = cursor.position();
        let mut previous_sequence = 0_u64;
        for _ in 0..version_count {
            let version = read_version(&mut cursor)?;
            if version.sequence == 0 || version.sequence <= previous_sequence {
                return Err(SstableError::InvalidHistory);
            }
            previous_sequence = version.sequence;
            min_sequence = min_sequence.min(version.sequence);
            max_sequence = max_sequence.max(version.sequence);
        }

        rows.push(RowIndex {
            key_start,
            key_end,
            versions_start,
            version_count,
        });
    }

    if cursor.remaining() != 0 {
        return Err(SstableError::TrailingPayloadBytes);
    }

    Ok(IndexedPayload {
        tablet_id,
        ownership_epoch,
        min_sequence,
        max_sequence,
        rows,
    })
}

fn read_version<'a>(cursor: &mut Cursor<'a>) -> Result<IndexedVersion<'a>, SstableError> {
    let sequence = cursor.u64()?;
    let tag = cursor.u8()?;
    let value = match tag {
        0 => None,
        1 => {
            let len = cursor.u32()? as usize;
            if len > MAX_VALUE_BYTES || len > cursor.remaining() {
                return Err(SstableError::InvalidLength);
            }
            Some(cursor.take(len)?)
        }
        other => return Err(SstableError::InvalidValueTag(other)),
    };
    Ok(IndexedVersion { sequence, value })
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn at(bytes: &'a [u8], offset: usize) -> Option<Self> {
        (offset <= bytes.len()).then_some(Self { bytes, offset })
    }

    fn position(&self) -> usize {
        self.offset
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], SstableError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(SstableError::InvalidLength)?;
        if end > self.bytes.len() {
            return Err(SstableError::InvalidLength);
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, SstableError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, SstableError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, SstableError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

fn read_u16(file: &mut File) -> Result<u16, SstableError> {
    let mut bytes = [0_u8; 2];
    file.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(file: &mut File) -> Result<u32, SstableError> {
    let mut bytes = [0_u8; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
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
