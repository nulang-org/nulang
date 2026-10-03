//! First persistent local-storage slice for NuDB.
//!
//! Commits are published into a mutable in-memory memtable through the
//! infallible `MvccStorage::apply_committed` boundary. Flushing that memtable to
//! an immutable sorted table is deliberately a separate fallible maintenance
//! operation, so a WAL-backed caller never acknowledges durability and then
//! encounters a second fallible storage step.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::tablet::{MvccStorage, TabletMutation, VersionedValue};

const SST_MAGIC: &[u8; 8] = b"NUDBSST1";
const SST_VERSION: u16 = 1;
const CHECKSUM_BYTES: usize = 32;
const MAX_TABLE_BYTES: usize = 256 * 1024 * 1024;
const MAX_KEY_BYTES: usize = 4 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;
const TABLE_PREFIX: &str = "nudb-sst-";
const TABLE_SUFFIX: &str = ".sst";

#[derive(Debug)]
pub struct LsmStorage {
    directory: PathBuf,
    current_sequence: u64,
    flushed_sequence: u64,
    next_generation: u64,
    mutable: BTreeMap<Vec<u8>, Vec<VersionedValue>>,
    tables: Vec<ImmutableTable>,
}

impl LsmStorage {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, LsmError> {
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory).map_err(LsmError::io)?;

        let mut discovered = Vec::new();
        for entry in fs::read_dir(&directory).map_err(LsmError::io)? {
            let entry = entry.map_err(LsmError::io)?;
            if !entry.file_type().map_err(LsmError::io)?.is_file() {
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

        let mut tables = Vec::with_capacity(discovered.len());
        let mut expected_generation = 1_u64;
        let mut expected_min_sequence = 1_u64;
        let mut current_sequence = 0_u64;

        for (generation, path) in discovered {
            if generation != expected_generation {
                return Err(LsmError::GenerationGap {
                    expected: expected_generation,
                    presented: generation,
                });
            }
            let table = ImmutableTable::load(&path)?;
            if table.generation != generation {
                return Err(LsmError::CorruptTable {
                    path,
                    reason: "table generation does not match file name".to_string(),
                });
            }
            if table.min_sequence != expected_min_sequence {
                return Err(LsmError::SequenceGap {
                    expected: expected_min_sequence,
                    presented: table.min_sequence,
                });
            }

            current_sequence = table.max_sequence;
            expected_min_sequence = current_sequence
                .checked_add(1)
                .ok_or(LsmError::SequenceOverflow)?;
            expected_generation = expected_generation
                .checked_add(1)
                .ok_or(LsmError::GenerationOverflow)?;
            tables.push(table);
        }

        Ok(Self {
            directory,
            current_sequence,
            flushed_sequence: current_sequence,
            next_generation: expected_generation,
            mutable: BTreeMap::new(),
            tables,
        })
    }

    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    pub fn mutable_version_count(&self) -> usize {
        self.mutable.values().map(Vec::len).sum()
    }

    /// Atomically publish the committed sequence interval since the last flush
    /// as one immutable checksummed sorted table.
    ///
    /// An empty mutation set still produces a metadata-only table when commit
    /// sequence advanced, preventing restart from regressing predecessor
    /// fencing. If neither state nor sequence advanced, this is a no-op.
    pub fn flush(&mut self) -> Result<Option<FlushResult>, LsmError> {
        if self.current_sequence == self.flushed_sequence {
            return Ok(None);
        }

        let min_sequence = self
            .flushed_sequence
            .checked_add(1)
            .ok_or(LsmError::SequenceOverflow)?;
        let max_sequence = self.current_sequence;
        let generation = self.next_generation;
        let table = ImmutableTable::from_memtable(
            generation,
            min_sequence,
            max_sequence,
            &self.mutable,
        )?;
        let version_count = table.version_count;
        let path = table_path(&self.directory, generation);
        table.write_atomic(&path)?;

        self.tables.push(table);
        self.mutable.clear();
        self.flushed_sequence = max_sequence;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(LsmError::GenerationOverflow)?;

        Ok(Some(FlushResult {
            generation,
            path,
            min_sequence,
            max_sequence,
            version_count,
        }))
    }
}

impl MvccStorage for LsmStorage {
    fn current_sequence(&self) -> u64 {
        self.current_sequence
    }

    fn apply_committed(&mut self, sequence: u64, mutations: Vec<TabletMutation>) {
        debug_assert_eq!(sequence, self.current_sequence.saturating_add(1));
        for mutation in mutations {
            match mutation {
                TabletMutation::Put { key, value } => {
                    self.mutable.entry(key).or_default().push(VersionedValue {
                        sequence,
                        value: Some(value),
                    });
                }
                TabletMutation::Delete { key } => {
                    self.mutable.entry(key).or_default().push(VersionedValue {
                        sequence,
                        value: None,
                    });
                }
            }
        }
        self.current_sequence = sequence;
    }

    fn read_at(&self, key: &[u8], snapshot: u64) -> Option<&[u8]> {
        let mut best = visible_version(&self.mutable, key, snapshot);
        for table in &self.tables {
            if table.min_sequence > snapshot {
                break;
            }
            if let Some(candidate) = table.visible_version(key, snapshot) {
                if best
                    .map(|current| candidate.sequence > current.sequence)
                    .unwrap_or(true)
                {
                    best = Some(candidate);
                }
            }
        }
        best.and_then(|version| version.value.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushResult {
    pub generation: u64,
    pub path: PathBuf,
    pub min_sequence: u64,
    pub max_sequence: u64,
    pub version_count: usize,
}

#[derive(Debug)]
struct ImmutableTable {
    generation: u64,
    min_sequence: u64,
    max_sequence: u64,
    version_count: usize,
    rows: BTreeMap<Vec<u8>, Vec<VersionedValue>>,
}

impl ImmutableTable {
    fn from_memtable(
        generation: u64,
        min_sequence: u64,
        max_sequence: u64,
        rows: &BTreeMap<Vec<u8>, Vec<VersionedValue>>,
    ) -> Result<Self, LsmError> {
        if min_sequence == 0 || min_sequence > max_sequence {
            return Err(LsmError::CorruptState(
                "immutable table sequence interval is invalid".to_string(),
            ));
        }

        let mut version_count = 0_usize;
        for versions in rows.values() {
            let mut previous = 0_u64;
            for version in versions {
                if version.sequence < min_sequence
                    || version.sequence > max_sequence
                    || version.sequence <= previous
                {
                    return Err(LsmError::CorruptState(
                        "memtable contains invalid MVCC sequence history".to_string(),
                    ));
                }
                previous = version.sequence;
                version_count = version_count
                    .checked_add(1)
                    .ok_or(LsmError::TableTooLarge(usize::MAX))?;
            }
        }

        Ok(Self {
            generation,
            min_sequence,
            max_sequence,
            version_count,
            rows: rows.clone(),
        })
    }

    fn visible_version(&self, key: &[u8], snapshot: u64) -> Option<&VersionedValue> {
        visible_version(&self.rows, key, snapshot)
    }

    fn write_atomic(&self, path: &Path) -> Result<(), LsmError> {
        let encoded = self.encode()?;
        let temp = temp_path(path);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)
            .map_err(LsmError::io)?;
        file.write_all(&encoded).map_err(LsmError::io)?;
        file.sync_data().map_err(LsmError::io)?;
        drop(file);

        fs::rename(&temp, path).map_err(LsmError::io)?;
        sync_parent_directory(path).map_err(LsmError::io)?;
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>, LsmError> {
        let key_count = u64::try_from(self.rows.len())
            .map_err(|_| LsmError::TableTooLarge(self.rows.len()))?;
        let version_count = u64::try_from(self.version_count)
            .map_err(|_| LsmError::TableTooLarge(self.version_count))?;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(SST_MAGIC);
        bytes.extend_from_slice(&SST_VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&self.min_sequence.to_le_bytes());
        bytes.extend_from_slice(&self.max_sequence.to_le_bytes());
        bytes.extend_from_slice(&key_count.to_le_bytes());
        bytes.extend_from_slice(&version_count.to_le_bytes());

        for (key, versions) in &self.rows {
            if key.len() > MAX_KEY_BYTES {
                return Err(LsmError::KeyTooLarge(key.len()));
            }
            let key_len = u32::try_from(key.len()).map_err(|_| LsmError::KeyTooLarge(key.len()))?;
            let versions_len = u32::try_from(versions.len())
                .map_err(|_| LsmError::TableTooLarge(versions.len()))?;
            bytes.extend_from_slice(&key_len.to_le_bytes());
            bytes.extend_from_slice(key);
            bytes.extend_from_slice(&versions_len.to_le_bytes());

            for version in versions {
                bytes.extend_from_slice(&version.sequence.to_le_bytes());
                match &version.value {
                    None => bytes.push(0),
                    Some(value) => {
                        if value.len() > MAX_VALUE_BYTES {
                            return Err(LsmError::ValueTooLarge(value.len()));
                        }
                        let value_len = u32::try_from(value.len())
                            .map_err(|_| LsmError::ValueTooLarge(value.len()))?;
                        bytes.push(1);
                        bytes.extend_from_slice(&value_len.to_le_bytes());
                        bytes.extend_from_slice(value);
                    }
                }
            }
        }

        if bytes.len().saturating_add(CHECKSUM_BYTES) > MAX_TABLE_BYTES {
            return Err(LsmError::TableTooLarge(bytes.len() + CHECKSUM_BYTES));
        }
        let checksum = blake3::hash(&bytes);
        bytes.extend_from_slice(checksum.as_bytes());
        Ok(bytes)
    }

    fn load(path: &Path) -> Result<Self, LsmError> {
        let bytes = fs::read(path).map_err(LsmError::io)?;
        if bytes.len() > MAX_TABLE_BYTES {
            return Err(LsmError::TableTooLarge(bytes.len()));
        }
        if bytes.len() < SST_MAGIC.len() + 2 + (5 * 8) + CHECKSUM_BYTES {
            return Err(LsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "table is shorter than the fixed header".to_string(),
            });
        }

        let checksum_start = bytes.len() - CHECKSUM_BYTES;
        let (data, stored_checksum) = bytes.split_at(checksum_start);
        if stored_checksum != blake3::hash(data).as_bytes() {
            return Err(LsmError::ChecksumMismatch {
                path: path.to_path_buf(),
            });
        }

        let mut cursor = 0_usize;
        let magic = take(data, &mut cursor, SST_MAGIC.len(), path)?;
        if magic != SST_MAGIC {
            return Err(LsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "invalid immutable-table magic".to_string(),
            });
        }
        let version = read_u16(data, &mut cursor, path)?;
        if version != SST_VERSION {
            return Err(LsmError::UnsupportedVersion {
                path: path.to_path_buf(),
                version,
            });
        }

        let generation = read_u64(data, &mut cursor, path)?;
        let min_sequence = read_u64(data, &mut cursor, path)?;
        let max_sequence = read_u64(data, &mut cursor, path)?;
        let key_count = read_u64(data, &mut cursor, path)?;
        let declared_version_count = read_u64(data, &mut cursor, path)?;

        if min_sequence == 0 || min_sequence > max_sequence {
            return Err(LsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "invalid immutable-table sequence interval".to_string(),
            });
        }

        let key_count = usize::try_from(key_count).map_err(|_| LsmError::CorruptTable {
            path: path.to_path_buf(),
            reason: "key count does not fit in memory".to_string(),
        })?;
        let declared_version_count =
            usize::try_from(declared_version_count).map_err(|_| LsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "version count does not fit in memory".to_string(),
            })?;

        let mut rows = BTreeMap::new();
        let mut actual_version_count = 0_usize;
        for _ in 0..key_count {
            let key_len = read_u32(data, &mut cursor, path)? as usize;
            if key_len > MAX_KEY_BYTES {
                return Err(LsmError::CorruptTable {
                    path: path.to_path_buf(),
                    reason: "key exceeds immutable-table limit".to_string(),
                });
            }
            let key = take(data, &mut cursor, key_len, path)?.to_vec();
            let version_count = read_u32(data, &mut cursor, path)? as usize;
            if version_count == 0 {
                return Err(LsmError::CorruptTable {
                    path: path.to_path_buf(),
                    reason: "row contains no versions".to_string(),
                });
            }

            let mut versions = Vec::with_capacity(version_count);
            let mut previous = 0_u64;
            for _ in 0..version_count {
                let sequence = read_u64(data, &mut cursor, path)?;
                if sequence < min_sequence || sequence > max_sequence || sequence <= previous {
                    return Err(LsmError::CorruptTable {
                        path: path.to_path_buf(),
                        reason: "row contains invalid MVCC sequence history".to_string(),
                    });
                }
                previous = sequence;

                let tag = take(data, &mut cursor, 1, path)?[0];
                let value = match tag {
                    0 => None,
                    1 => {
                        let value_len = read_u32(data, &mut cursor, path)? as usize;
                        if value_len > MAX_VALUE_BYTES {
                            return Err(LsmError::CorruptTable {
                                path: path.to_path_buf(),
                                reason: "value exceeds immutable-table limit".to_string(),
                            });
                        }
                        Some(take(data, &mut cursor, value_len, path)?.to_vec())
                    }
                    _ => {
                        return Err(LsmError::CorruptTable {
                            path: path.to_path_buf(),
                            reason: "invalid value tag".to_string(),
                        })
                    }
                };
                versions.push(VersionedValue { sequence, value });
                actual_version_count = actual_version_count
                    .checked_add(1)
                    .ok_or(LsmError::TableTooLarge(usize::MAX))?;
            }

            if rows.insert(key, versions).is_some() {
                return Err(LsmError::CorruptTable {
                    path: path.to_path_buf(),
                    reason: "duplicate key in immutable table".to_string(),
                });
            }
        }

        if cursor != data.len() {
            return Err(LsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "immutable table has trailing payload bytes".to_string(),
            });
        }
        if actual_version_count != declared_version_count {
            return Err(LsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "immutable-table version count mismatch".to_string(),
            });
        }

        Ok(Self {
            generation,
            min_sequence,
            max_sequence,
            version_count: actual_version_count,
            rows,
        })
    }
}

fn visible_version<'a>(
    rows: &'a BTreeMap<Vec<u8>, Vec<VersionedValue>>,
    key: &[u8],
    snapshot: u64,
) -> Option<&'a VersionedValue> {
    let versions = rows.get(key)?;
    let visible = versions.partition_point(|version| version.sequence <= snapshot);
    visible.checked_sub(1).map(|index| &versions[index])
}

fn table_path(directory: &Path, generation: u64) -> PathBuf {
    directory.join(format!("{TABLE_PREFIX}{generation:020}{TABLE_SUFFIX}"))
}

fn temp_path(path: &Path) -> PathBuf {
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".tmp");
    PathBuf::from(temp)
}

fn parse_generation(file_name: &str) -> Option<u64> {
    let generation = file_name
        .strip_prefix(TABLE_PREFIX)?
        .strip_suffix(TABLE_SUFFIX)?;
    if generation.len() != 20 || !generation.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    generation.parse().ok()
}

fn take<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    len: usize,
    path: &Path,
) -> Result<&'a [u8], LsmError> {
    let end = cursor.checked_add(len).ok_or_else(|| LsmError::CorruptTable {
        path: path.to_path_buf(),
        reason: "immutable-table offset overflow".to_string(),
    })?;
    let slice = bytes
        .get(*cursor..end)
        .ok_or_else(|| LsmError::CorruptTable {
            path: path.to_path_buf(),
            reason: "immutable table ended unexpectedly".to_string(),
        })?;
    *cursor = end;
    Ok(slice)
}

fn read_u16(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u16, LsmError> {
    let raw: [u8; 2] = take(bytes, cursor, 2, path)?
        .try_into()
        .expect("fixed-width read");
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u32, LsmError> {
    let raw: [u8; 4] = take(bytes, cursor, 4, path)?
        .try_into()
        .expect("fixed-width read");
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u64, LsmError> {
    let raw: [u8; 8] = take(bytes, cursor, 8, path)?
        .try_into()
        .expect("fixed-width read");
    Ok(u64::from_le_bytes(raw))
}

fn sync_parent_directory(path: &Path) -> io::Result<()> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LsmError {
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
    GenerationGap {
        expected: u64,
        presented: u64,
    },
    SequenceGap {
        expected: u64,
        presented: u64,
    },
    SequenceOverflow,
    GenerationOverflow,
    KeyTooLarge(usize),
    ValueTooLarge(usize),
    TableTooLarge(usize),
    CorruptState(String),
}

impl LsmError {
    fn io(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for LsmError {
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
            Self::GenerationGap {
                expected,
                presented,
            } => write!(
                f,
                "immutable-table generation gap: expected {expected}, found {presented}"
            ),
            Self::SequenceGap {
                expected,
                presented,
            } => write!(
                f,
                "immutable-table sequence gap: expected {expected}, found {presented}"
            ),
            Self::SequenceOverflow => f.write_str("immutable-table sequence overflow"),
            Self::GenerationOverflow => f.write_str("immutable-table generation overflow"),
            Self::KeyTooLarge(size) => write!(f, "immutable-table key is too large ({size} bytes)"),
            Self::ValueTooLarge(size) => {
                write!(f, "immutable-table value is too large ({size} bytes)")
            }
            Self::TableTooLarge(size) => {
                write!(f, "immutable table is too large ({size} bytes)")
            }
            Self::CorruptState(reason) => write!(f, "invalid memtable state: {reason}"),
        }
    }
}

impl std::error::Error for LsmError {}
