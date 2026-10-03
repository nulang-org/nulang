//! WAL-backed single-node tablet coordinator.
//!
//! This type enforces the first NuDB durability ordering contract:
//!
//! ```text
//! validate write
//!     -> append + sync WAL
//!     -> publish MVCC state
//! ```
//!
//! No fallible validation is performed after the WAL acknowledges durability.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::checkpoint::{self, CheckpointError};
use super::manifest::{Manifest, ManifestEntry, ManifestError};
use super::sstable::{
    self, Sstable, SstableBlockCache, SstableCacheStats, SstableError,
    DEFAULT_BLOCK_CACHE_BYTES,
};
use super::tablet::{
    MemoryTablet, TabletDescriptor, TabletError, TabletMutation, TabletSnapshotRow,
    TabletSnapshotState, TabletWrite, VersionedValue,
};
use super::wal::{FileWal, WalError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabletSstableCacheStats {
    pub max_bytes: usize,
    pub resident_bytes: usize,
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
}

impl From<SstableCacheStats> for TabletSstableCacheStats {
    fn from(stats: SstableCacheStats) -> Self {
        Self {
            max_bytes: stats.max_bytes,
            resident_bytes: stats.resident_bytes,
            entries: stats.entries,
            hits: stats.hits,
            misses: stats.misses,
        }
    }
}

#[derive(Debug)]
pub struct WalBackedTablet {
    tablet: MemoryTablet,
    wal: FileWal,
    checkpoint_path: PathBuf,
    manifest_path: PathBuf,
    sstable_dir: PathBuf,
    /// Manifest-validated immutable generations served directly without
    /// rehydrating duplicate memtables. Ordered oldest to newest.
    sstables: Vec<Sstable>,
    /// One byte-bounded cache shared by every SSTable owned by this tablet.
    sstable_cache: Arc<Mutex<SstableBlockCache>>,
}

impl WalBackedTablet {
    pub fn open(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
    ) -> Result<Self, WalBackedError> {
        Self::open_with_sstable_cache_bytes(descriptor, wal_path, DEFAULT_BLOCK_CACHE_BYTES)
    }

    /// Open a tablet with an explicit per-tablet SSTable serving-cache budget.
    ///
    /// The budget counts exact encoded block bytes retained by the cache. A
    /// zero-byte budget disables caching while preserving block-backed reads.
    pub fn open_with_sstable_cache_bytes(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
        sstable_cache_bytes: usize,
    ) -> Result<Self, WalBackedError> {
        let wal_path = wal_path.as_ref();
        let checkpoint_path = checkpoint::checkpoint_path_for_wal(wal_path);
        let manifest_path = wal_path.with_extension("manifest");
        let sstable_dir = wal_path.with_extension("sstables");
        let wal = FileWal::open(wal_path)?;
        let mut tablet = checkpoint::load_checkpoint(&checkpoint_path, descriptor.clone())?
            .unwrap_or_else(|| MemoryTablet::new(descriptor.clone()));
        let checkpoint_floor = tablet.current_sequence();
        let sstable_cache = Arc::new(Mutex::new(SstableBlockCache::new(sstable_cache_bytes)));

        let manifest = Manifest::load_or_empty(&manifest_path, descriptor.id().get())?;
        let mut sstables = Vec::new();
        for entry in manifest.entries() {
            let path = sstable_dir.join(&entry.file_name);
            let table = sstable::Sstable::open_with_cache(&path, Arc::clone(&sstable_cache))?;
            let metadata = table.metadata();
            if metadata.tablet_id != entry.tablet_id
                || metadata.ownership_epoch != entry.ownership_epoch
                || metadata.min_sequence != entry.min_sequence
                || metadata.max_sequence != entry.max_sequence
                || metadata.row_count != entry.row_count
                || metadata.min_key != entry.min_key
                || metadata.max_key != entry.max_key
                || metadata.checksum != entry.checksum
                || metadata.file_name != entry.file_name
            {
                return Err(WalBackedError::ManifestSstableMismatch(
                    entry.file_name.clone(),
                ));
            }
            if metadata.tablet_id != descriptor.id().get()
                || metadata.ownership_epoch > descriptor.ownership_epoch()
                || !descriptor.range().contains(&metadata.min_key)
                || !descriptor.range().contains(&metadata.max_key)
            {
                return Err(WalBackedError::ManifestSstableMismatch(
                    entry.file_name.clone(),
                ));
            }

            // A checkpoint already contains all history through its sequence.
            // Older tables remain durable artifacts but do not need to stay in
            // the serving set for this process.
            if metadata.max_sequence <= checkpoint_floor {
                continue;
            }

            if metadata.max_sequence > tablet.current_sequence() {
                if !table.has_contiguous_sequence_coverage_after(tablet.current_sequence()) {
                    continue;
                }
                tablet.advance_recovered_sequence(metadata.max_sequence);
            }
            sstables.push(table);
        }
        wal.replay_after_checkpoint(&mut tablet)?;
        Ok(Self {
            tablet,
            wal,
            checkpoint_path,
            manifest_path,
            sstable_dir,
            sstables,
            sstable_cache,
        })
    }

    pub fn descriptor(&self) -> &TabletDescriptor {
        self.tablet.descriptor()
    }

    pub fn current_sequence(&self) -> u64 {
        self.tablet.current_sequence()
    }

    /// Estimated logical bytes in the active mutable memtable.
    pub fn mutable_memtable_bytes(&self) -> usize {
        self.tablet.mutable_memtable_bytes()
    }

    /// Number of frozen in-memory generations that have not yet been retired
    /// behind the durable SSTable serving tier.
    pub fn immutable_memtable_count(&self) -> usize {
        self.tablet.immutable_memtable_count()
    }

    pub fn sstable_cache_stats(&self) -> Result<TabletSstableCacheStats, WalBackedError> {
        let stats = self
            .sstable_cache
            .lock()
            .map_err(|_| SstableError::CachePoisoned)?
            .stats();
        Ok(stats.into())
    }

    /// Freeze the active mutable generation when it reaches the supplied byte target.
    pub fn rotate_memtable_if_bytes_at_least(&mut self, min_bytes: usize) -> bool {
        self.tablet.rotate_memtable_if_bytes_at_least(min_bytes)
    }

    /// Durably flush the oldest frozen memtable to an immutable SSTable, publish
    /// its manifest entry, install it in the serving tier, and only then retire
    /// the duplicate in-memory generation. WAL reclamation remains unchanged.
    pub fn flush_oldest_immutable_to_sstable(&mut self) -> Result<bool, WalBackedError> {
        let Some(rows) = self.tablet.oldest_immutable_rows() else {
            return Ok(false);
        };
        let min_sequence = rows
            .iter()
            .flat_map(|row| row.versions.iter())
            .map(|version| version.sequence)
            .min()
            .ok_or(WalBackedError::NoImmutableMemtable)?;
        let max_sequence = rows
            .iter()
            .flat_map(|row| row.versions.iter())
            .map(|version| version.sequence)
            .max()
            .ok_or(WalBackedError::NoImmutableMemtable)?;
        let identity = serde_json::to_vec(&rows)
            .map_err(|error| WalBackedError::FlushIdentity(error.to_string()))?;
        let digest = blake3::hash(&identity).to_hex();
        let file_name = format!(
            "tablet-{}-{}-{}-{}.sst",
            self.tablet.descriptor().id().get(),
            min_sequence,
            max_sequence,
            &digest.as_str()[..16]
        );

        let mut manifest =
            Manifest::load_or_empty(&self.manifest_path, self.tablet.descriptor().id().get())?;
        let path = self.sstable_dir.join(&file_name);
        let expected = sstable::expected_metadata(
            file_name.clone(),
            self.tablet.descriptor().id().get(),
            self.tablet.descriptor().ownership_epoch(),
            &rows,
        )?;

        let table = if path.exists() {
            let existing =
                sstable::Sstable::open_with_cache(&path, Arc::clone(&self.sstable_cache))?;
            if existing.metadata() != &expected {
                return Err(WalBackedError::Sstable(SstableError::ExistingFileMismatch(
                    file_name.clone(),
                )));
            }
            existing
        } else {
            sstable::write_sstable(
                &path,
                self.tablet.descriptor().id().get(),
                self.tablet.descriptor().ownership_epoch(),
                &rows,
            )?;
            let written =
                sstable::Sstable::open_with_cache(&path, Arc::clone(&self.sstable_cache))?;
            if written.metadata() != &expected {
                return Err(WalBackedError::Sstable(SstableError::ExistingFileMismatch(
                    file_name.clone(),
                )));
            }
            written
        };

        let metadata = table.metadata().clone();
        let entry = ManifestEntry {
            file_name: metadata.file_name.clone(),
            tablet_id: metadata.tablet_id,
            ownership_epoch: metadata.ownership_epoch,
            min_sequence: metadata.min_sequence,
            max_sequence: metadata.max_sequence,
            row_count: metadata.row_count,
            min_key: metadata.min_key.clone(),
            max_key: metadata.max_key.clone(),
            checksum: metadata.checksum,
        };
        let inserted = manifest.register(entry)?;
        if inserted {
            manifest.publish(&self.manifest_path)?;
        }

        // From here onward no operation can fail: the SSTable and manifest are
        // already durable, so installing the serving handle must precede memory
        // retirement without introducing an ambiguous post-commit error.
        if !self
            .sstables
            .iter()
            .any(|existing| existing.metadata().file_name == metadata.file_name)
        {
            self.sstables.push(table);
            self.sstables.sort_by(|a, b| {
                (
                    a.metadata().min_sequence,
                    a.metadata().max_sequence,
                    &a.metadata().file_name,
                )
                    .cmp(&(
                        b.metadata().min_sequence,
                        b.metadata().max_sequence,
                        &b.metadata().file_name,
                    ))
            });
        }
        let evicted = self.tablet.evict_oldest_immutable();
        debug_assert!(evicted, "oldest immutable disappeared after durable flush");
        Ok(inserted)
    }

    pub fn durable_sstable_count(&self) -> Result<usize, WalBackedError> {
        let manifest =
            Manifest::load_or_empty(&self.manifest_path, self.tablet.descriptor().id().get())?;
        Ok(manifest.entries().len())
    }

    pub fn prepare_write(
        &self,
        presented_epoch: u64,
        expected_previous_sequence: u64,
        mutations: Vec<TabletMutation>,
    ) -> Result<TabletWrite, TabletError> {
        self.tablet
            .prepare_write(presented_epoch, expected_previous_sequence, mutations)
    }

    /// Durably commit one write before making it visible to readers.
    pub fn commit(&mut self, write: TabletWrite) -> Result<u64, WalBackedError> {
        self.tablet.validate_write(&write)?;
        self.wal.append_write(&write)?;
        Ok(self.tablet.publish_validated(write))
    }

    /// Durably commit a consecutive group of independent writes with one WAL
    /// synchronization boundary, then publish them to MVCC state in order.
    ///
    /// The complete group is validated before any WAL bytes are emitted. Once
    /// the WAL acknowledges durability, publication is infallible. An I/O
    /// error during the append can leave an ambiguous durable prefix, so the
    /// poisoned WAL contract requires reopen/recovery before retrying.
    pub fn commit_batch(&mut self, writes: Vec<TabletWrite>) -> Result<u64, WalBackedError> {
        if writes.is_empty() {
            return Ok(self.tablet.current_sequence());
        }

        let mut projected_sequence = self.tablet.current_sequence();
        for write in &writes {
            self.tablet
                .validate_write_at_sequence(write, projected_sequence)?;
            projected_sequence = write.sequence();
        }

        self.wal.append_batch(&writes)?;
        for write in writes {
            self.tablet.publish_validated(write);
        }
        Ok(projected_sequence)
    }

    /// Read one key at a committed snapshot across resident and block-backed
    /// immutable tiers. SSTable I/O/checksum failures are surfaced explicitly.
    pub fn read_at<'a>(
        &'a self,
        key: &[u8],
        snapshot: u64,
    ) -> Result<Option<Cow<'a, [u8]>>, WalBackedError> {
        let memory = self.tablet.version_at(key, snapshot)?;
        let mut best_sequence = memory.map(|version| version.sequence);
        let mut disk_winner: Option<VersionedValue> = None;

        for table in self.sstables.iter().rev() {
            if best_sequence.is_some_and(|sequence| table.metadata().max_sequence < sequence) {
                continue;
            }
            if let Some(candidate) = table.version_at(key, snapshot)? {
                if best_sequence
                    .map(|sequence| candidate.sequence > sequence)
                    .unwrap_or(true)
                {
                    best_sequence = Some(candidate.sequence);
                    disk_winner = Some(candidate);
                }
            }
        }

        if let Some(version) = disk_winner {
            return Ok(version.value.map(Cow::Owned));
        }
        Ok(memory
            .and_then(|version| version.value.as_deref())
            .map(Cow::Borrowed))
    }

    pub fn read_latest<'a>(
        &'a self,
        key: &[u8],
    ) -> Result<Option<Cow<'a, [u8]>>, WalBackedError> {
        self.read_at(key, self.tablet.current_sequence())
    }

    fn checkpoint_state(&self) -> Result<TabletSnapshotState, WalBackedError> {
        let memory = self.tablet.snapshot_state();
        let mut merged: BTreeMap<Vec<u8>, BTreeMap<u64, Option<Vec<u8>>>> = BTreeMap::new();

        for table in &self.sstables {
            let rows = table.read_all_rows()?;
            merge_snapshot_rows(&mut merged, &rows)?;
        }
        merge_snapshot_rows(&mut merged, &memory.rows)?;

        let rows = merged
            .into_iter()
            .map(|(key, versions)| TabletSnapshotRow {
                key,
                versions: versions
                    .into_iter()
                    .map(|(sequence, value)| VersionedValue { sequence, value })
                    .collect(),
            })
            .collect();
        Ok(TabletSnapshotState {
            current_sequence: self.tablet.current_sequence(),
            rows,
        })
    }

    /// Atomically publish a self-contained checkpoint without reclaiming the WAL.
    ///
    /// The checkpoint materializes both the resident memtable tier and every
    /// active serving SSTable, so a subsequent WAL reclamation never depends on
    /// the manifest remaining available. The sequential SSTable scan bypasses
    /// the serving block cache so checkpointing cannot evict hot read blocks.
    pub fn publish_checkpoint(&self) -> Result<(), WalBackedError> {
        let state = self.checkpoint_state()?;
        let checkpoint_tablet =
            MemoryTablet::restore_snapshot(self.tablet.descriptor().clone(), state)?;
        checkpoint::write_checkpoint(&self.checkpoint_path, &checkpoint_tablet)?;
        Ok(())
    }

    /// Publish a durable checkpoint, then reclaim the WAL through that exact
    /// committed sequence. The ordering is safety-critical: if reclamation
    /// fails, the already-published checkpoint plus the full WAL remain a valid
    /// recovery state.
    pub fn checkpoint(&mut self) -> Result<(), WalBackedError> {
        self.publish_checkpoint()?;
        self.wal.reclaim_through(self.tablet.current_sequence())?;
        Ok(())
    }
}

fn merge_snapshot_rows(
    merged: &mut BTreeMap<Vec<u8>, BTreeMap<u64, Option<Vec<u8>>>>,
    rows: &[TabletSnapshotRow],
) -> Result<(), WalBackedError> {
    for row in rows {
        let versions = merged.entry(row.key.clone()).or_default();
        for version in &row.versions {
            match versions.get(&version.sequence) {
                Some(existing) if existing != &version.value => {
                    return Err(WalBackedError::SnapshotCompositionConflict {
                        sequence: version.sequence,
                    });
                }
                Some(_) => {}
                None => {
                    versions.insert(version.sequence, version.value.clone());
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalBackedError {
    Tablet(TabletError),
    Wal(WalError),
    Checkpoint(CheckpointError),
    Manifest(ManifestError),
    Sstable(SstableError),
    NoImmutableMemtable,
    FlushIdentity(String),
    ManifestSstableMismatch(String),
    SnapshotCompositionConflict { sequence: u64 },
}

impl From<TabletError> for WalBackedError {
    fn from(error: TabletError) -> Self {
        Self::Tablet(error)
    }
}

impl From<WalError> for WalBackedError {
    fn from(error: WalError) -> Self {
        Self::Wal(error)
    }
}

impl From<CheckpointError> for WalBackedError {
    fn from(error: CheckpointError) -> Self {
        Self::Checkpoint(error)
    }
}

impl From<ManifestError> for WalBackedError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<SstableError> for WalBackedError {
    fn from(error: SstableError) -> Self {
        Self::Sstable(error)
    }
}

impl fmt::Display for WalBackedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tablet(error) => write!(f, "tablet commit rejected: {error}"),
            Self::Wal(error) => write!(f, "tablet WAL failure: {error}"),
            Self::Checkpoint(error) => write!(f, "tablet checkpoint failure: {error}"),
            Self::Manifest(error) => write!(f, "tablet manifest failure: {error}"),
            Self::Sstable(error) => write!(f, "tablet SSTable failure: {error}"),
            Self::NoImmutableMemtable => f.write_str("no immutable memtable is available to flush"),
            Self::FlushIdentity(message) => write!(f, "failed to derive flush identity: {message}"),
            Self::ManifestSstableMismatch(file) => {
                write!(f, "manifest metadata does not match SSTable {file}")
            }
            Self::SnapshotCompositionConflict { sequence } => write!(
                f,
                "conflicting MVCC values while composing checkpoint at sequence {sequence}"
            ),
        }
    }
}

impl std::error::Error for WalBackedError {}
