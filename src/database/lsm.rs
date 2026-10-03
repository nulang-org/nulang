//! First persistent local-storage slice for NuDB.
//!
//! Commits are published into a mutable in-memory memtable through the
//! infallible `MvccStorage::apply_committed` boundary. Flushing that memtable to
//! an immutable sorted table is deliberately a separate fallible maintenance
//! operation, so a WAL-backed caller never acknowledges durability and then
//! encounters a second fallible storage step.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::sstable::{discover_tables, table_path, visible_version, Sstable, SstableError};
use super::tablet::{MvccStorage, TabletMutation, VersionedValue};

#[derive(Debug)]
pub struct LsmStorage {
    directory: PathBuf,
    current_sequence: u64,
    flushed_sequence: u64,
    next_generation: u64,
    mutable: BTreeMap<Vec<u8>, Vec<VersionedValue>>,
    tables: Vec<Sstable>,
}

impl LsmStorage {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, LsmError> {
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory).map_err(LsmError::io)?;
        let discovered = discover_tables(&directory).map_err(LsmError::from)?;

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
            let table = Sstable::load(&path).map_err(LsmError::from)?;
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
        let table = Sstable::from_rows(generation, min_sequence, max_sequence, &self.mutable)
            .map_err(LsmError::from)?;
        let version_count = table.version_count;
        let path = table_path(&self.directory, generation);
        table.write_atomic(&path).map_err(LsmError::from)?;

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
        let mut best = self
            .mutable
            .get(key)
            .and_then(|versions| visible_version(versions, snapshot));
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

impl From<SstableError> for LsmError {
    fn from(error: SstableError) -> Self {
        match error {
            SstableError::Io { kind, message } => Self::Io { kind, message },
            SstableError::ChecksumMismatch { path } => Self::ChecksumMismatch { path },
            SstableError::CorruptTable { path, reason } => Self::CorruptTable { path, reason },
            SstableError::UnsupportedVersion { path, version } => {
                Self::UnsupportedVersion { path, version }
            }
            SstableError::KeyTooLarge(size) => Self::KeyTooLarge(size),
            SstableError::ValueTooLarge(size) => Self::ValueTooLarge(size),
            SstableError::TableTooLarge(size) => Self::TableTooLarge(size),
            SstableError::CorruptState(reason) => Self::CorruptState(reason),
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
