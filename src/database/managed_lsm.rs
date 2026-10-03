//! Manifest-backed LSM storage with crash-safe whole-table-set compaction.
//!
//! Commit publication remains memory-only and infallible. Flush and compaction
//! are separate fallible maintenance operations. Immutable tables are written
//! and synced before a checksummed manifest atomically makes them authoritative;
//! obsolete tables are reclaimed only after that manifest swap succeeds.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::sstable::{
    atomic_write, discover_tables, sync_parent_directory, table_path, visible_version, Sstable,
    SstableError,
};
use super::tablet::{MvccStorage, TabletMutation, VersionedValue};

const MANIFEST_MAGIC: &[u8; 8] = b"NUDBMAN1";
const MANIFEST_VERSION: u16 = 1;
const CHECKSUM_BYTES: usize = 32;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MANIFEST_FILE: &str = "MANIFEST";

#[derive(Debug)]
pub struct ManagedLsmStorage {
    directory: PathBuf,
    current_sequence: u64,
    flushed_sequence: u64,
    oldest_readable_sequence: u64,
    next_generation: u64,
    mutable: BTreeMap<Vec<u8>, Vec<VersionedValue>>,
    tables: Vec<Sstable>,
}

impl ManagedLsmStorage {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, ManagedLsmError> {
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory).map_err(ManagedLsmError::io)?;
        let manifest_path = directory.join(MANIFEST_FILE);
        let discovered = discover_tables(&directory).map_err(ManagedLsmError::from)?;

        let manifest = if manifest_path.exists() {
            Manifest::load(&manifest_path)?
        } else {
            let manifest = Manifest::from_legacy_tables(&discovered)?;
            manifest.write_atomic(&manifest_path)?;
            manifest
        };

        let mut tables = Vec::with_capacity(manifest.active_generations.len());
        let mut expected_min_sequence = 1_u64;
        let mut current_sequence = 0_u64;

        for generation in &manifest.active_generations {
            let path = table_path(&directory, *generation);
            let table = Sstable::load(&path).map_err(ManagedLsmError::from)?;
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

        // Generations are never reused, including generations belonging to an
        // SSTable that was durably written but never published in MANIFEST.
        let max_disk_generation = discovered
            .last()
            .map(|(generation, _)| *generation)
            .unwrap_or(0);
        let max_active_generation = manifest.active_generations.last().copied().unwrap_or(0);
        let next_generation = max_disk_generation
            .max(max_active_generation)
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

    /// Publish all committed state since the previous flush.
    ///
    /// The immutable table is durable before the manifest references it. If the
    /// manifest update fails, the table is merely an orphan and the mutable
    /// state remains intact for a retry.
    pub fn flush(&mut self) -> Result<Option<ManagedFlushResult>, ManagedLsmError> {
        if self.current_sequence == self.flushed_sequence {
            return Ok(None);
        }

        let min_sequence = self
            .flushed_sequence
            .checked_add(1)
            .ok_or(ManagedLsmError::SequenceOverflow)?;
        let max_sequence = self.current_sequence;
        let generation = self.fresh_generation()?;
        let following_generation = generation
            .checked_add(1)
            .ok_or(ManagedLsmError::GenerationOverflow)?;
        let table = Sstable::from_rows(generation, min_sequence, max_sequence, &self.mutable)
            .map_err(ManagedLsmError::from)?;
        let version_count = table.version_count;
        let path = table_path(&self.directory, generation);
        table.write_atomic(&path).map_err(ManagedLsmError::from)?;

        let mut generations: Vec<u64> = self
            .tables
            .iter()
            .map(|active| active.generation)
            .collect();
        generations.push(generation);
        Manifest {
            oldest_readable_sequence: self.oldest_readable_sequence,
            flushed_sequence: max_sequence,
            active_generations: generations,
        }
        .write_atomic(&self.directory.join(MANIFEST_FILE))?;

        self.tables.push(table);
        self.mutable.clear();
        self.flushed_sequence = max_sequence;
        self.next_generation = following_generation;

        Ok(Some(ManagedFlushResult {
            generation,
            path,
            min_sequence,
            max_sequence,
            version_count,
        }))
    }

    /// Merge every active immutable table and atomically replace the active set.
    ///
    /// Compaction is forbidden while committed state remains in the mutable
    /// memtable. Once the replacement manifest is published, cleanup failures
    /// are reported in `CompactionResult` rather than returned as an operation
    /// error, because the new table set is already authoritative at that point.
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
            Manifest {
                oldest_readable_sequence: 0,
                flushed_sequence: 0,
                active_generations: Vec::new(),
            }
            .write_atomic(&self.directory.join(MANIFEST_FILE))?;
            return Ok(CompactionResult {
                input_tables,
                output_tables: 0,
                safe_point: 0,
                versions_removed: 0,
                keys_removed: 0,
                obsolete_files_retained: 0,
                cleanup_sync_failed: false,
            });
        }

        let generation = self.fresh_generation()?;
        let following_generation = generation
            .checked_add(1)
            .ok_or(ManagedLsmError::GenerationOverflow)?;
        let mut rows: BTreeMap<Vec<u8>, Vec<VersionedValue>> = BTreeMap::new();
        for table in &self.tables {
            for row in table.rows() {
                rows.entry(row.key.clone())
                    .or_default()
                    .extend(row.versions.iter().cloned());
            }
        }

        let (versions_removed, keys_removed) = collect_rows(&mut rows, target_safe_point);
        let replacement = Sstable::from_rows(generation, 1, self.current_sequence, &rows)
            .map_err(ManagedLsmError::from)?;
        let replacement_path = table_path(&self.directory, generation);
        replacement
            .write_atomic(&replacement_path)
            .map_err(ManagedLsmError::from)?;

        Manifest {
            oldest_readable_sequence: target_safe_point,
            flushed_sequence: self.current_sequence,
            active_generations: vec![generation],
        }
        .write_atomic(&self.directory.join(MANIFEST_FILE))?;

        // Everything below this point is deliberately infallible with respect
        // to the caller-visible compaction result: the manifest has committed.
        self.tables = vec![replacement];
        self.oldest_readable_sequence = target_safe_point;
        self.next_generation = following_generation;

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
        let cleanup_sync_failed = sync_parent_directory(&replacement_path).is_err();

        Ok(CompactionResult {
            input_tables,
            output_tables: 1,
            safe_point: target_safe_point,
            versions_removed,
            keys_removed,
            obsolete_files_retained,
            cleanup_sync_failed,
        })
    }

    /// Read a stable, ordered key range at one retained MVCC snapshot.
    ///
    /// `start` is inclusive and `end_exclusive` is optional. Immutable tables
    /// use their sparse block index to enter the range; newer visible versions
    /// from later tables or the mutable memtable replace older candidates.
    pub fn scan_at(
        &self,
        start: &[u8],
        end_exclusive: Option<&[u8]>,
        snapshot: u64,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ManagedLsmError> {
        self.validate_snapshot(snapshot)?;
        if limit == 0 || end_exclusive.map(|end| end <= start).unwrap_or(false) {
            return Ok(Vec::new());
        }

        let mut merged: BTreeMap<Vec<u8>, (u64, Option<Vec<u8>>)> = BTreeMap::new();
        for table in &self.tables {
            if table.min_sequence > snapshot {
                break;
            }
            for row in table.range_rows(start, end_exclusive) {
                if let Some(version) = visible_version(&row.versions, snapshot) {
                    insert_newest(&mut merged, &row.key, version);
                }
            }
        }

        for (key, versions) in &self.mutable {
            if key.as_slice() < start
                || end_exclusive
                    .map(|end| key.as_slice() >= end)
                    .unwrap_or(false)
            {
                continue;
            }
            if let Some(version) = visible_version(versions, snapshot) {
                insert_newest(&mut merged, key, version);
            }
        }

        Ok(merged
            .into_iter()
            .filter_map(|(key, (_, value))| value.map(|value| (key, value)))
            .take(limit)
            .collect())
    }

    fn validate_snapshot(&self, snapshot: u64) -> Result<(), ManagedLsmError> {
        if snapshot > self.current_sequence {
            return Err(ManagedLsmError::SnapshotAhead {
                committed: self.current_sequence,
                requested: snapshot,
            });
        }
        if snapshot < self.oldest_readable_sequence {
            return Err(ManagedLsmError::SnapshotCollected {
                oldest_readable: self.oldest_readable_sequence,
                requested: snapshot,
            });
        }
        Ok(())
    }

    fn fresh_generation(&self) -> Result<u64, ManagedLsmError> {
        let mut generation = self.next_generation;
        loop {
            if !table_path(&self.directory, generation).exists() {
                return Ok(generation);
            }
            generation = generation
                .checked_add(1)
                .ok_or(ManagedLsmError::GenerationOverflow)?;
        }
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

fn insert_newest(
    merged: &mut BTreeMap<Vec<u8>, (u64, Option<Vec<u8>>)>,
    key: &[u8],
    version: &VersionedValue,
) {
    let replace = merged
        .get(key)
        .map(|(sequence, _)| version.sequence > *sequence)
        .unwrap_or(true);
    if replace {
        merged.insert(key.to_vec(), (version.sequence, version.value.clone()));
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
    pub cleanup_sync_failed: bool,
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
            let table = Sstable::load(path).map_err(ManagedLsmError::from)?;
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
            return Err(ManagedLsmError::ManifestTooLarge(
                bytes.len() + CHECKSUM_BYTES,
            ));
        }
        let checksum = blake3::hash(&bytes);
        bytes.extend_from_slice(checksum.as_bytes());
        Ok(bytes)
    }

    fn write_atomic(&self, path: &Path) -> Result<(), ManagedLsmError> {
        let encoded = self.encode()?;
        atomic_write(path, &encoded).map_err(ManagedLsmError::io)
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
        if take_manifest(data, &mut cursor, MANIFEST_MAGIC.len(), path)? != MANIFEST_MAGIC {
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
        if count > data.len() / 8 {
            return Err(ManagedLsmError::CorruptManifest {
                path: path.to_path_buf(),
                reason: "active table count exceeds manifest payload".to_string(),
            });
        }

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

fn validate_generations(generations: &[u64]) -> Result<(), String> {
    let mut previous = 0_u64;
    for generation in generations {
        if *generation == 0 || *generation <= previous {
            return Err(
                "active table generations must be non-zero and strictly increasing".to_string(),
            );
        }
        previous = *generation;
    }
    Ok(())
}

fn take_manifest<'a>(
    bytes: &'a [u8],
    cursor: &mut usize,
    len: usize,
    path: &Path,
) -> Result<&'a [u8], ManagedLsmError> {
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| ManagedLsmError::CorruptManifest {
            path: path.to_path_buf(),
            reason: "manifest offset overflow".to_string(),
        })?;
    let slice = bytes
        .get(*cursor..end)
        .ok_or_else(|| ManagedLsmError::CorruptManifest {
            path: path.to_path_buf(),
            reason: "manifest ended unexpectedly".to_string(),
        })?;
    *cursor = end;
    Ok(slice)
}

fn read_manifest_u16(
    bytes: &[u8],
    cursor: &mut usize,
    path: &Path,
) -> Result<u16, ManagedLsmError> {
    let raw: [u8; 2] = take_manifest(bytes, cursor, 2, path)?
        .try_into()
        .expect("fixed-width read");
    Ok(u16::from_le_bytes(raw))
}

fn read_manifest_u64(
    bytes: &[u8],
    cursor: &mut usize,
    path: &Path,
) -> Result<u64, ManagedLsmError> {
    let raw: [u8; 8] = take_manifest(bytes, cursor, 8, path)?
        .try_into()
        .expect("fixed-width read");
    Ok(u64::from_le_bytes(raw))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedLsmError {
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
    ManifestChecksumMismatch {
        path: PathBuf,
    },
    CorruptManifest {
        path: PathBuf,
        reason: String,
    },
    UnsupportedManifestVersion {
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
    SafePointAhead {
        current_sequence: u64,
        safe_point: u64,
    },
    SnapshotAhead {
        committed: u64,
        requested: u64,
    },
    SnapshotCollected {
        oldest_readable: u64,
        requested: u64,
    },
    UnflushedStateForCompaction {
        current_sequence: u64,
        flushed_sequence: u64,
    },
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

impl From<SstableError> for ManagedLsmError {
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
            Self::SequenceOverflow => f.write_str("managed LSM sequence overflow"),
            Self::GenerationOverflow => f.write_str("managed LSM generation overflow"),
            Self::SafePointAhead {
                current_sequence,
                safe_point,
            } => write!(
                f,
                "compaction safe point {safe_point} is ahead of committed sequence {current_sequence}"
            ),
            Self::SnapshotAhead {
                committed,
                requested,
            } => write!(
                f,
                "snapshot {requested} is ahead of committed sequence {committed}"
            ),
            Self::SnapshotCollected {
                oldest_readable,
                requested,
            } => write!(
                f,
                "snapshot {requested} is older than retained sequence {oldest_readable}"
            ),
            Self::UnflushedStateForCompaction {
                current_sequence,
                flushed_sequence,
            } => write!(
                f,
                "cannot compact with unflushed commits: committed {current_sequence}, flushed {flushed_sequence}"
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
            Self::ManifestTooLarge(size) => {
                write!(f, "manifest is too large ({size} bytes)")
            }
            Self::CorruptState(reason) => write!(f, "invalid managed LSM state: {reason}"),
        }
    }
}

impl std::error::Error for ManagedLsmError {}
