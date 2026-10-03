//! Offline SSTable compaction for NuDB tablets.
//!
//! This module deliberately starts with an offline authority transition. Callers
//! must quiesce the tablet before invoking compaction so no writer can publish a
//! competing manifest while the replacement is being built. The compactor keeps
//! every MVCC version and tombstone; snapshot-aware garbage collection is a later
//! policy layer.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use super::manifest::{Manifest, ManifestEntry, ManifestError, SstableFormat, SstableIntegrity};
use super::sstable::{self, SstableError};
use super::sstable_v2::{write_sstable_v2, SstableV2, SstableV2Error};
use super::tablet::{TabletDescriptor, TabletSnapshotRow, VersionedValue};

/// Rewrite all active immutable authorities into one SSTable v2.
///
/// This is intentionally offline: the caller must ensure no live tablet owner is
/// publishing writes, flushes, or manifest changes for `wal_path` during the call.
/// A single already-v2 table is a no-op; a single v1 table is rewritten so legacy
/// formats can be retired incrementally.
///
/// Durability ordering:
///
/// ```text
/// validate source manifest + tables
///   -> merge full MVCC history without GC
///   -> write + sync replacement SSTable v2
///   -> atomically publish one-entry replacement manifest
///   -> best-effort unlink obsolete SSTables
/// ```
///
/// A crash before manifest publication leaves only an orphan replacement file.
/// A crash after publication leaves the replacement authoritative and any old
/// files as harmless orphans.
pub fn compact_tablet_sstables_to_v2(
    descriptor: &TabletDescriptor,
    wal_path: impl AsRef<Path>,
) -> Result<bool, CompactionError> {
    let wal_path = wal_path.as_ref();
    let manifest_path = wal_path.with_extension("manifest");
    let sstable_dir = wal_path.with_extension("sstables");
    let source_manifest = Manifest::load_or_empty(&manifest_path, descriptor.id().get())?;

    if source_manifest.entries().is_empty() {
        return Ok(false);
    }
    if source_manifest.entries().len() == 1
        && source_manifest.entries()[0].format == SstableFormat::V2
    {
        return Ok(false);
    }

    let source_names: Vec<String> = source_manifest
        .entries()
        .iter()
        .map(|entry| entry.file_name.clone())
        .collect();
    let mut merged: BTreeMap<Vec<u8>, BTreeMap<u64, Option<Vec<u8>>>> = BTreeMap::new();

    for entry in source_manifest.entries() {
        let rows = load_manifest_rows(entry, &sstable_dir, descriptor)?;
        merge_snapshot_rows(&mut merged, &rows)?;
    }

    let rows: Vec<TabletSnapshotRow> = merged
        .into_iter()
        .map(|(key, versions)| TabletSnapshotRow {
            key,
            versions: versions
                .into_iter()
                .map(|(sequence, value)| VersionedValue { sequence, value })
                .collect(),
        })
        .collect();
    if rows.is_empty() {
        return Err(CompactionError::EmptyHistory);
    }

    let min_sequence = rows
        .iter()
        .flat_map(|row| row.versions.iter())
        .map(|version| version.sequence)
        .min()
        .ok_or(CompactionError::EmptyHistory)?;
    let max_sequence = rows
        .iter()
        .flat_map(|row| row.versions.iter())
        .map(|version| version.sequence)
        .max()
        .ok_or(CompactionError::EmptyHistory)?;
    let identity = serde_json::to_vec(&rows)
        .map_err(|error| CompactionError::Serialization(error.to_string()))?;
    let digest = blake3::hash(&identity).to_hex();
    let file_name = format!(
        "tablet-{}-{}-{}-compact-v2-{}.sst",
        descriptor.id().get(),
        min_sequence,
        max_sequence,
        &digest.as_str()[..16]
    );
    let replacement_path = sstable_dir.join(&file_name);

    if replacement_path.exists() {
        let existing = SstableV2::open(&replacement_path).map_err(CompactionError::from_v2)?;
        validate_replacement(&existing, descriptor, &file_name, &rows)?;
    } else {
        write_sstable_v2(
            &replacement_path,
            descriptor.id().get(),
            descriptor.ownership_epoch(),
            &rows,
        )
        .map_err(CompactionError::from_v2)?;
    }

    let replacement = SstableV2::open(&replacement_path).map_err(CompactionError::from_v2)?;
    validate_replacement(&replacement, descriptor, &file_name, &rows)?;
    let metadata = replacement.metadata().clone();

    // The offline contract is the real serialization boundary. This second read
    // still catches accidental in-process misuse before publication in the common
    // case, without pretending to be a lock-free compare-and-swap protocol.
    let before_publish = Manifest::load_or_empty(&manifest_path, descriptor.id().get())?;
    if before_publish != source_manifest {
        return Err(CompactionError::ConcurrentManifestChange);
    }

    let mut replacement_manifest = Manifest::empty(descriptor.id().get());
    replacement_manifest.register(ManifestEntry {
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
    })?;
    replacement_manifest.publish(&manifest_path)?;

    // Once the replacement manifest is durable, old files are no longer
    // authority. Cleanup is intentionally best-effort: a failed unlink leaks an
    // orphan but must not make a completed authority transition look rolled back.
    let mut removed_any = false;
    for old_name in source_names {
        if old_name == file_name {
            continue;
        }
        match fs::remove_file(sstable_dir.join(old_name)) {
            Ok(()) => removed_any = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
    if removed_any {
        let _ = sync_directory(&sstable_dir);
    }

    Ok(true)
}

fn load_manifest_rows(
    entry: &ManifestEntry,
    sstable_dir: &Path,
    descriptor: &TabletDescriptor,
) -> Result<Vec<TabletSnapshotRow>, CompactionError> {
    let path = sstable_dir.join(&entry.file_name);
    match (entry.format, entry.integrity) {
        (SstableFormat::V1, SstableIntegrity::WholePayloadBlake3(expected_checksum)) => {
            let table = sstable::Sstable::open(&path)?;
            let metadata = table.metadata();
            if metadata.file_name != entry.file_name
                || metadata.tablet_id != entry.tablet_id
                || metadata.ownership_epoch != entry.ownership_epoch
                || metadata.min_sequence != entry.min_sequence
                || metadata.max_sequence != entry.max_sequence
                || metadata.row_count != entry.row_count
                || metadata.min_key != entry.min_key
                || metadata.max_key != entry.max_key
                || metadata.checksum != expected_checksum
                || metadata.tablet_id != descriptor.id().get()
                || metadata.ownership_epoch > descriptor.ownership_epoch()
                || !descriptor.range().contains(&metadata.min_key)
                || !descriptor.range().contains(&metadata.max_key)
            {
                return Err(CompactionError::ManifestSstableMismatch(
                    entry.file_name.clone(),
                ));
            }
            Ok(table.rows().to_vec())
        }
        (SstableFormat::V2, SstableIntegrity::FooterBlake3(expected_checksum)) => {
            let table = SstableV2::open(&path).map_err(CompactionError::from_v2)?;
            let metadata = table.metadata();
            if metadata.file_name != entry.file_name
                || metadata.tablet_id != entry.tablet_id
                || metadata.ownership_epoch != entry.ownership_epoch
                || metadata.min_sequence != entry.min_sequence
                || metadata.max_sequence != entry.max_sequence
                || metadata.row_count != entry.row_count
                || metadata.min_key != entry.min_key
                || metadata.max_key != entry.max_key
                || metadata.footer_checksum != expected_checksum
                || metadata.tablet_id != descriptor.id().get()
                || metadata.ownership_epoch > descriptor.ownership_epoch()
                || !descriptor.range().contains(&metadata.min_key)
                || !descriptor.range().contains(&metadata.max_key)
            {
                return Err(CompactionError::ManifestSstableMismatch(
                    entry.file_name.clone(),
                ));
            }
            table.snapshot_rows().map_err(CompactionError::from_v2)
        }
        _ => Err(CompactionError::ManifestSstableMismatch(
            entry.file_name.clone(),
        )),
    }
}

fn merge_snapshot_rows(
    merged: &mut BTreeMap<Vec<u8>, BTreeMap<u64, Option<Vec<u8>>>>,
    rows: &[TabletSnapshotRow],
) -> Result<(), CompactionError> {
    for row in rows {
        let versions = merged.entry(row.key.clone()).or_default();
        for version in &row.versions {
            match versions.get(&version.sequence) {
                Some(existing) if existing != &version.value => {
                    return Err(CompactionError::SnapshotConflict {
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

fn validate_replacement(
    table: &SstableV2,
    descriptor: &TabletDescriptor,
    file_name: &str,
    rows: &[TabletSnapshotRow],
) -> Result<(), CompactionError> {
    let metadata = table.metadata();
    if metadata.file_name != file_name
        || metadata.tablet_id != descriptor.id().get()
        || metadata.ownership_epoch != descriptor.ownership_epoch()
        || metadata.row_count as usize != rows.len()
        || metadata.min_key.as_slice() != rows.first().unwrap().key.as_slice()
        || metadata.max_key.as_slice() != rows.last().unwrap().key.as_slice()
        || !descriptor.range().contains(&metadata.min_key)
        || !descriptor.range().contains(&metadata.max_key)
    {
        return Err(CompactionError::ReplacementMismatch(file_name.to_owned()));
    }
    let decoded = table
        .snapshot_rows()
        .map_err(CompactionError::from_v2)?;
    if decoded != rows {
        return Err(CompactionError::ReplacementMismatch(file_name.to_owned()));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionError {
    Manifest(ManifestError),
    Sstable(SstableError),
    SstableV2(String),
    ManifestSstableMismatch(String),
    ReplacementMismatch(String),
    SnapshotConflict { sequence: u64 },
    ConcurrentManifestChange,
    EmptyHistory,
    Serialization(String),
}

impl CompactionError {
    fn from_v2(error: SstableV2Error) -> Self {
        Self::SstableV2(error.to_string())
    }
}

impl From<ManifestError> for CompactionError {
    fn from(error: ManifestError) -> Self {
        Self::Manifest(error)
    }
}

impl From<SstableError> for CompactionError {
    fn from(error: SstableError) -> Self {
        Self::Sstable(error)
    }
}

impl fmt::Display for CompactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(error) => write!(f, "NuDB compaction manifest failure: {error}"),
            Self::Sstable(error) => write!(f, "NuDB compaction SSTable v1 failure: {error}"),
            Self::SstableV2(error) => write!(f, "NuDB compaction SSTable v2 failure: {error}"),
            Self::ManifestSstableMismatch(file) => {
                write!(f, "NuDB compaction manifest metadata does not match {file}")
            }
            Self::ReplacementMismatch(file) => {
                write!(f, "NuDB compaction replacement history does not match {file}")
            }
            Self::SnapshotConflict { sequence } => {
                write!(
                    f,
                    "NuDB compaction found conflicting MVCC values at sequence {sequence}"
                )
            }
            Self::ConcurrentManifestChange => {
                f.write_str("NuDB compaction source manifest changed before publication")
            }
            Self::EmptyHistory => f.write_str("NuDB compaction source history is empty"),
            Self::Serialization(error) => {
                write!(
                    f,
                    "NuDB compaction identity serialization failed: {error}"
                )
            }
        }
    }
}

impl std::error::Error for CompactionError {}
