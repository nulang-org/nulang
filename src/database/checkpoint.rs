//! Atomic single-tablet checkpoints for NulangDB.
//!
//! A checkpoint is published before any WAL prefix is reclaimed. Recovery may
//! therefore see either a full WAL plus a checkpoint or a compacted WAL whose
//! base sequence equals the checkpoint sequence; both states are valid.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::tablet::{MemoryTablet, TabletDescriptor, TabletSnapshotState};

const CHECKPOINT_MAGIC: &[u8; 8] = b"NUDBCKP1";
const CHECKPOINT_VERSION: u16 = 1;
const MAX_CHECKPOINT_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DiskCheckpoint {
    version: u16,
    tablet_id: u64,
    ownership_epoch: u64,
    range_start: Vec<u8>,
    range_end: Option<Vec<u8>>,
    state: TabletSnapshotState,
}

pub(crate) fn checkpoint_path_for_wal(wal_path: &Path) -> PathBuf {
    wal_path.with_extension("checkpoint")
}

pub(crate) fn write_checkpoint(path: &Path, tablet: &MemoryTablet) -> Result<(), CheckpointError> {
    let descriptor = tablet.descriptor();
    let disk = DiskCheckpoint {
        version: CHECKPOINT_VERSION,
        tablet_id: descriptor.id().get(),
        ownership_epoch: descriptor.ownership_epoch(),
        range_start: descriptor.range().start().to_vec(),
        range_end: descriptor.range().end().map(ToOwned::to_owned),
        state: tablet.snapshot_state(),
    };

    let payload = serde_json::to_vec(&disk)
        .map_err(|error| CheckpointError::Serialization(error.to_string()))?;
    if payload.len() > MAX_CHECKPOINT_BYTES {
        return Err(CheckpointError::TooLarge(payload.len()));
    }

    let temp = temp_path(path);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    file.write_all(CHECKPOINT_MAGIC)?;
    file.write_all(&(payload.len() as u32).to_le_bytes())?;
    file.write_all(&payload)?;
    file.write_all(blake3::hash(&payload).as_bytes())?;
    file.sync_data()?;
    drop(file);

    fs::rename(&temp, path)?;
    sync_parent_directory(path)?;
    Ok(())
}

pub(crate) fn load_checkpoint(
    path: &Path,
    descriptor: TabletDescriptor,
) -> Result<Option<MemoryTablet>, CheckpointError> {
    if !path.exists() {
        return Ok(None);
    }

    let mut file = File::open(path)?;
    let mut magic = [0_u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != CHECKPOINT_MAGIC {
        return Err(CheckpointError::InvalidHeader);
    }

    let mut length = [0_u8; 4];
    file.read_exact(&mut length)?;
    let payload_len = u32::from_le_bytes(length) as usize;
    if payload_len > MAX_CHECKPOINT_BYTES {
        return Err(CheckpointError::TooLarge(payload_len));
    }

    let mut payload = vec![0_u8; payload_len];
    file.read_exact(&mut payload)?;
    let mut checksum = [0_u8; 32];
    file.read_exact(&mut checksum)?;
    if checksum != *blake3::hash(&payload).as_bytes() {
        return Err(CheckpointError::ChecksumMismatch);
    }

    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(CheckpointError::TrailingBytes);
    }

    let disk: DiskCheckpoint = serde_json::from_slice(&payload)
        .map_err(|error| CheckpointError::Serialization(error.to_string()))?;
    if disk.version != CHECKPOINT_VERSION {
        return Err(CheckpointError::UnsupportedVersion(disk.version));
    }
    if disk.tablet_id != descriptor.id().get() {
        return Err(CheckpointError::DescriptorMismatch);
    }
    if descriptor.ownership_epoch() < disk.ownership_epoch {
        return Err(CheckpointError::StaleDescriptorEpoch {
            checkpoint: disk.ownership_epoch,
            presented: descriptor.ownership_epoch(),
        });
    }
    if disk.range_start != descriptor.range().start()
        || disk.range_end.as_deref() != descriptor.range().end()
    {
        return Err(CheckpointError::DescriptorMismatch);
    }

    MemoryTablet::restore_snapshot(descriptor, disk.state)
        .map(Some)
        .map_err(|error| CheckpointError::InvalidState(error.to_string()))
}

fn temp_path(path: &Path) -> PathBuf {
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".tmp");
    PathBuf::from(temp)
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
pub enum CheckpointError {
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    InvalidHeader,
    UnsupportedVersion(u16),
    TooLarge(usize),
    ChecksumMismatch,
    TrailingBytes,
    DescriptorMismatch,
    StaleDescriptorEpoch {
        checkpoint: u64,
        presented: u64,
    },
    InvalidState(String),
    Serialization(String),
}

impl From<io::Error> for CheckpointError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message, .. } => write!(f, "checkpoint I/O error: {message}"),
            Self::InvalidHeader => f.write_str("invalid NulangDB checkpoint header"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported NulangDB checkpoint version {version}")
            }
            Self::TooLarge(size) => write!(f, "NulangDB checkpoint is too large ({size} bytes)"),
            Self::ChecksumMismatch => f.write_str("NulangDB checkpoint checksum mismatch"),
            Self::TrailingBytes => f.write_str("NulangDB checkpoint has trailing bytes"),
            Self::DescriptorMismatch => {
                f.write_str("NulangDB checkpoint does not match the requested tablet descriptor")
            }
            Self::StaleDescriptorEpoch {
                checkpoint,
                presented,
            } => write!(
                f,
                "tablet descriptor epoch {presented} is older than checkpoint epoch {checkpoint}"
            ),
            Self::InvalidState(reason) => write!(f, "invalid checkpoint state: {reason}"),
            Self::Serialization(message) => {
                write!(f, "checkpoint serialization error: {message}")
            }
        }
    }
}

impl std::error::Error for CheckpointError {}
