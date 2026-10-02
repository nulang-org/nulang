//! Immutable NuDB SSTable v1 format.
//!
//! SSTables are durable flush artifacts, but are not yet a recovery authority.
//! WAL/checkpoint recovery remains canonical until a later serving-integration slice.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::tablet::{TabletSnapshotRow, VersionedValue};

const SSTABLE_MAGIC: &[u8; 8] = b"NUDBSST1";
const SSTABLE_VERSION: u16 = 1;
const MAX_SSTABLE_BYTES: usize = 256 * 1024 * 1024;
const MAX_ROWS: usize = 4_000_000;
const MAX_VERSIONS_PER_ROW: usize = 65_536;
const MAX_KEY_BYTES: usize = 16 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SstableMetadata {
    pub(crate) tablet_id: u64,
    pub(crate) ownership_epoch: u64,
    pub(crate) min_sequence: u64,
    pub(crate) max_sequence: u64,
    pub(crate) row_count: u32,
    pub(crate) min_key: Vec<u8>,
    pub(crate) max_key: Vec<u8>,
    pub(crate) checksum: [u8; 32],
    pub(crate) file_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Sstable {
    metadata: SstableMetadata,
    rows: Vec<TabletSnapshotRow>,
}

impl Sstable {
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

        let (tablet_id, ownership_epoch, rows, min_sequence, max_sequence) =
            decode_payload(&payload)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(SstableError::InvalidFileName)?
            .to_owned();
        let min_key = rows.first().map(|row| row.key.clone()).unwrap_or_default();
        let max_key = rows.last().map(|row| row.key.clone()).unwrap_or_default();
        let row_count =
            u32::try_from(rows.len()).map_err(|_| SstableError::TooManyRows(rows.len()))?;
        Ok(Self {
            metadata: SstableMetadata {
                tablet_id,
                ownership_epoch,
                min_sequence,
                max_sequence,
                row_count,
                min_key,
                max_key,
                checksum,
                file_name,
            },
            rows,
        })
    }

    pub(crate) fn tablet_id(&self) -> u64 {
        self.metadata.tablet_id
    }

    pub(crate) fn metadata(&self) -> &SstableMetadata {
        &self.metadata
    }

    pub(crate) fn get_at(&self, key: &[u8], snapshot: u64) -> Result<Option<&[u8]>, SstableError> {
        let Ok(index) = self
            .rows
            .binary_search_by(|row| row.key.as_slice().cmp(key))
        else {
            return Ok(None);
        };
        let version = self.rows[index]
            .versions
            .iter()
            .rev()
            .find(|version| version.sequence <= snapshot);
        Ok(version.and_then(|version| version.value.as_deref()))
    }
}

pub(crate) fn expected_metadata(
    file_name: String,
    tablet_id: u64,
    ownership_epoch: u64,
    rows: &[TabletSnapshotRow],
) -> Result<SstableMetadata, SstableError> {
    if rows.is_empty() {
        return Err(SstableError::EmptyTable);
    }
    validate_rows(rows)?;
    let payload = encode_payload(tablet_id, ownership_epoch, rows)?;
    if payload.len() > MAX_SSTABLE_BYTES {
        return Err(SstableError::TooLarge(payload.len()));
    }
    let checksum = *blake3::hash(&payload).as_bytes();
    let min_sequence = rows
        .iter()
        .flat_map(|row| row.versions.iter())
        .map(|version| version.sequence)
        .min()
        .unwrap();
    let max_sequence = rows
        .iter()
        .flat_map(|row| row.versions.iter())
        .map(|version| version.sequence)
        .max()
        .unwrap();
    Ok(SstableMetadata {
        tablet_id,
        ownership_epoch,
        min_sequence,
        max_sequence,
        row_count: u32::try_from(rows.len()).map_err(|_| SstableError::TooManyRows(rows.len()))?,
        min_key: rows.first().unwrap().key.clone(),
        max_key: rows.last().unwrap().key.clone(),
        checksum,
        file_name,
    })
}

pub(crate) fn write_sstable(
    path: &Path,
    tablet_id: u64,
    ownership_epoch: u64,
    rows: &[TabletSnapshotRow],
) -> Result<SstableMetadata, SstableError> {
    if rows.is_empty() {
        return Err(SstableError::EmptyTable);
    }
    validate_rows(rows)?;
    let payload = encode_payload(tablet_id, ownership_epoch, rows)?;
    if payload.len() > MAX_SSTABLE_BYTES {
        return Err(SstableError::TooLarge(payload.len()));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temp = appended_path(path, ".tmp");
    let checksum = *blake3::hash(&payload).as_bytes();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    file.write_all(SSTABLE_MAGIC)?;
    file.write_all(&SSTABLE_VERSION.to_le_bytes())?;
    file.write_all(&(payload.len() as u32).to_le_bytes())?;
    file.write_all(&payload)?;
    file.write_all(&checksum)?;
    #[cfg(test)]
    super::interruption::hit(super::interruption::StorageInterruptionPoint::SstableAfterTempWrite)?;
    file.sync_data()?;
    #[cfg(test)]
    super::interruption::hit(super::interruption::StorageInterruptionPoint::SstableAfterTempSync)?;
    drop(file);
    fs::rename(&temp, path)?;
    #[cfg(test)]
    super::interruption::hit(super::interruption::StorageInterruptionPoint::SstableAfterRename)?;
    sync_parent_directory(path)?;
    #[cfg(test)]
    super::interruption::hit(
        super::interruption::StorageInterruptionPoint::SstableAfterDirectorySync,
    )?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SstableError::InvalidFileName)?
        .to_owned();
    expected_metadata(file_name, tablet_id, ownership_epoch, rows)
}

fn encode_payload(
    tablet_id: u64,
    ownership_epoch: u64,
    rows: &[TabletSnapshotRow],
) -> Result<Vec<u8>, SstableError> {
    let mut out = Vec::new();
    out.extend_from_slice(&tablet_id.to_le_bytes());
    out.extend_from_slice(&ownership_epoch.to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for row in rows {
        write_len_prefixed(&mut out, &row.key)?;
        out.extend_from_slice(&(row.versions.len() as u32).to_le_bytes());
        for version in &row.versions {
            out.extend_from_slice(&version.sequence.to_le_bytes());
            match &version.value {
                Some(value) => {
                    out.push(1);
                    write_len_prefixed(&mut out, value)?;
                }
                None => out.push(0),
            }
        }
    }
    Ok(out)
}

fn decode_payload(
    payload: &[u8],
) -> Result<(u64, u64, Vec<TabletSnapshotRow>, u64, u64), SstableError> {
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
    for _ in 0..row_count {
        let key = cursor.bytes(MAX_KEY_BYTES)?;
        let version_count = cursor.u32()? as usize;
        if version_count == 0 || version_count > MAX_VERSIONS_PER_ROW {
            return Err(SstableError::InvalidHistory);
        }
        if version_count > cursor.remaining() / 9 {
            return Err(SstableError::InvalidLength);
        }
        let mut versions = Vec::with_capacity(version_count);
        for _ in 0..version_count {
            let sequence = cursor.u64()?;
            let tag = cursor.u8()?;
            let value = match tag {
                0 => None,
                1 => Some(cursor.bytes(MAX_VALUE_BYTES)?),
                other => return Err(SstableError::InvalidValueTag(other)),
            };
            versions.push(VersionedValue { sequence, value });
        }
        rows.push(TabletSnapshotRow { key, versions });
    }
    if cursor.remaining() != 0 {
        return Err(SstableError::TrailingPayloadBytes);
    }
    validate_rows(&rows)?;
    let min_sequence = rows
        .iter()
        .flat_map(|row| row.versions.iter())
        .map(|v| v.sequence)
        .min()
        .unwrap();
    let max_sequence = rows
        .iter()
        .flat_map(|row| row.versions.iter())
        .map(|v| v.sequence)
        .max()
        .unwrap();
    Ok((tablet_id, ownership_epoch, rows, min_sequence, max_sequence))
}

fn validate_rows(rows: &[TabletSnapshotRow]) -> Result<(), SstableError> {
    if rows.len() > MAX_ROWS {
        return Err(SstableError::TooManyRows(rows.len()));
    }
    let mut previous_key: Option<&[u8]> = None;
    for row in rows {
        if row.key.is_empty() || row.key.len() > MAX_KEY_BYTES {
            return Err(SstableError::InvalidKey);
        }
        if previous_key.is_some_and(|key| key >= row.key.as_slice()) {
            return Err(SstableError::RowsNotStrictlySorted);
        }
        previous_key = Some(&row.key);
        if row.versions.is_empty() || row.versions.len() > MAX_VERSIONS_PER_ROW {
            return Err(SstableError::InvalidHistory);
        }
        let mut previous_sequence = 0_u64;
        for version in &row.versions {
            if version.sequence == 0 || version.sequence <= previous_sequence {
                return Err(SstableError::InvalidHistory);
            }
            previous_sequence = version.sequence;
            if version
                .value
                .as_ref()
                .is_some_and(|value| value.len() > MAX_VALUE_BYTES)
            {
                return Err(SstableError::ValueTooLarge);
            }
        }
    }
    Ok(())
}

fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SstableError> {
    let len = u32::try_from(bytes.len()).map_err(|_| SstableError::InvalidLength)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
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
    fn bytes(&mut self, max: usize) -> Result<Vec<u8>, SstableError> {
        let len = self.u32()? as usize;
        if len > max || len > self.remaining() {
            return Err(SstableError::InvalidLength);
        }
        Ok(self.take(len)?.to_vec())
    }
}

fn read_u16(file: &mut File) -> io::Result<u16> {
    let mut b = [0; 2];
    file.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn read_u32(file: &mut File) -> io::Result<u32> {
    let mut b = [0; 4];
    file.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn appended_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SstableError {
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    InvalidHeader,
    UnsupportedVersion(u16),
    TooLarge(usize),
    ChecksumMismatch,
    TrailingBytes,
    TrailingPayloadBytes,
    InvalidFileName,
    EmptyTable,
    TooManyRows(usize),
    InvalidKey,
    RowsNotStrictlySorted,
    InvalidHistory,
    InvalidValueTag(u8),
    ValueTooLarge,
    InvalidLength,
    ExistingFileMismatch(String),
}

impl From<io::Error> for SstableError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}
impl fmt::Display for SstableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NuDB SSTable error: {self:?}")
    }
}
impl std::error::Error for SstableError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(1);
    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_nudb_sstable_{name}_{}_{}.sst",
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
    fn write_open_and_mvcc_lookup_round_trip() {
        let path = temp_path("roundtrip");
        let _ = fs::remove_file(&path);
        let metadata = write_sstable(&path, 42, 7, &rows()).unwrap();
        assert_eq!(metadata.min_sequence, 1);
        assert_eq!(metadata.max_sequence, 4);
        assert_eq!(metadata.row_count, 2);
        let table = Sstable::open(&path).unwrap();
        assert_eq!(table.tablet_id(), 42);
        assert_eq!(table.get_at(b"alpha", 1).unwrap(), Some(&b"one"[..]));
        assert_eq!(table.get_at(b"alpha", 3).unwrap(), Some(&b"three"[..]));
        assert_eq!(table.get_at(b"beta", 3).unwrap(), Some(&b"two"[..]));
        assert_eq!(table.get_at(b"beta", 4).unwrap(), None);
        assert_eq!(table.get_at(b"missing", 4).unwrap(), None);
        let _ = fs::remove_file(path);
    }
    #[test]
    fn complete_checksum_corruption_fails_closed() {
        let path = temp_path("corrupt");
        let _ = fs::remove_file(&path);
        write_sstable(&path, 42, 7, &rows()).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[16] ^= 0x40;
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            Sstable::open(&path).unwrap_err(),
            SstableError::ChecksumMismatch
        );
        let _ = fs::remove_file(path);
    }
    #[test]
    fn truncated_sstable_fails_closed() {
        let path = temp_path("truncated");
        let _ = fs::remove_file(&path);
        write_sstable(&path, 42, 7, &rows()).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 9);
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            Sstable::open(&path),
            Err(SstableError::Io { .. }) | Err(SstableError::InvalidLength)
        ));
        let _ = fs::remove_file(path);
    }
}
