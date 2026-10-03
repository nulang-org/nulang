//! Shared immutable sorted-table codec and in-memory lookup accelerators.
//!
//! The on-disk representation intentionally remains `NUDBSST1` version 1 so
//! tables produced by the first LSM prototype and the manifest-backed engine
//! remain mutually readable. Bloom filters and the sparse block index are
//! reconstructed on open; they accelerate reads without changing durable bytes.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::tablet::VersionedValue;

const SST_MAGIC: &[u8; 8] = b"NUDBSST1";
const SST_VERSION: u16 = 1;
const CHECKSUM_BYTES: usize = 32;
const MAX_TABLE_BYTES: usize = 256 * 1024 * 1024;
const MAX_KEY_BYTES: usize = 4 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;
const TABLE_PREFIX: &str = "nudb-sst-";
const TABLE_SUFFIX: &str = ".sst";
const BLOCK_ROWS: usize = 64;
const BLOOM_BITS_PER_KEY: usize = 10;
const BLOOM_HASHES: u64 = 7;

#[derive(Debug, Clone)]
pub(crate) struct SstableRow {
    pub(crate) key: Vec<u8>,
    pub(crate) versions: Vec<VersionedValue>,
}

#[derive(Debug, Clone)]
struct BlockIndexEntry {
    first_key: Vec<u8>,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Default)]
struct BloomFilter {
    bits: Vec<u64>,
    bit_len: usize,
}

impl BloomFilter {
    fn from_rows(rows: &[SstableRow]) -> Self {
        if rows.is_empty() {
            return Self::default();
        }
        let bit_len = rows
            .len()
            .saturating_mul(BLOOM_BITS_PER_KEY)
            .max(64)
            .next_multiple_of(64);
        let mut filter = Self {
            bits: vec![0; bit_len / 64],
            bit_len,
        };
        for row in rows {
            filter.insert(&row.key);
        }
        filter
    }

    fn insert(&mut self, key: &[u8]) {
        if self.bit_len == 0 {
            return;
        }
        let (h1, h2) = bloom_hashes(key);
        for index in 0..BLOOM_HASHES {
            let bit = h1
                .wrapping_add(index.wrapping_mul(h2))
                .wrapping_rem(self.bit_len as u64) as usize;
            self.bits[bit / 64] |= 1_u64 << (bit % 64);
        }
    }

    fn may_contain(&self, key: &[u8]) -> bool {
        if self.bit_len == 0 {
            return false;
        }
        let (h1, h2) = bloom_hashes(key);
        (0..BLOOM_HASHES).all(|index| {
            let bit = h1
                .wrapping_add(index.wrapping_mul(h2))
                .wrapping_rem(self.bit_len as u64) as usize;
            self.bits[bit / 64] & (1_u64 << (bit % 64)) != 0
        })
    }
}

fn bloom_hashes(key: &[u8]) -> (u64, u64) {
    let digest = blake3::hash(key);
    let bytes = digest.as_bytes();
    let h1 = u64::from_le_bytes(bytes[0..8].try_into().expect("fixed hash width"));
    let mut h2 = u64::from_le_bytes(bytes[8..16].try_into().expect("fixed hash width"));
    // A zero second hash would probe the same bit repeatedly. Making it odd also
    // gives better coverage when the Bloom bit count is a power of two.
    h2 |= 1;
    (h1, h2)
}

#[derive(Debug, Clone)]
pub(crate) struct Sstable {
    pub(crate) generation: u64,
    pub(crate) min_sequence: u64,
    pub(crate) max_sequence: u64,
    pub(crate) version_count: usize,
    rows: Vec<SstableRow>,
    blocks: Vec<BlockIndexEntry>,
    bloom: BloomFilter,
}

impl Sstable {
    pub(crate) fn from_rows(
        generation: u64,
        min_sequence: u64,
        max_sequence: u64,
        rows: &BTreeMap<Vec<u8>, Vec<VersionedValue>>,
    ) -> Result<Self, SstableError> {
        if generation == 0 || min_sequence == 0 || min_sequence > max_sequence {
            return Err(SstableError::CorruptState(
                "immutable table metadata is invalid".to_string(),
            ));
        }

        let mut version_count = 0_usize;
        let mut sorted_rows = Vec::with_capacity(rows.len());
        for (key, versions) in rows {
            if key.len() > MAX_KEY_BYTES {
                return Err(SstableError::KeyTooLarge(key.len()));
            }
            if versions.is_empty() {
                return Err(SstableError::CorruptState(
                    "immutable table row contains no versions".to_string(),
                ));
            }
            let mut previous = 0_u64;
            for version in versions {
                if version.sequence < min_sequence
                    || version.sequence > max_sequence
                    || version.sequence <= previous
                {
                    return Err(SstableError::CorruptState(
                        "table rows contain invalid MVCC sequence history".to_string(),
                    ));
                }
                if let Some(value) = &version.value {
                    if value.len() > MAX_VALUE_BYTES {
                        return Err(SstableError::ValueTooLarge(value.len()));
                    }
                }
                previous = version.sequence;
                version_count = version_count
                    .checked_add(1)
                    .ok_or(SstableError::TableTooLarge(usize::MAX))?;
            }
            sorted_rows.push(SstableRow {
                key: key.clone(),
                versions: versions.clone(),
            });
        }

        Ok(Self::finish(
            generation,
            min_sequence,
            max_sequence,
            version_count,
            sorted_rows,
        ))
    }

    fn finish(
        generation: u64,
        min_sequence: u64,
        max_sequence: u64,
        version_count: usize,
        rows: Vec<SstableRow>,
    ) -> Self {
        let blocks = build_block_index(&rows);
        let bloom = BloomFilter::from_rows(&rows);
        Self {
            generation,
            min_sequence,
            max_sequence,
            version_count,
            rows,
            blocks,
            bloom,
        }
    }

    pub(crate) fn rows(&self) -> &[SstableRow] {
        &self.rows
    }

    pub(crate) fn visible_version(&self, key: &[u8], snapshot: u64) -> Option<&VersionedValue> {
        if !self.bloom.may_contain(key) {
            return None;
        }
        let row = self.find_row(key)?;
        visible_version(&row.versions, snapshot)
    }

    pub(crate) fn range_rows(&self, start: &[u8], end_exclusive: Option<&[u8]>) -> &[SstableRow] {
        if let Some(end) = end_exclusive {
            if end <= start {
                return &self.rows[0..0];
            }
        }
        let start_index = self.lower_bound(start);
        let end_index = match end_exclusive {
            Some(end) => self.rows.partition_point(|row| row.key.as_slice() < end),
            None => self.rows.len(),
        };
        if start_index >= end_index {
            &self.rows[0..0]
        } else {
            &self.rows[start_index..end_index]
        }
    }

    fn find_row(&self, key: &[u8]) -> Option<&SstableRow> {
        let block = self.block_for_key(key)?;
        self.rows[block.start..block.end]
            .binary_search_by(|row| row.key.as_slice().cmp(key))
            .ok()
            .map(|offset| &self.rows[block.start + offset])
    }

    fn lower_bound(&self, key: &[u8]) -> usize {
        if self.rows.is_empty() {
            return 0;
        }
        let Some(block) = self.block_for_key(key) else {
            return 0;
        };
        let slice = &self.rows[block.start..block.end];
        block.start + slice.partition_point(|row| row.key.as_slice() < key)
    }

    fn block_for_key(&self, key: &[u8]) -> Option<&BlockIndexEntry> {
        if self.blocks.is_empty() {
            return None;
        }
        let position = self
            .blocks
            .partition_point(|block| block.first_key.as_slice() <= key);
        let index = position.saturating_sub(1);
        self.blocks.get(index)
    }

    pub(crate) fn write_atomic(&self, path: &Path) -> Result<(), SstableError> {
        let encoded = self.encode()?;
        atomic_write(path, &encoded).map_err(SstableError::io)
    }

    fn encode(&self) -> Result<Vec<u8>, SstableError> {
        let key_count = u64::try_from(self.rows.len())
            .map_err(|_| SstableError::TableTooLarge(self.rows.len()))?;
        let version_count = u64::try_from(self.version_count)
            .map_err(|_| SstableError::TableTooLarge(self.version_count))?;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(SST_MAGIC);
        bytes.extend_from_slice(&SST_VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&self.min_sequence.to_le_bytes());
        bytes.extend_from_slice(&self.max_sequence.to_le_bytes());
        bytes.extend_from_slice(&key_count.to_le_bytes());
        bytes.extend_from_slice(&version_count.to_le_bytes());

        for row in &self.rows {
            if row.key.len() > MAX_KEY_BYTES {
                return Err(SstableError::KeyTooLarge(row.key.len()));
            }
            let key_len = u32::try_from(row.key.len())
                .map_err(|_| SstableError::KeyTooLarge(row.key.len()))?;
            let versions_len = u32::try_from(row.versions.len())
                .map_err(|_| SstableError::TableTooLarge(row.versions.len()))?;
            bytes.extend_from_slice(&key_len.to_le_bytes());
            bytes.extend_from_slice(&row.key);
            bytes.extend_from_slice(&versions_len.to_le_bytes());

            for version in &row.versions {
                bytes.extend_from_slice(&version.sequence.to_le_bytes());
                match &version.value {
                    None => bytes.push(0),
                    Some(value) => {
                        if value.len() > MAX_VALUE_BYTES {
                            return Err(SstableError::ValueTooLarge(value.len()));
                        }
                        let value_len = u32::try_from(value.len())
                            .map_err(|_| SstableError::ValueTooLarge(value.len()))?;
                        bytes.push(1);
                        bytes.extend_from_slice(&value_len.to_le_bytes());
                        bytes.extend_from_slice(value);
                    }
                }
            }
        }

        if bytes.len().saturating_add(CHECKSUM_BYTES) > MAX_TABLE_BYTES {
            return Err(SstableError::TableTooLarge(bytes.len() + CHECKSUM_BYTES));
        }
        let checksum = blake3::hash(&bytes);
        bytes.extend_from_slice(checksum.as_bytes());
        Ok(bytes)
    }

    pub(crate) fn load(path: &Path) -> Result<Self, SstableError> {
        let bytes = fs::read(path).map_err(SstableError::io)?;
        if bytes.len() > MAX_TABLE_BYTES {
            return Err(SstableError::TableTooLarge(bytes.len()));
        }
        if bytes.len() < SST_MAGIC.len() + 2 + (5 * 8) + CHECKSUM_BYTES {
            return Err(SstableError::CorruptTable {
                path: path.to_path_buf(),
                reason: "table is shorter than the fixed header".to_string(),
            });
        }

        let checksum_start = bytes.len() - CHECKSUM_BYTES;
        let (data, stored_checksum) = bytes.split_at(checksum_start);
        if stored_checksum != blake3::hash(data).as_bytes() {
            return Err(SstableError::ChecksumMismatch {
                path: path.to_path_buf(),
            });
        }

        let mut cursor = 0_usize;
        if take(data, &mut cursor, SST_MAGIC.len(), path)? != SST_MAGIC {
            return Err(SstableError::CorruptTable {
                path: path.to_path_buf(),
                reason: "invalid immutable-table magic".to_string(),
            });
        }
        let version = read_u16(data, &mut cursor, path)?;
        if version != SST_VERSION {
            return Err(SstableError::UnsupportedVersion {
                path: path.to_path_buf(),
                version,
            });
        }

        let generation = read_u64(data, &mut cursor, path)?;
        let min_sequence = read_u64(data, &mut cursor, path)?;
        let max_sequence = read_u64(data, &mut cursor, path)?;
        let key_count = read_u64(data, &mut cursor, path)?;
        let declared_version_count = read_u64(data, &mut cursor, path)?;
        if generation == 0 || min_sequence == 0 || min_sequence > max_sequence {
            return Err(SstableError::CorruptTable {
                path: path.to_path_buf(),
                reason: "invalid immutable-table metadata".to_string(),
            });
        }

        let key_count = usize::try_from(key_count).map_err(|_| SstableError::CorruptTable {
            path: path.to_path_buf(),
            reason: "key count does not fit in memory".to_string(),
        })?;
        if key_count > data.len() {
            return Err(SstableError::CorruptTable {
                path: path.to_path_buf(),
                reason: "key count exceeds payload capacity".to_string(),
            });
        }
        let declared_version_count =
            usize::try_from(declared_version_count).map_err(|_| SstableError::CorruptTable {
                path: path.to_path_buf(),
                reason: "version count does not fit in memory".to_string(),
            })?;
        if declared_version_count > data.len() {
            return Err(SstableError::CorruptTable {
                path: path.to_path_buf(),
                reason: "version count exceeds payload capacity".to_string(),
            });
        }

        let mut rows = Vec::with_capacity(key_count);
        let mut actual_version_count = 0_usize;
        let mut previous_key: Option<Vec<u8>> = None;
        for _ in 0..key_count {
            let key_len = read_u32(data, &mut cursor, path)? as usize;
            if key_len > MAX_KEY_BYTES {
                return Err(SstableError::CorruptTable {
                    path: path.to_path_buf(),
                    reason: "key exceeds immutable-table limit".to_string(),
                });
            }
            let key = take(data, &mut cursor, key_len, path)?.to_vec();
            if previous_key
                .as_ref()
                .map(|previous| previous.as_slice() >= key.as_slice())
                .unwrap_or(false)
            {
                return Err(SstableError::CorruptTable {
                    path: path.to_path_buf(),
                    reason: "immutable-table keys are not strictly sorted".to_string(),
                });
            }
            previous_key = Some(key.clone());

            let version_count = read_u32(data, &mut cursor, path)? as usize;
            if version_count == 0 || version_count > data.len() {
                return Err(SstableError::CorruptTable {
                    path: path.to_path_buf(),
                    reason: "row contains an invalid version count".to_string(),
                });
            }
            let mut versions = Vec::with_capacity(version_count);
            let mut previous_sequence = 0_u64;
            for _ in 0..version_count {
                let sequence = read_u64(data, &mut cursor, path)?;
                if sequence < min_sequence
                    || sequence > max_sequence
                    || sequence <= previous_sequence
                {
                    return Err(SstableError::CorruptTable {
                        path: path.to_path_buf(),
                        reason: "row contains invalid MVCC sequence history".to_string(),
                    });
                }
                previous_sequence = sequence;
                let tag = take(data, &mut cursor, 1, path)?[0];
                let value = match tag {
                    0 => None,
                    1 => {
                        let value_len = read_u32(data, &mut cursor, path)? as usize;
                        if value_len > MAX_VALUE_BYTES {
                            return Err(SstableError::CorruptTable {
                                path: path.to_path_buf(),
                                reason: "value exceeds immutable-table limit".to_string(),
                            });
                        }
                        Some(take(data, &mut cursor, value_len, path)?.to_vec())
                    }
                    _ => {
                        return Err(SstableError::CorruptTable {
                            path: path.to_path_buf(),
                            reason: "invalid value tag".to_string(),
                        });
                    }
                };
                versions.push(VersionedValue { sequence, value });
                actual_version_count = actual_version_count
                    .checked_add(1)
                    .ok_or(SstableError::TableTooLarge(usize::MAX))?;
            }
            rows.push(SstableRow { key, versions });
        }

        if cursor != data.len() {
            return Err(SstableError::CorruptTable {
                path: path.to_path_buf(),
                reason: "immutable table has trailing payload bytes".to_string(),
            });
        }
        if actual_version_count != declared_version_count {
            return Err(SstableError::CorruptTable {
                path: path.to_path_buf(),
                reason: "immutable-table version count mismatch".to_string(),
            });
        }

        Ok(Self::finish(
            generation,
            min_sequence,
            max_sequence,
            actual_version_count,
            rows,
        ))
    }
}

fn build_block_index(rows: &[SstableRow]) -> Vec<BlockIndexEntry> {
    let mut blocks = Vec::with_capacity(rows.len().div_ceil(BLOCK_ROWS));
    for start in (0..rows.len()).step_by(BLOCK_ROWS) {
        let end = start.saturating_add(BLOCK_ROWS).min(rows.len());
        blocks.push(BlockIndexEntry {
            first_key: rows[start].key.clone(),
            start,
            end,
        });
    }
    blocks
}

pub(crate) fn visible_version(
    versions: &[VersionedValue],
    snapshot: u64,
) -> Option<&VersionedValue> {
    let visible = versions.partition_point(|version| version.sequence <= snapshot);
    visible.checked_sub(1).map(|index| &versions[index])
}

pub(crate) fn table_path(directory: &Path, generation: u64) -> PathBuf {
    directory.join(format!("{TABLE_PREFIX}{generation:020}{TABLE_SUFFIX}"))
}

pub(crate) fn parse_generation(file_name: &str) -> Option<u64> {
    let generation = file_name
        .strip_prefix(TABLE_PREFIX)?
        .strip_suffix(TABLE_SUFFIX)?;
    if generation.len() != 20 || !generation.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    generation.parse().ok()
}

pub(crate) fn discover_tables(directory: &Path) -> Result<Vec<(u64, PathBuf)>, SstableError> {
    let mut discovered = Vec::new();
    for entry in fs::read_dir(directory).map_err(SstableError::io)? {
        let entry = entry.map_err(SstableError::io)?;
        if !entry.file_type().map_err(SstableError::io)?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(generation) = parse_generation(file_name) else {
            continue;
        };
        discovered.push((generation, entry.path()));
    }
    discovered.sort_by_key(|(generation, _)| *generation);
    Ok(discovered)
}

pub(crate) fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_data()?;
    drop(file);
    fs::rename(&temp, path)?;
    sync_parent_directory(path)
}

fn take<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    len: usize,
    path: &Path,
) -> Result<&'a [u8], SstableError> {
    let end = cursor.checked_add(len).ok_or_else(|| SstableError::CorruptTable {
        path: path.to_path_buf(),
        reason: "immutable-table offset overflow".to_string(),
    })?;
    let slice = bytes.get(*cursor..end).ok_or_else(|| SstableError::CorruptTable {
        path: path.to_path_buf(),
        reason: "immutable table ended unexpectedly".to_string(),
    })?;
    *cursor = end;
    Ok(slice)
}

fn read_u16(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u16, SstableError> {
    let raw: [u8; 2] = take(bytes, cursor, 2, path)?
        .try_into()
        .expect("fixed-width read");
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u32, SstableError> {
    let raw: [u8; 4] = take(bytes, cursor, 4, path)?
        .try_into()
        .expect("fixed-width read");
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u64, SstableError> {
    let raw: [u8; 8] = take(bytes, cursor, 8, path)?
        .try_into()
        .expect("fixed-width read");
    Ok(u64::from_le_bytes(raw))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SstableError {
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    ChecksumMismatch {
        path: PathBuf,
    },
    CorruptTable {
        path: PathBuf,
        reason: String,
    },
    UnsupportedVersion {
        path: PathBuf,
        version: u16,
    },
    KeyTooLarge(usize),
    ValueTooLarge(usize),
    TableTooLarge(usize),
    CorruptState(String),
}

impl SstableError {
    fn io(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for SstableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message, .. } => write!(f, "immutable-table I/O error: {message}"),
            Self::ChecksumMismatch { path } => {
                write!(f, "immutable-table checksum mismatch: {}", path.display())
            }
            Self::CorruptTable { path, reason } => {
                write!(f, "corrupt immutable table {}: {reason}", path.display())
            }
            Self::UnsupportedVersion { path, version } => write!(
                f,
                "unsupported immutable-table version {version}: {}",
                path.display()
            ),
            Self::KeyTooLarge(size) => {
                write!(f, "immutable-table key is too large ({size} bytes)")
            }
            Self::ValueTooLarge(size) => {
                write!(f, "immutable-table value is too large ({size} bytes)")
            }
            Self::TableTooLarge(size) => {
                write!(f, "immutable table is too large ({size} bytes)")
            }
            Self::CorruptState(reason) => write!(f, "invalid immutable-table state: {reason}"),
        }
    }
}

impl std::error::Error for SstableError {}
