//! Crash-safe manifest for durable NuDB SSTable artifacts.
//!
//! The manifest is the authority for immutable SSTables. Compaction publishes
//! a replacement entry and persisted obsolete-file intent in one atomic
//! manifest update; physical source-file retirement is retried until durable.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MANIFEST_MAGIC: &[u8; 8] = b"NUDBMAN1";
const MANIFEST_VERSION: u16 = 1;
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ManifestEntry {
    pub(crate) file_name: String,
    pub(crate) tablet_id: u64,
    pub(crate) ownership_epoch: u64,
    pub(crate) min_sequence: u64,
    pub(crate) max_sequence: u64,
    pub(crate) row_count: u32,
    pub(crate) min_key: Vec<u8>,
    pub(crate) max_key: Vec<u8>,
    pub(crate) checksum: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DiskManifest {
    version: u16,
    tablet_id: u64,
    entries: Vec<ManifestEntry>,
    #[serde(default)]
    obsolete_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    tablet_id: u64,
    entries: Vec<ManifestEntry>,
    obsolete_files: Vec<String>,
}

impl Manifest {
    pub(crate) fn empty(tablet_id: u64) -> Self {
        Self {
            tablet_id,
            entries: Vec::new(),
            obsolete_files: Vec::new(),
        }
    }

    pub(crate) fn load_or_empty(path: &Path, tablet_id: u64) -> Result<Self, ManifestError> {
        if !path.exists() {
            return Ok(Self::empty(tablet_id));
        }
        let mut file = File::open(path)?;
        let mut magic = [0_u8; 8];
        file.read_exact(&mut magic)?;
        if &magic != MANIFEST_MAGIC {
            return Err(ManifestError::InvalidHeader);
        }
        let mut length = [0_u8; 4];
        file.read_exact(&mut length)?;
        let payload_len = u32::from_le_bytes(length) as usize;
        if payload_len > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge(payload_len));
        }
        let mut payload = vec![0_u8; payload_len];
        file.read_exact(&mut payload)?;
        let mut checksum = [0_u8; 32];
        file.read_exact(&mut checksum)?;
        if checksum != *blake3::hash(&payload).as_bytes() {
            return Err(ManifestError::ChecksumMismatch);
        }
        let mut trailing = [0_u8; 1];
        if file.read(&mut trailing)? != 0 {
            return Err(ManifestError::TrailingBytes);
        }
        let disk: DiskManifest = serde_json::from_slice(&payload)
            .map_err(|error| ManifestError::Serialization(error.to_string()))?;
        if disk.version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion(disk.version));
        }
        if disk.tablet_id != tablet_id {
            return Err(ManifestError::TabletMismatch {
                expected: tablet_id,
                presented: disk.tablet_id,
            });
        }
        validate_manifest(tablet_id, &disk.entries, &disk.obsolete_files)?;
        Ok(Self {
            tablet_id,
            entries: disk.entries,
            obsolete_files: disk.obsolete_files,
        })
    }

    pub(crate) fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    pub(crate) fn obsolete_files(&self) -> &[String] {
        &self.obsolete_files
    }

    pub(crate) fn register(&mut self, entry: ManifestEntry) -> Result<bool, ManifestError> {
        validate_entry(self.tablet_id, &entry)?;
        if self
            .obsolete_files
            .iter()
            .any(|name| name == &entry.file_name)
        {
            return Err(ManifestError::ActiveObsoleteOverlap(entry.file_name));
        }
        if let Some(existing) = self
            .entries
            .iter()
            .find(|existing| existing.file_name == entry.file_name)
        {
            if existing == &entry {
                return Ok(false);
            }
            return Err(ManifestError::ConflictingEntry(entry.file_name));
        }
        self.entries.push(entry);
        sort_entries(&mut self.entries);
        Ok(true)
    }

    /// Atomically describe a compaction result before any source file is
    /// physically retired. Every source must currently be authoritative.
    pub(crate) fn replace_entries(
        &mut self,
        source_files: &[String],
        replacement: ManifestEntry,
    ) -> Result<(), ManifestError> {
        validate_entry(self.tablet_id, &replacement)?;
        if source_files.is_empty() {
            return Err(ManifestError::EmptyReplacementSet);
        }
        let source_set: BTreeSet<&str> = source_files.iter().map(String::as_str).collect();
        if source_set.len() != source_files.len() {
            return Err(ManifestError::DuplicateReplacementSource);
        }
        for source in &source_set {
            validate_file_name(source)?;
            if !self.entries.iter().any(|entry| entry.file_name == *source) {
                return Err(ManifestError::MissingReplacementSource(
                    (*source).to_owned(),
                ));
            }
        }
        if !source_set.contains(replacement.file_name.as_str())
            && self
                .entries
                .iter()
                .any(|entry| entry.file_name == replacement.file_name)
        {
            return Err(ManifestError::ConflictingEntry(replacement.file_name));
        }

        self.entries
            .retain(|entry| !source_set.contains(entry.file_name.as_str()));
        self.entries.push(replacement.clone());
        sort_entries(&mut self.entries);

        for source in source_files {
            if source != &replacement.file_name && !self.obsolete_files.contains(source) {
                self.obsolete_files.push(source.clone());
            }
        }
        self.obsolete_files.sort();
        validate_manifest(self.tablet_id, &self.entries, &self.obsolete_files)
    }

    pub(crate) fn clear_obsolete_files(&mut self) {
        self.obsolete_files.clear();
    }

    pub(crate) fn publish(&self, path: &Path) -> Result<(), ManifestError> {
        validate_manifest(self.tablet_id, &self.entries, &self.obsolete_files)?;
        let disk = DiskManifest {
            version: MANIFEST_VERSION,
            tablet_id: self.tablet_id,
            entries: self.entries.clone(),
            obsolete_files: self.obsolete_files.clone(),
        };
        let payload = serde_json::to_vec(&disk)
            .map_err(|error| ManifestError::Serialization(error.to_string()))?;
        if payload.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge(payload.len()));
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let temp = appended_path(path, ".tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)?;
        file.write_all(MANIFEST_MAGIC)?;
        file.write_all(&(payload.len() as u32).to_le_bytes())?;
        file.write_all(&payload)?;
        file.write_all(blake3::hash(&payload).as_bytes())?;
        #[cfg(test)]
        super::interruption::hit(
            super::interruption::StorageInterruptionPoint::ManifestAfterTempWrite,
        )?;
        file.sync_data()?;
        #[cfg(test)]
        super::interruption::hit(
            super::interruption::StorageInterruptionPoint::ManifestAfterTempSync,
        )?;
        drop(file);
        fs::rename(&temp, path)?;
        #[cfg(test)]
        super::interruption::hit(
            super::interruption::StorageInterruptionPoint::ManifestAfterRename,
        )?;
        sync_parent_directory(path)?;
        #[cfg(test)]
        super::interruption::hit(
            super::interruption::StorageInterruptionPoint::ManifestAfterDirectorySync,
        )?;
        Ok(())
    }
}

fn sort_entries(entries: &mut [ManifestEntry]) {
    entries.sort_by(|a, b| {
        (a.min_sequence, a.max_sequence, &a.file_name).cmp(&(
            b.min_sequence,
            b.max_sequence,
            &b.file_name,
        ))
    });
}

fn validate_manifest(
    tablet_id: u64,
    entries: &[ManifestEntry],
    obsolete_files: &[String],
) -> Result<(), ManifestError> {
    validate_entries(tablet_id, entries)?;
    let active: BTreeSet<&str> = entries
        .iter()
        .map(|entry| entry.file_name.as_str())
        .collect();
    let mut obsolete = BTreeSet::new();
    for file_name in obsolete_files {
        validate_file_name(file_name)?;
        if !obsolete.insert(file_name.as_str()) {
            return Err(ManifestError::DuplicateObsoleteFile(file_name.clone()));
        }
        if active.contains(file_name.as_str()) {
            return Err(ManifestError::ActiveObsoleteOverlap(file_name.clone()));
        }
    }
    Ok(())
}

fn validate_entries(tablet_id: u64, entries: &[ManifestEntry]) -> Result<(), ManifestError> {
    let mut names = BTreeSet::new();
    for entry in entries {
        validate_entry(tablet_id, entry)?;
        if !names.insert(entry.file_name.as_str()) {
            return Err(ManifestError::DuplicateEntry(entry.file_name.clone()));
        }
    }
    Ok(())
}

fn validate_file_name(file_name: &str) -> Result<(), ManifestError> {
    if file_name.is_empty()
        || file_name.contains('/')
        || file_name.contains('\\')
        || file_name == "."
        || file_name == ".."
    {
        return Err(ManifestError::InvalidFileName(file_name.to_owned()));
    }
    Ok(())
}

fn validate_entry(tablet_id: u64, entry: &ManifestEntry) -> Result<(), ManifestError> {
    if entry.tablet_id != tablet_id {
        return Err(ManifestError::TabletMismatch {
            expected: tablet_id,
            presented: entry.tablet_id,
        });
    }
    validate_file_name(&entry.file_name)?;
    if entry.ownership_epoch == 0
        || entry.min_sequence == 0
        || entry.max_sequence < entry.min_sequence
        || entry.row_count == 0
        || entry.min_key > entry.max_key
    {
        return Err(ManifestError::InvalidEntry(entry.file_name.clone()));
    }
    Ok(())
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
pub enum ManifestError {
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    InvalidHeader,
    UnsupportedVersion(u16),
    TooLarge(usize),
    ChecksumMismatch,
    TrailingBytes,
    TabletMismatch {
        expected: u64,
        presented: u64,
    },
    InvalidFileName(String),
    InvalidEntry(String),
    DuplicateEntry(String),
    ConflictingEntry(String),
    EmptyReplacementSet,
    DuplicateReplacementSource,
    MissingReplacementSource(String),
    DuplicateObsoleteFile(String),
    ActiveObsoleteOverlap(String),
    Serialization(String),
}

impl From<io::Error> for ManifestError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NuDB manifest error: {self:?}")
    }
}
impl std::error::Error for ManifestError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_nudb_manifest_{}_{}.manifest",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn entry(name: &str, checksum: u8) -> ManifestEntry {
        ManifestEntry {
            file_name: name.to_owned(),
            tablet_id: 42,
            ownership_epoch: 7,
            min_sequence: checksum as u64,
            max_sequence: checksum as u64,
            row_count: 1,
            min_key: b"a".to_vec(),
            max_key: b"z".to_vec(),
            checksum: [checksum; 32],
        }
    }

    #[test]
    fn publish_load_and_deduplicate_entries() {
        let path = temp_path();
        let _ = fs::remove_file(&path);
        let mut manifest = Manifest::load_or_empty(&path, 42).unwrap();
        assert!(manifest.entries().is_empty());
        assert!(manifest.register(entry("0001.sst", 1)).unwrap());
        assert!(!manifest.register(entry("0001.sst", 1)).unwrap());
        manifest.publish(&path).unwrap();
        let reopened = Manifest::load_or_empty(&path, 42).unwrap();
        assert_eq!(reopened.entries().len(), 1);
        assert_eq!(reopened.entries()[0].file_name, "0001.sst");
        assert!(reopened.obsolete_files().is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn replacement_is_atomic_with_obsolete_intent() {
        let path = temp_path();
        let _ = fs::remove_file(&path);
        let mut manifest = Manifest::empty(42);
        for sequence in 1..=4 {
            manifest
                .register(entry(&format!("{sequence:04}.sst"), sequence))
                .unwrap();
        }
        let sources = (1..=4)
            .map(|sequence| format!("{sequence:04}.sst"))
            .collect::<Vec<_>>();
        let mut replacement = entry("compact.sst", 9);
        replacement.min_sequence = 1;
        replacement.max_sequence = 4;
        manifest.replace_entries(&sources, replacement).unwrap();
        manifest.publish(&path).unwrap();

        let reopened = Manifest::load_or_empty(&path, 42).unwrap();
        assert_eq!(reopened.entries().len(), 1);
        assert_eq!(reopened.entries()[0].file_name, "compact.sst");
        assert_eq!(reopened.obsolete_files(), sources.as_slice());

        let mut cleared = reopened;
        cleared.clear_obsolete_files();
        cleared.publish(&path).unwrap();
        assert!(Manifest::load_or_empty(&path, 42)
            .unwrap()
            .obsolete_files()
            .is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn duplicate_name_with_different_identity_fails_closed() {
        let mut manifest = Manifest::empty(42);
        manifest.register(entry("0001.sst", 1)).unwrap();
        assert_eq!(
            manifest.register(entry("0001.sst", 2)).unwrap_err(),
            ManifestError::ConflictingEntry("0001.sst".to_owned())
        );
    }

    #[test]
    fn checksum_corruption_fails_closed() {
        let path = temp_path();
        let _ = fs::remove_file(&path);
        let mut manifest = Manifest::empty(42);
        manifest.register(entry("0001.sst", 1)).unwrap();
        manifest.publish(&path).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[16] ^= 0x20;
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            Manifest::load_or_empty(&path, 42).unwrap_err(),
            ManifestError::ChecksumMismatch
        );
        let _ = fs::remove_file(path);
    }
}
