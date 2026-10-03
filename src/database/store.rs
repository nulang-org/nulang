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

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::checkpoint::{self, CheckpointError};
use super::manifest::{
    Manifest, ManifestEntry, ManifestError, SstableFormat, SstableIntegrity,
};
use super::sstable::{self, SstableError};
use super::sstable_indexed::IndexedSstable;
use super::sstable_v2::{write_sstable_v2, SstableV2, SstableV2Error};
use super::tablet::{
    MemoryTablet, TabletDescriptor, TabletError, TabletMutation, TabletSnapshotRow,
    TabletSnapshotState, TabletWrite, VersionedValue,
};
use super::wal::{FileWal, WalError};

#[derive(Debug)]
enum ServingSstable {
    V1(IndexedSstable),
    V2(SstableV2),
}

#[derive(Debug, Clone, Copy)]
struct ServingVersion<'a> {
    sequence: u64,
    value: Option<&'a [u8]>,
}

impl ServingSstable {
    fn file_name(&self) -> &str {
        match self {
            Self::V1(table) => &table.metadata().file_name,
            Self::V2(table) => &table.metadata().file_name,
        }
    }

    fn max_sequence(&self) -> u64 {
        match self {
            Self::V1(table) => table.metadata().max_sequence,
            Self::V2(table) => table.metadata().max_sequence,
        }
    }

    fn has_contiguous_sequence_coverage_after(&self, floor: u64) -> bool {
        match self {
            Self::V1(table) => table.has_contiguous_sequence_coverage_after(floor),
            Self::V2(table) => table.has_contiguous_sequence_coverage_after(floor),
        }
    }

    fn version_at(
        &self,
        key: &[u8],
        snapshot: u64,
    ) -> Result<Option<ServingVersion<'_>>, WalBackedError> {
        match self {
            Self::V1(table) => Ok(table.version_at(key, snapshot)?.map(|version| ServingVersion {
                sequence: version.sequence,
                value: version.value,
            })),
            Self::V2(table) => Ok(table
                .version_at(key, snapshot)
                .map_err(WalBackedError::from_sstable_v2)?
                .map(|version| ServingVersion {
                    sequence: version.sequence,
                    value: version.value,
                })),
        }
    }

    fn snapshot_rows(&self, sstable_dir: &Path) -> Result<Vec<TabletSnapshotRow>, WalBackedError> {
        match self {
            Self::V1(indexed) => {
                let path = sstable_dir.join(&indexed.metadata().file_name);
                let table = sstable::Sstable::open(&path)?;
                if table.metadata() != indexed.metadata() {
                    return Err(WalBackedError::ManifestSstableMismatch(
                        indexed.metadata().file_name.clone(),
                    ));
                }
                Ok(table.rows().to_vec())
            }
            Self::V2(table) => table
                .snapshot_rows()
                .map_err(WalBackedError::from_sstable_v2),
        }
    }
}

#[derive(Debug)]
pub struct WalBackedTablet {
    tablet: MemoryTablet,
    sstables: Vec<ServingSstable>,
    wal: FileWal,
    checkpoint_path: PathBuf,
    manifest_path: PathBuf,
    sstable_dir: PathBuf,
}

impl WalBackedTablet {
    /// Open a tablet and reconstruct its MVCC state from durable storage.
    /// Manifest-referenced SSTables remain compact encoded serving sources
    /// instead of being copied back into immutable memtables.
    pub fn open(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
    ) -> Result<Self, WalBackedError> {
        let wal_path = wal_path.as_ref();
        let checkpoint_path = checkpoint::checkpoint_path_for_wal(wal_path);
        let manifest_path = wal_path.with_extension("manifest");
        let sstable_dir = wal_path.with_extension("sstables");
        let wal = FileWal::open(wal_path)?;
        let mut tablet = checkpoint::load_checkpoint(&checkpoint_path, descriptor.clone())?
            .unwrap_or_else(|| MemoryTablet::new(descriptor.clone()));

        let manifest = Manifest::load_or_empty(&manifest_path, descriptor.id().get())?;
        let mut sstables = Vec::with_capacity(manifest.entries().len());
        for entry in manifest.entries() {
            let table = open_manifest_sstable(entry, &sstable_dir, &descriptor)?;
            let max_sequence = table.max_sequence();
            if max_sequence > tablet.current_sequence()
                && table.has_contiguous_sequence_coverage_after(tablet.current_sequence())
            {
                advance_recovered_sequence(&mut tablet, max_sequence)?;
            }
            sstables.push(table);
        }

        wal.replay_after_checkpoint(&mut tablet)?;
        Ok(Self {
            tablet,
            sstables,
            wal,
            checkpoint_path,
            manifest_path,
            sstable_dir,
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

    /// Number of frozen in-memory generations that remain resident.
    pub fn immutable_memtable_count(&self) -> usize {
        self.tablet.immutable_memtable_count()
    }

    /// Freeze the active mutable generation when it reaches the supplied byte target.
    /// Rotation is in-memory only; persistence and WAL reclamation are unchanged.
    pub fn rotate_memtable_if_bytes_at_least(&mut self, min_bytes: usize) -> bool {
        self.tablet.rotate_memtable_if_bytes_at_least(min_bytes)
    }

    /// Durably flush the oldest frozen memtable to an immutable v2 SSTable and
    /// publish a format-explicit manifest entry. Once the manifest is durable
    /// and the mmap-backed reader is installed, exactly that frozen generation
    /// is evicted while newer immutable and mutable state stays resident.
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
            "tablet-{}-{}-{}-v2-{}.sst",
            self.tablet.descriptor().id().get(),
            min_sequence,
            max_sequence,
            &digest.as_str()[..16]
        );

        let mut manifest =
            Manifest::load_or_empty(&self.manifest_path, self.tablet.descriptor().id().get())?;
        let path = self.sstable_dir.join(&file_name);

        if path.exists() {
            let existing = SstableV2::open(&path).map_err(WalBackedError::from_sstable_v2)?;
            validate_v2_flush_identity(
                &existing,
                self.tablet.descriptor(),
                &file_name,
                &rows,
            )?;
        } else {
            write_sstable_v2(
                &path,
                self.tablet.descriptor().id().get(),
                self.tablet.descriptor().ownership_epoch(),
                &rows,
            )
            .map_err(WalBackedError::from_sstable_v2)?;
        }

        let table = SstableV2::open(&path).map_err(WalBackedError::from_sstable_v2)?;
        validate_v2_flush_identity(
            &table,
            self.tablet.descriptor(),
            &file_name,
            &rows,
        )?;
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
            format: SstableFormat::V2,
            integrity: SstableIntegrity::FooterBlake3(metadata.footer_checksum),
        };
        let inserted = manifest.register(entry)?;
        if inserted {
            manifest.publish(&self.manifest_path)?;
        }

        if !self
            .sstables
            .iter()
            .any(|existing| existing.file_name() == metadata.file_name)
        {
            self.sstables.push(ServingSstable::V2(table));
        }

        let evicted = self.tablet.pop_oldest_immutable_rows();
        debug_assert_eq!(evicted.as_deref(), Some(rows.as_slice()));

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

    pub fn read_at(&self, key: &[u8], snapshot: u64) -> Result<Option<&[u8]>, WalBackedError> {
        let resident_version = self.tablet.visible_version_at(key, snapshot)?;
        let mut best_sequence = resident_version.map_or(0, |version| version.sequence);
        let mut best_value = resident_version.and_then(|version| version.value.as_deref());
        let mut found = resident_version.is_some();

        for table in &self.sstables {
            let Some(candidate) = table.version_at(key, snapshot)? else {
                continue;
            };
            if !found || candidate.sequence > best_sequence {
                found = true;
                best_sequence = candidate.sequence;
                best_value = candidate.value;
            }
        }

        Ok(if found { best_value } else { None })
    }

    /// Read the newest committed value while preserving storage failures.
    /// Out-of-range keys retain the historical `read_latest` behavior and route
    /// as absent rather than as a tablet-range error.
    pub fn read_latest(&self, key: &[u8]) -> Result<Option<&[u8]>, WalBackedError> {
        if !self.tablet.descriptor().range().contains(key) {
            return Ok(None);
        }
        self.read_at(key, self.tablet.current_sequence())
    }

    fn checkpoint_state(&self) -> Result<TabletSnapshotState, WalBackedError> {
        let resident = self.tablet.snapshot_state();
        let mut merged: BTreeMap<Vec<u8>, BTreeMap<u64, Option<Vec<u8>>>> = BTreeMap::new();

        for table in &self.sstables {
            let rows = table.snapshot_rows(&self.sstable_dir)?;
            merge_snapshot_rows(&mut merged, &rows)?;
        }
        merge_snapshot_rows(&mut merged, &resident.rows)?;

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
    /// The checkpoint materializes both resident MVCC state and all active
    /// manifest-backed SSTable history before it can authorize WAL reclamation.
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

fn open_manifest_sstable(
    entry: &ManifestEntry,
    sstable_dir: &Path,
    descriptor: &TabletDescriptor,
) -> Result<ServingSstable, WalBackedError> {
    let path = sstable_dir.join(&entry.file_name);
    match (entry.format, entry.integrity) {
        (SstableFormat::V1, SstableIntegrity::WholePayloadBlake3(expected_checksum)) => {
            let table = IndexedSstable::open(&path)?;
            let metadata = table.metadata();
            if metadata.tablet_id != entry.tablet_id
                || metadata.ownership_epoch != entry.ownership_epoch
                || metadata.min_sequence != entry.min_sequence
                || metadata.max_sequence != entry.max_sequence
                || metadata.row_count != entry.row_count
                || metadata.min_key != entry.min_key
                || metadata.max_key != entry.max_key
                || metadata.checksum != expected_checksum
                || metadata.file_name != entry.file_name
                || metadata.tablet_id != descriptor.id().get()
                || metadata.ownership_epoch > descriptor.ownership_epoch()
            {
                return Err(WalBackedError::ManifestSstableMismatch(
                    entry.file_name.clone(),
                ));
            }
            Ok(ServingSstable::V1(table))
        }
        (SstableFormat::V2, SstableIntegrity::FooterBlake3(expected_checksum)) => {
            let table = SstableV2::open(&path).map_err(WalBackedError::from_sstable_v2)?;
            let metadata = table.metadata();
            if metadata.tablet_id != entry.tablet_id
                || metadata.ownership_epoch != entry.ownership_epoch
                || metadata.min_sequence != entry.min_sequence
                || metadata.max_sequence != entry.max_sequence
                || metadata.row_count != entry.row_count
                || metadata.min_key != entry.min_key
                || metadata.max_key != entry.max_key
                || metadata.footer_checksum != expected_checksum
                || metadata.file_name != entry.file_name
                || metadata.tablet_id != descriptor.id().get()
                || metadata.ownership_epoch > descriptor.ownership_epoch()
            {
                return Err(WalBackedError::ManifestSstableMismatch(
                    entry.file_name.clone(),
                ));
            }
            Ok(ServingSstable::V2(table))
        }
        _ => Err(WalBackedError::ManifestSstableMismatch(
            entry.file_name.clone(),
        )),
    }
}

fn validate_v2_flush_identity(
    table: &SstableV2,
    descriptor: &TabletDescriptor,
    file_name: &str,
    rows: &[TabletSnapshotRow],
) -> Result<(), WalBackedError> {
    let metadata = table.metadata();
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
    if metadata.file_name != file_name
        || metadata.tablet_id != descriptor.id().get()
        || metadata.ownership_epoch != descriptor.ownership_epoch()
        || metadata.min_sequence != min_sequence
        || metadata.max_sequence != max_sequence
        || metadata.row_count as usize != rows.len()
        || metadata.min_key.as_slice() != rows.first().unwrap().key.as_slice()
        || metadata.max_key.as_slice() != rows.last().unwrap().key.as_slice()
    {
        return Err(WalBackedError::SstableV2(format!(
            "existing SSTable v2 metadata does not match flush identity for {file_name}"
        )));
    }
    let decoded = table
        .snapshot_rows()
        .map_err(WalBackedError::from_sstable_v2)?;
    if decoded != rows {
        return Err(WalBackedError::SstableV2(format!(
            "existing SSTable v2 history does not match flush identity for {file_name}"
        )));
    }
    Ok(())
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

fn advance_recovered_sequence(tablet: &mut MemoryTablet, sequence: u64) -> Result<(), TabletError> {
    if sequence <= tablet.current_sequence() {
        return Ok(());
    }
    let descriptor = tablet.descriptor().clone();
    let mut state = tablet.snapshot_state();
    state.current_sequence = sequence;
    *tablet = MemoryTablet::restore_snapshot(descriptor, state)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalBackedError {
    Tablet(TabletError),
    Wal(WalError),
    Checkpoint(CheckpointError),
    Manifest(ManifestError),
    Sstable(SstableError),
    SstableV2(String),
    NoImmutableMemtable,
    FlushIdentity(String),
    ManifestSstableMismatch(String),
    SnapshotCompositionConflict { sequence: u64 },
}

impl WalBackedError {
    fn from_sstable_v2(error: SstableV2Error) -> Self {
        Self::SstableV2(error.to_string())
    }
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
            Self::SstableV2(error) => write!(f, "tablet SSTable v2 failure: {error}"),
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
