//! Manifest-backed LSM storage with crash-safe whole-table-set compaction.
//!
//! Commit publication remains memory-only and infallible. Flush and compaction
//! are separate fallible maintenance operations. Immutable tables are written
//! and synced before a checksummed manifest atomically makes them authoritative;
//! obsolete tables are reclaimed only after that manifest swap succeeds.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::tablet::{MvccStorage, TabletMutation, VersionedValue};

const SST_MAGIC: &[u8; 8] = b"NUDBSST1";
const SST_VERSION: u16 = 1;
const MANIFEST_MAGIC: &[u8; 8] = b"NUDBMAN1";
const MANIFEST_VERSION: u16 = 1;
const CHECKSUM_BYTES: usize = 32;
const MAX_TABLE_BYTES: usize = 256 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_KEY_BYTES: usize = 4 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;
const TABLE_PREFIX: &str = "nudb-sst-";
const TABLE_SUFFIX: &str = ".sst";
const MANIFEST_FILE: &str = "MANIFEST";

#[derive(Debug)]
pub struct ManagedLsmStorage {
    directory: PathBuf,
    current_sequence: u64,
    flushed_sequence: u64,
    oldest_readable_sequence: u64,
    next_generation: u64,
    mutable: BTreeMap<Vec<u8>, Vec<VersionedValue>>,
    tables: Vec<ImmutableTable>,
}

impl ManagedLsmStorage {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, ManagedLsmError> {
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory).map_err(ManagedLsmError::io)?;
        let manifest_path = directory.join(MANIFEST_FILE);

        let manifest = if manifest_path.exists() {
            Manifest::load(&manifest_path)?
        } else {
            let discovered = discover_legacy_tables(&directory)?;
            let manifest = Manifest::from_legacy_tables(&discovered)?;
            manifest.write_atomic(&manifest_path)?;
            manifest
        };

        let mut tables = Vec::with_capacity(manifest.active_generations.len());
        let mut expected_min_sequence = 1_u64;
        let mut current_sequence = 0_u64;

        for generation in &manifest.active_generations {
            let path = table_path(&directory, *generation);
            let table = ImmutableTable::load(&path)?;
            if table.generation != *generation {
                return Err(ManagedLsmError::CorruptTable {
                    path,
                    reason: "table generation does not match manifest".to_string(),
                });
            }
            if table.min_sequence != expected_min_sequence {
                return Err(ManagedLsmError::SequenceGap {
                    expected: expected_min_sequence,
                    presented: table.min_sequence,
                });
            }
            current_sequence = table.max_sequence;
            expected_min_sequence = current_sequence
                .checked_add(1)
                .ok_or(ManagedLsmError::SequenceOverflow)?;
            tables.push(table);
        }

        if current_sequence != manifest.flushed_sequence {
            return Err(ManagedLsmError::CorruptManifest {
                path: manifest_path,
                reason: format!(
                    "manifest flushed sequence {} does not match active table tail {}",
                    manifest.flushed_sequence, current_sequence
                ),
            });
        }

        let next_generation = manifest
            .active_generations
            .last()
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(ManagedLsmError::GenerationOverflow)?;

        Ok(Self {
            directory,
            current_sequence,
            flushed_sequence: current_sequence,
            oldest_readable_sequence: manifest.oldest_readable_sequence,
            next_generation,
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

    pub fn flush(&mut self) -> Result<Option<ManagedFlushResult>, ManagedLsmError> {
        if self.current_sequence == self.flushed_sequence {
            return Ok(None);
        }

        let min_sequence = self
            .flushed_sequence
            .checked_add(1)
            .ok_or(ManagedLsmError::SequenceOverflow)?;
        let max_sequence = self.current_sequence;
        let generation = self.next_generation;
        let table = ImmutableTable::from_rows(
            generation,
            min_sequence,
            max_sequence,
            &self.mutable,
        )?;
        let version_count = table.version_count;
        let path = table_path(&self.directory, generation);

        // A failed prior publication may have left this generation orphaned.
        // Because it is not named by the current manifest it is safe to replace.
        if path.exists() {
            fs::remove_file(&path).map_err(ManagedLsmError::io)?;
        }
        table.write_atomic(&path)?;

        let mut generations: Vec<u64> = self.tables.iter().map(|table| table.generation).collect();
        generations.push(generation);
        let manifest = Manifest {
            oldest_readable_sequence: self.oldest_readable_sequence,
            flushed_sequence: max_sequence,
            active_generations: generations,
        };
        manifest.write_atomic(&self.directory.join(MANIFEST_FILE))?;

        self.tables.push(table);
        self.mutable.clear();
        self.flushed_sequence = max_sequence;
        self.next_generation = generation
            .checked_add(1)
            .ok_or(ManagedLsmError::GenerationOverflow)?;

        Ok(Some(ManagedFlushResult {
            generation,
            path,
            min_sequence,
            max_sequence,
            version_count,
        }))
    }

    /// Merge every currently active immutable table and publish the replacement
    /// table set through one atomic manifest swap.
    ///
    /// Compaction is deliberately forbidden while committed mutations remain in
    /// the mutable memtable. Call `flush` first so the table set represents a
    /// complete durable sequence interval.
    pub fn compact_all(&mut self, safe_point: u64) -> Result<CompactionResult, ManagedLsmError> {
        if self.current_sequence != self.flushed_sequence {
            return Err(ManagedLsmError::UnflushedStateForCompaction {
                current_sequence: self.current_sequence,
                flushed_sequence: self.flushed_sequence,
            });
        }
        if safe_point > self.current_sequence {
            return Err(ManagedLsmError::SafePointAhead {
                current_sequence: self.current_sequence,
                safe_point,
            });
        }

        let target_safe_point = safe_point.max(self.oldest_readable_sequence);
        let input_tables = self.tables.len();
        let old_paths: Vec<PathBuf> = self
            .tables
            .iter()
            .map(|table| table_path(&self.directory, table.generation))
            .collect();

        if self.current_sequence == 0 {
            let manifest = Manifest {
                oldest_readable_sequence: 0,
                flushed_sequence: 0,
                active_generations: Vec::new(),
            };
            manifest.write_atomic(&self.directory.join(MANIFEST_FILE))?;
            return Ok(CompactionResult {
                input_tables,
                output_tables: 0,
                safe_point: 0,
                versions_removed: 0,
                keys_removed: 0,
                obsolete_files_retained: 0,
            });
        }

        let mut rows: BTreeMap<Vec<u8>, Vec<VersionedValue>> = BTreeMap::new();
        for table in &self.tables {
            for (key, versions) in &table.rows {
                rows.entry(key.clone()).or_default().extend(versions.clone());
            }
        }

        let (versions_removed, keys_removed) = collect_rows(&mut rows, target_safe_point);
        let generation = self.next_generation;
        let replacement =
            ImmutableTable::from_rows(generation, 1, self.current_sequence, &rows)?;
        let replacement_path = table_path(&self.directory, generation);
        if replacement_path.exists() {
            fs::remove_file(&replacement_path).map_err(ManagedLsmError::io)?;
        }
        replacement.write_atomic(&replacement_path)?;

        let manifest = Manifest {
            oldest_readable_sequence: target_safe_point,
            flushed_sequence: self.current_sequence,
            active_generations: vec![generation],
        };
        manifest.write_atomic(&self.directory.join(MANIFEST_FILE))?;

        self.tables = vec![replacement];
        self.oldest_readable_sequence = target_safe_point;
        self.next_generation = generation
            .checked_add(1)
            .ok_or(ManagedLsmError::GenerationOverflow)?;

        // Once the manifest names only the replacement table, old tables are
        // unreachable after restart. Deletion is therefore cleanup, not part of
        // the atomic correctness boundary.
        let mut obsolete_files_retained = 0_usize;
        for path in old_paths {
            if path == replacement_path {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => obsolete_files_retained += 1,
            }
        }
        sync_parent_directory(&replacement_path).map_err(ManagedLsmError::io)?;

        Ok(CompactionResult {
            input_tables,
            output_tables: 1,
            safe_point: target_safe_point,
            versions_removed,
            keys_removed,
            obsolete_files_retained,
        })
    }
}

impl MvccStorage for ManagedLsmStorage {
    fn current_sequence(&self) -> u64 {
        self.current_sequence
    }

    fn oldest_readable_sequence(&self) -> u64 {
        self.oldest_readable_sequence
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
pub struct ManagedFlushResult {
    pub generation: u64,
    pub path: PathBuf,
    pub min_sequence: u64,
    pub max_sequence: u64,
    pub version_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    pub input_tables: usize,
    pub output_tables: usize,
    pub safe_point: u64,
    pub versions_removed: usize,
    pub keys_removed: usize,
    pub obsolete_files_retained: usize,
}

#[derive(Debug, Clone)]
struct Manifest {
    oldest_readable_sequence: u64,
    flushed_sequence: u64,
    active_generations: Vec<u64>,
}

impl Manifest {
    fn from_legacy_tables(discovered: &[(u64, PathBuf)]) -> Result<Self, ManagedLsmError> {
        let mut expected_generation = 1_u64;
        let mut expected_min_sequence = 1_u64;
        let mut flushed_sequence = 0_u64;
        let mut generations = Vec::with_capacity(discovered.len());

        for (generation, path) in discovered {
            if *generation != expected_generation {
                return Err(ManagedLsmError::GenerationGap {
                    expected: expected_generation,
                    presented: *generation,
                });
            }
            let table = ImmutableTable::load(path)?;
            if table.generation != *generation {
                return Err(ManagedLsmError::CorruptTable {
                    path: path.clone(),
                    reason: "table generation does not match file name".to_string(),
                });
            }
            if table.min_sequence != expected_min_sequence {
                return Err(ManagedLsmError::SequenceGap {
                    expected: expected_min_sequence,
                    presented: table.min_sequence,
                });
            }
            flushed_sequence = table.max_sequence;
            expected_min_sequence = flushed_sequence
                .checked_add(1)
                .ok_or(ManagedLsmError::SequenceOverflow)?;
            expected_generation = expected_generation
                .checked_add(1)
                .ok_or(ManagedLsmError::GenerationOverflow)?;
            generations.push(*generation);
        }

        Ok(Self {
            oldest_readable_sequence: 0,
            flushed_sequence,
            active_generations: generations,
        })
    }

    fn encode(&self) -> Result<Vec<u8>, ManagedLsmError> {
        if self.oldest_readable_sequence > self.flushed_sequence {
            return Err(ManagedLsmError::CorruptState(
                "manifest retention floor exceeds flushed sequence".to_string(),
            ));
        }
        validate_generations(&self.active_generations).map_err(ManagedLsmError::CorruptState)?;
        if self.flushed_sequence == 0 && !self.active_generations.is_empty() {
            return Err(ManagedLsmError::CorruptState(
                "empty manifest cannot reference tables".to_string(),
            ));
        }
        if self.flushed_sequence > 0 && self.active_generations.is_empty() {
            return Err(ManagedLsmError::CorruptState(
                "non-empty manifest must reference a table".to_string(),
            ));
        }

        let count = u64::try_from(self.active_generations.len()).map_err(|_| {
            ManagedLsmError::CorruptState("too many active table generations".to_string())
        })?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MANIFEST_MAGIC);
        bytes.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.oldest_readable_sequence.to_le_bytes());
        bytes.extend_from_slice(&self.flushed_sequence.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        for generation in &self.active_generations {
            bytes.extend_from_slice(&generation.to_le_bytes());
        }
        if bytes.len().saturating_add(CHECKSUM_BYTES) > MAX_MANIFEST_BYTES {
            return Err(ManagedLsmError::ManifestTooLarge(bytes.len() + CHECKSUM_BYTES));
        }
        let checksum = blake3::hash(&bytes);
        bytes.extend_from_slice(checksum.as_bytes());
        Ok(bytes)
    }

    fn write_atomic(&self, path: &Path) -> Result<(), ManagedLsmError> {
        let encoded = self.encode()?;
        write_atomic(path, &encoded).map_err(ManagedLsmError::io)
    }

    fn load(path: &Path) -> Result<Self, ManagedLsmError> {
        let bytes = fs::read(path).map_err(ManagedLsmError::io)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(ManagedLsmError::ManifestTooLarge(bytes.len()));
        }
        let fixed = MANIFEST_MAGIC.len() + 2 + (3 * 8) + CHECKSUM_BYTES;
        if bytes.len() < fixed {
            return Err(ManagedLsmError::CorruptManifest {
                path: path.to_path_buf(),
                reason: "manifest is shorter than the fixed header".to_string(),
            });
        }
        let checksum_start = bytes.len() - CHECKSUM_BYTES;
        let (data, stored_checksum) = bytes.split_at(checksum_start);
        if stored_checksum != blake3::hash(data).as_bytes() {
            return Err(ManagedLsmError::ManifestChecksumMismatch {
                path: path.to_path_buf(),
            });
        }

        let mut cursor = 0_usize;
        let magic = take_manifest(data, &mut cursor, MANIFEST_MAGIC.len(), path)?;
        if magic != MANIFEST_MAGIC {
            return Err(ManagedLsmError::CorruptManifest {
                path: path.to_path_buf(),
                reason: "invalid manifest magic".to_string(),
            });
        }
        let version = read_manifest_u16(data, &mut cursor, path)?;
        if version != MANIFEST_VERSION {
            return Err(ManagedLsmError::UnsupportedManifestVersion {
                path: path.to_path_buf(),
                version,
            });
        }
        let oldest_readable_sequence = read_manifest_u64(data, &mut cursor, path)?;
        let flushed_sequence = read_manifest_u64(data, &mut cursor, path)?;
        let count = read_manifest_u64(data, &mut cursor, path)?;
        let count = usize::try_from(count).map_err(|_| ManagedLsmError::CorruptManifest {
            path: path.to_path_buf(),
            reason: "active table count does not fit in memory".to_string(),
        })?;

        let mut active_generations = Vec::with_capacity(count);
        for _ in 0..count {
            active_generations.push(read_manifest_u64(data, &mut cursor, path)?);
        }
        if cursor != data.len() {
            return Err(ManagedLsmError::CorruptManifest {
                path: path.to_path_buf(),
                reason: "manifest contains trailing bytes".to_string(),
            });
        }
        if oldest_readable_sequence > flushed_sequence {
            return Err(ManagedLsmError::CorruptManifest {
                path: path.to_path_buf(),
                reason: "retention floor exceeds flushed sequence".to_string(),
            });
        }
        validate_generations(&active_generations).map_err(|reason| {
            ManagedLsmError::CorruptManifest {
                path: path.to_path_buf(),
                reason,
            }
        })?;
        if flushed_sequence == 0 && !active_generations.is_empty() {
            return Err(ManagedLsmError::CorruptManifest {
                path: path.to_path_buf(),
                reason: "empty manifest references active tables".to_string(),
            });
        }
        if flushed_sequence > 0 && active_generations.is_empty() {
            return Err(ManagedLsmError::CorruptManifest {
                path: path.to_path_buf(),
                reason: "non-empty manifest references no tables".to_string(),
            });
        }

        Ok(Self {
            oldest_readable_sequence,
            flushed_sequence,
            active_generations,
        })
    }
}

#[derive(Debug, Clone)]
struct ImmutableTable {
    generation: u64,
    min_sequence: u64,
    max_sequence: u64,
    version_count: usize,
    rows: BTreeMap<Vec<u8>, Vec<VersionedValue>>,
}

impl ImmutableTable {
    fn from_rows(
        generation: u64,
        min_sequence: u64,
        max_sequence: u64,
        rows: &BTreeMap<Vec<u8>, Vec<VersionedValue>>,
    ) -> Result<Self, ManagedLsmError> {
        if generation == 0 || min_sequence == 0 || min_sequence > max_sequence {
            return Err(ManagedLsmError::CorruptState(
                "immutable table metadata is invalid".to_string(),
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
                    return Err(ManagedLsmError::CorruptState(
                        "table rows contain invalid MVCC sequence history".to_string(),
                    ));
                }
                previous = version.sequence;
                version_count = version_count
                    .checked_add(1)
                    .ok_or(ManagedLsmError::TableTooLarge(usize::MAX))?;
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

    fn write_atomic(&self, path: &Path) -> Result<(), ManagedLsmError> {
        let encoded = self.encode()?;
        write_atomic(path, &encoded).map_err(ManagedLsmError::io)
    }

    fn encode(&self) -> Result<Vec<u8>, ManagedLsmError> {
        let key_count = u64::try_from(self.rows.len())
            .map_err(|_| ManagedLsmError::TableTooLarge(self.rows.len()))?;
        let version_count = u64::try_from(self.version_count)
            .map_err(|_| ManagedLsmError::TableTooLarge(self.version_count))?;
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
                return Err(ManagedLsmError::KeyTooLarge(key.len()));
            }
            let key_len = u32::try_from(key.len())
                .map_err(|_| ManagedLsmError::KeyTooLarge(key.len()))?;
            let versions_len = u32::try_from(versions.len())
                .map_err(|_| ManagedLsmError::TableTooLarge(versions.len()))?;
            bytes.extend_from_slice(&key_len.to_le_bytes());
            bytes.extend_from_slice(key);
            bytes.extend_from_slice(&versions_len.to_le_bytes());
            for version in versions {
                bytes.extend_from_slice(&version.sequence.to_le_bytes());
                match &version.value {
                    None => bytes.push(0),
                    Some(value) => {
                        if value.len() > MAX_VALUE_BYTES {
                            return Err(ManagedLsmError::ValueTooLarge(value.len()));
                        }
                        let value_len = u32::try_from(value.len())
                            .map_err(|_| ManagedLsmError::ValueTooLarge(value.len()))?;
                        bytes.push(1);
                        bytes.extend_from_slice(&value_len.to_le_bytes());
                        bytes.extend_from_slice(value);
                    }
                }
            }
        }

        if bytes.len().saturating_add(CHECKSUM_BYTES) > MAX_TABLE_BYTES {
            return Err(ManagedLsmError::TableTooLarge(bytes.len() + CHECKSUM_BYTES));
        }
        let checksum = blake3::hash(&bytes);
        bytes.extend_from_slice(checksum.as_bytes());
        Ok(bytes)
    }

    fn load(path: &Path) -> Result<Self, ManagedLsmError> {
        let bytes = fs::read(path).map_err(ManagedLsmError::io)?;
        if bytes.len() > MAX_TABLE_BYTES {
            return Err(ManagedLsmError::TableTooLarge(bytes.len()));
        }
        if bytes.len() < SST_MAGIC.len() + 2 + (5 * 8) + CHECKSUM_BYTES {
            return Err(ManagedLsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "table is shorter than the fixed header".to_string(),
            });
        }
        let checksum_start = bytes.len() - CHECKSUM_BYTES;
        let (data, stored_checksum) = bytes.split_at(checksum_start);
        if stored_checksum != blake3::hash(data).as_bytes() {
            return Err(ManagedLsmError::ChecksumMismatch {
                path: path.to_path_buf(),
            });
        }

        let mut cursor = 0_usize;
        if take_table(data, &mut cursor, SST_MAGIC.len(), path)? != SST_MAGIC {
            return Err(ManagedLsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "invalid immutable-table magic".to_string(),
            });
        }
        let version = read_table_u16(data, &mut cursor, path)?;
        if version != SST_VERSION {
            return Err(ManagedLsmError::UnsupportedVersion {
                path: path.to_path_buf(),
                version,
            });
        }
        let generation = read_table_u64(data, &mut cursor, path)?;
        let min_sequence = read_table_u64(data, &mut cursor, path)?;
        let max_sequence = read_table_u64(data, &mut cursor, path)?;
        let key_count = read_table_u64(data, &mut cursor, path)?;
        let declared_version_count = read_table_u64(data, &mut cursor, path)?;
        if generation == 0 || min_sequence == 0 || min_sequence > max_sequence {
            return Err(ManagedLsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "invalid immutable-table metadata".to_string(),
            });
        }

        let key_count = usize::try_from(key_count).map_err(|_| ManagedLsmError::CorruptTable {
            path: path.to_path_buf(),
            reason: "key count does not fit in memory".to_string(),
        })?;
        let declared_version_count = usize::try_from(declared_version_count).map_err(|_| {
            ManagedLsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "version count does not fit in memory".to_string(),
            }
        })?;
        let mut rows = BTreeMap::new();
        let mut actual_version_count = 0_usize;

        for _ in 0..key_count {
            let key_len = read_table_u32(data, &mut cursor, path)? as usize;
            if key_len > MAX_KEY_BYTES {
                return Err(ManagedLsmError::CorruptTable {
                    path: path.to_path_buf(),
                    reason: "key exceeds immutable-table limit".to_string(),
                });
            }
            let key = take_table(data, &mut cursor, key_len, path)?.to_vec();
            let version_count = read_table_u32(data, &mut cursor, path)? as usize;
            if version_count == 0 {
                return Err(ManagedLsmError::CorruptTable {
                    path: path.to_path_buf(),
                    reason: "row contains no versions".to_string(),
                });
            }
            let mut versions = Vec::with_capacity(version_count);
            let mut previous = 0_u64;
            for _ in 0..version_count {
                let sequence = read_table_u64(data, &mut cursor, path)?;
                if sequence < min_sequence || sequence > max_sequence || sequence <= previous {
                    return Err(ManagedLsmError::CorruptTable {
                        path: path.to_path_buf(),
                        reason: "row contains invalid MVCC sequence history".to_string(),
                    });
                }
                previous = sequence;
                let tag = take_table(data, &mut cursor, 1, path)?[0];
                let value = match tag {
                    0 => None,
                    1 => {
                        let value_len = read_table_u32(data, &mut cursor, path)? as usize;
                        if value_len > MAX_VALUE_BYTES {
                            return Err(ManagedLsmError::CorruptTable {
                                path: path.to_path_buf(),
                                reason: "value exceeds immutable-table limit".to_string(),
                            });
                        }
                        Some(take_table(data, &mut cursor, value_len, path)?.to_vec())
                    }
                    _ => {
                        return Err(ManagedLsmError::CorruptTable {
                            path: path.to_path_buf(),
                            reason: "invalid value tag".to_string(),
                        })
                    }
                };
                versions.push(VersionedValue { sequence, value });
                actual_version_count = actual_version_count
                    .checked_add(1)
                    .ok_or(ManagedLsmError::TableTooLarge(usize::MAX))?;
            }
            if rows.insert(key, versions).is_some() {
                return Err(ManagedLsmError::CorruptTable {
                    path: path.to_path_buf(),
                    reason: "duplicate key in immutable table".to_string(),
                });
            }
        }
        if cursor != data.len() || actual_version_count != declared_version_count {
            return Err(ManagedLsmError::CorruptTable {
                path: path.to_path_buf(),
                reason: "immutable-table payload/count mismatch".to_string(),
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

fn collect_rows(
    rows: &mut BTreeMap<Vec<u8>, Vec<VersionedValue>>,
    safe_point: u64,
) -> (usize, usize) {
    let mut versions_removed = 0_usize;
    let mut keys_to_remove = Vec::new();
    for (key, versions) in rows.iter_mut() {
        let first_newer = versions.partition_point(|version| version.sequence <= safe_point);
        let Some(anchor_index) = first_newer.checked_sub(1) else {
            continue;
        };
        let anchor_is_tombstone = versions[anchor_index].value.is_none();
        let has_newer_versions = first_newer < versions.len();
        if anchor_is_tombstone && !has_newer_versions {
            versions_removed += versions.len();
            keys_to_remove.push(key.clone());
            continue;
        }
        if anchor_index > 0 {
            versions_removed += anchor_index;
            versions.drain(..anchor_index);
        }
    }
    let keys_removed = keys_to_remove.len();
    for key in keys_to_remove {
        rows.remove(&key);
    }
    (versions_removed, keys_removed)
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

fn discover_legacy_tables(directory: &Path) -> Result<Vec<(u64, PathBuf)>, ManagedLsmError> {
    let mut discovered = Vec::new();
    for entry in fs::read_dir(directory).map_err(ManagedLsmError::io)? {
        let entry = entry.map_err(ManagedLsmError::io)?;
        if !entry.file_type().map_err(ManagedLsmError::io)?.is_file() {
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

fn validate_generations(generations: &[u64]) -> Result<(), String> {
    let mut previous = 0_u64;
    for generation in generations {
        if *generation == 0 || *generation <= previous {
            return Err("active table generations must be non-zero and strictly increasing".into());
        }
        previous = *generation;
    }
    Ok(())
}

fn table_path(directory: &Path, generation: u64) -> PathBuf {
    directory.join(format!("{TABLE_PREFIX}{generation:020}{TABLE_SUFFIX}"))
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

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
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

fn take_table<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    len: usize,
    path: &Path,
) -> Result<&'a [u8], ManagedLsmError> {
    take_checked(bytes, cursor, len).ok_or_else(|| ManagedLsmError::CorruptTable {
        path: path.to_path_buf(),
        reason: "immutable table ended unexpectedly".to_string(),
    })
}

fn read_table_u16(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u16, ManagedLsmError> {
    let raw: [u8; 2] = take_table(bytes, cursor, 2, path)?.try_into().expect("fixed width");
    Ok(u16::from_le_bytes(raw))
}

fn read_table_u32(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u32, ManagedLsmError> {
    let raw: [u8; 4] = take_table(bytes, cursor, 4, path)?.try_into().expect("fixed width");
    Ok(u32::from_le_bytes(raw))
}

fn read_table_u64(bytes: &[u8], cursor: &mut usize, path: &Path) -> Result<u64, ManagedLsmError> {
    let raw: [u8; 8] = take_table(bytes, cursor, 8, path)?.try_into().expect("fixed width");
    Ok(u64::from_le_bytes(raw))
}

fn take_manifest<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    len: usize,
    path: &Path,
) -> Result<&'a [u8], ManagedLsmError> {
    take_checked(bytes, cursor, len).ok_or_else(|| ManagedLsmError::CorruptManifest {
        path: path.to_path_buf(),
        reason: "manifest ended unexpectedly".to_string(),
    })
}

fn read_manifest_u16(
    bytes: &[u8],
    cursor: &mut usize,
    path: &Path,
) -> Result<u16, ManagedLsmError> {
    let raw: [u8; 2] = take_manifest(bytes, cursor, 2, path)?.try_into().expect("fixed width");
    Ok(u16::from_le_bytes(raw))
}

fn read_manifest_u64(
    bytes: &[u8],
    cursor: &mut usize,
    path: &Path,
) -> Result<u64, ManagedLsmError> {
    let raw: [u8; 8] = take_manifest(bytes, cursor, 8, path)?.try_into().expect("fixed width");
    Ok(u64::from_le_bytes(raw))
}

fn take_checked<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = cursor.checked_add(len)?;
    let slice = bytes.get(*cursor..end)?;
    *cursor = end;
    Some(slice)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedLsmError {
    Io { kind: io::ErrorKind, message: String },
    ChecksumMismatch { path: PathBuf },
    CorruptTable { path: PathBuf, reason: String },
    UnsupportedVersion { path: PathBuf, version: u16 },
    ManifestChecksumMismatch { path: PathBuf },
    CorruptManifest { path: PathBuf, reason: String },
    UnsupportedManifestVersion { path: PathBuf, version: u16 },
    GenerationGap { expected: u64, presented: u64 },
    SequenceGap { expected: u64, presented: u64 },
    SequenceOverflow,
    GenerationOverflow,
    SafePointAhead { current_sequence: u64, safe_point: u64 },
    UnflushedStateForCompaction { current_sequence: u64, flushed_sequence: u64 },
    KeyTooLarge(usize),
    ValueTooLarge(usize),
    TableTooLarge(usize),
    ManifestTooLarge(usize),
    CorruptState(String),
}

impl ManagedLsmError {
    fn io(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for ManagedLsmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message, .. } => write!(f, "managed LSM I/O error: {message}"),
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
            Self::ManifestChecksumMismatch { path } => {
                write!(f, "manifest checksum mismatch: {}", path.display())
            }
            Self::CorruptManifest { path, reason } => {
                write!(f, "corrupt manifest {}: {reason}", path.display())
            }
            Self::UnsupportedManifestVersion { path, version } => write!(
                f,
                "unsupported manifest version {version}: {}",
                path.display()
            ),
            Self::GenerationGap { expected, presented } => write!(
                f,
                "immutable-table generation gap: expected {expected}, found {presented}"
            ),
            Self::SequenceGap { expected, presented } => write!(
                f,
                "immutable-table sequence gap: expected {expected}, found {presented}"
            ),
            Self::SequenceOverflow => f.write_str("managed LSM sequence overflow"),
            Self::GenerationOverflow => f.write_str("managed LSM generation overflow"),
            Self::SafePointAhead { current_sequence, safe_point } => write!(
                f,
                "compaction safe point {safe_point} is ahead of committed sequence {current_sequence}"
            ),
            Self::UnflushedStateForCompaction {
                current_sequence,
                flushed_sequence,
            } => write!(
                f,
                "cannot compact with unflushed commits: committed {current_sequence}, flushed {flushed_sequence}"
            ),
            Self::KeyTooLarge(size) => write!(f, "immutable-table key is too large ({size} bytes)"),
            Self::ValueTooLarge(size) => write!(f, "immutable-table value is too large ({size} bytes)"),
            Self::TableTooLarge(size) => write!(f, "immutable table is too large ({size} bytes)"),
            Self::ManifestTooLarge(size) => write!(f, "manifest is too large ({size} bytes)"),
            Self::CorruptState(reason) => write!(f, "invalid managed LSM state: {reason}"),
        }
    }
}

impl std::error::Error for ManagedLsmError {}
