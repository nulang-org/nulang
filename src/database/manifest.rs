//! Crash-safe manifest for durable NuDB SSTable flush artifacts.
//!
//! Manifest v2 makes the SSTable format and integrity contract explicit. Legacy
//! `NUDBMAN1` files remain readable and are translated in memory to v1 entries
//! with whole-payload BLAKE3 integrity. New publications use `NUDBMAN2` and can
//! describe both v1 whole-payload and v2 footer integrity without overloading one
//! checksum field with two different meanings.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MANIFEST_MAGIC_V1: &[u8; 8] = b"NUDBMAN1";
const MANIFEST_MAGIC_V2: &[u8; 8] = b"NUDBMAN2";
const MANIFEST_VERSION_V1: u16 = 1;
const MANIFEST_VERSION_V2: u16 = 2;
const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SstableFormat {
    V1,
    V2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "checksum", rename_all = "snake_case")]
pub(crate) enum SstableIntegrity {
    WholePayloadBlake3([u8; 32]),
    FooterBlake3([u8; 32]),
}

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
    pub(crate) format: SstableFormat,
    pub(crate) integrity: SstableIntegrity,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct LegacyManifestEntry {
    file_name: String,
    tablet_id: u64,
    ownership_epoch: u64,
    min_sequence: u64,
    max_sequence: u64,
    row_count: u32,
    min_key: Vec<u8>,
    max_key: Vec<u8>,
    checksum: [u8; 32],
}

impl From<LegacyManifestEntry> for ManifestEntry {
    fn from(entry: LegacyManifestEntry) -> Self {
        Self {
            file_name: entry.file_name,
            tablet_id: entry.tablet_id,
            ownership_epoch: entry.ownership_epoch,
            min_sequence: entry.min_sequence,
            max_sequence: entry.max_sequence,
            row_count: entry.row_count,
            min_key: entry.min_key,
            max_key: entry.max_key,
            format: SstableFormat::V1,
            integrity: SstableIntegrity::WholePayloadBlake3(entry.checksum),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DiskManifestV1 {
    version: u16,
    tablet_id: u64,
    entries: Vec<LegacyManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DiskManifestV2 {
    version: u16,
    tablet_id: u64,
    entries: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    tablet_id: u64,
    entries: Vec<ManifestEntry>,
}

impl Manifest {
    pub(crate) fn empty(tablet_id: u64) -> Self {
        Self {
            tablet_id,
            entries: Vec::new(),
        }
    }

    pub(crate) fn load_or_empty(path: &Path, tablet_id: u64) -> Result<Self, ManifestError> {
        if !path.exists() {
            return Ok(Self::empty(tablet_id));
        }
        let mut file = File::open(path)?;
        let mut magic = [0_u8; 8];
        file.read_exact(&mut magic)?;
        if &magic != MANIFEST_MAGIC_V1 && &magic != MANIFEST_MAGIC_V2 {
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

        let (presented_tablet_id, entries) = if &magic == MANIFEST_MAGIC_V1 {
            let disk: DiskManifestV1 = serde_json::from_slice(&payload)
                .map_err(|error| ManifestError::Serialization(error.to_string()))?;
            if disk.version != MANIFEST_VERSION_V1 {
                return Err(ManifestError::UnsupportedVersion(disk.version));
            }
            (
                disk.tablet_id,
                disk.entries.into_iter().map(ManifestEntry::from).collect(),
            )
        } else {
            let disk: DiskManifestV2 = serde_json::from_slice(&payload)
                .map_err(|error| ManifestError::Serialization(error.to_string()))?;
            if disk.version != MANIFEST_VERSION_V2 {
                return Err(ManifestError::UnsupportedVersion(disk.version));
            }
            (disk.tablet_id, disk.entries)
        };

        if presented_tablet_id != tablet_id {
            return Err(ManifestError::TabletMismatch {
                expected: tablet_id,
                presented: presented_tablet_id,
            });
        }
        validate_entries(tablet_id, &entries)?;
        Ok(Self { tablet_id, entries })
    }

    pub(crate) fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    pub(crate) fn register(&mut self, entry: ManifestEntry) -> Result<bool, ManifestError> {
        validate_entry(self.tablet_id, &entry)?;
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
        self.entries.sort_by(|a, b| {
            (a.min_sequence, a.max_sequence, &a.file_name).cmp(&(
                b.min_sequence,
                b.max_sequence,
                &b.file_name,
            ))
        });
        Ok(true)
    }

    pub(crate) fn publish(&self, path: &Path) -> Result<(), ManifestError> {
        validate_entries(self.tablet_id, &self.entries)?;
        let disk = DiskManifestV2 {
            version: MANIFEST_VERSION_V2,
            tablet_id: self.tablet_id,
            entries: self.entries.clone(),
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
        file.write_all(MANIFEST_MAGIC_V2)?;
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

fn validate_entries(tablet_id: u64, entries: &[ManifestEntry]) -> Result<(), ManifestError> {
    let mut names = std::collections::BTreeSet::new();
    for entry in entries {
        validate_entry(tablet_id, entry)?;
        if !names.insert(entry.file_name.as_str()) {
            return Err(ManifestError::DuplicateEntry(entry.file_name.clone()));
        }
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
    if entry.file_name.is_empty()
        || entry.file_name.contains('/')
        || entry.file_name.contains('\\')
        || entry.file_name == "."
        || entry.file_name == ".."
    {
        return Err(ManifestError::InvalidFileName(entry.file_name.clone()));
    }
    if entry.ownership_epoch == 0
        || entry.min_sequence == 0
        || entry.max_sequence < entry.min_sequence
        || entry.row_count == 0
        || entry.min_key > entry.max_key
    {
        return Err(ManifestError::InvalidEntry(entry.file_name.clone()));
    }
    if !matches!(
        (entry.format, entry.integrity),
        (SstableFormat::V1, SstableIntegrity::WholePayloadBlake3(_))
            | (SstableFormat::V2, SstableIntegrity::FooterBlake3(_))
    ) {
        return Err(ManifestError::IntegrityKindMismatch(
            entry.file_name.clone(),
        ));
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
    IntegrityKindMismatch(String),
    DuplicateEntry(String),
    ConflictingEntry(String),
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
            min_sequence: 1,
            max_sequence: 3,
            row_count: 2,
            min_key: b"a".to_vec(),
            max_key: b"z".to_vec(),
            format: SstableFormat::V1,
            integrity: SstableIntegrity::WholePayloadBlake3([checksum; 32]),
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
        assert_eq!(reopened.entries()[0].format, SstableFormat::V1);
        assert_eq!(
            reopened.entries()[0].integrity,
            SstableIntegrity::WholePayloadBlake3([1; 32])
        );
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
