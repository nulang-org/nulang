//! Crash-safe tablet checkpoints for the NuDB prototype.
//!
//! A checkpoint is a self-contained, checksummed MVCC image. Publication is
//! ordered as:
//!
//! ```text
//! write temp file -> sync file -> atomic rename -> sync parent directory
//! ```
//!
//! WAL reclamation is intentionally separate and may happen only after this
//! function returns success. A crash before reclamation therefore leaves either
//! the previous checkpoint or the new checkpoint plus the still-complete WAL.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::tablet::{MemoryTablet, TabletCheckpointImage, TabletDescriptor, TabletError};

const CHECKPOINT_MAGIC: &[u8; 8] = b"NUDBCP02";
const CHECKPOINT_VERSION: u16 = 2;
const HEADER_PREFIX_BYTES: usize = 2 + 8;
const HEADER_BYTES: usize = HEADER_PREFIX_BYTES + 32;
const MAX_CHECKPOINT_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DiskCheckpoint {
    version: u16,
    image: TabletCheckpointImage,
    wal_anchor_digest: Option<[u8; 32]>,
}

pub(crate) struct LoadedCheckpoint {
    pub tablet: MemoryTablet,
    pub wal_anchor_digest: Option<[u8; 32]>,
}

pub(crate) fn write_checkpoint(
    path: impl AsRef<Path>,
    tablet: &MemoryTablet,
    wal_anchor_digest: Option<[u8; 32]>,
) -> Result<u64, CheckpointError> {
    let path = path.as_ref();
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let payload = serde_json::to_vec(&DiskCheckpoint {
        version: CHECKPOINT_VERSION,
        image: tablet.checkpoint_image(),
        wal_anchor_digest,
    })
    .map_err(|error| CheckpointError::Serialization {
        message: error.to_string(),
    })?;

    if payload.len() > MAX_CHECKPOINT_BYTES {
        return Err(CheckpointError::TooLarge {
            length: payload.len(),
        });
    }

    let payload_len = u64::try_from(payload.len()).map_err(|_| CheckpointError::TooLarge {
        length: payload.len(),
    })?;
    let header = encode_header(payload_len);
    let payload_checksum = blake3::hash(&payload);
    let temp_path = temp_path(path);

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp_path)?;
    file.write_all(CHECKPOINT_MAGIC)?;
    file.write_all(&header)?;
    file.write_all(&payload)?;
    file.write_all(payload_checksum.as_bytes())?;
    file.sync_all()?;
    drop(file);

    fs::rename(&temp_path, path)?;
    sync_parent_directory(path)?;

    Ok(tablet.current_sequence())
}

pub(crate) fn load_checkpoint(
    path: impl AsRef<Path>,
    descriptor: TabletDescriptor,
) -> Result<Option<LoadedCheckpoint>, CheckpointError> {
    let path = path.as_ref();
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    let mut magic = [0_u8; CHECKPOINT_MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|error| checkpoint_read_error(error, "checkpoint magic"))?;
    if &magic != CHECKPOINT_MAGIC {
        return Err(CheckpointError::InvalidHeader);
    }

    let mut header = [0_u8; HEADER_BYTES];
    file.read_exact(&mut header)
        .map_err(|error| checkpoint_read_error(error, "checkpoint header"))?;
    let payload_len = decode_header(&header)?;

    let mut payload = vec![0_u8; payload_len];
    file.read_exact(&mut payload)
        .map_err(|error| checkpoint_read_error(error, "checkpoint payload"))?;

    let mut stored_checksum = [0_u8; 32];
    file.read_exact(&mut stored_checksum)
        .map_err(|error| checkpoint_read_error(error, "checkpoint payload checksum"))?;
    let actual_checksum = *blake3::hash(&payload).as_bytes();
    if stored_checksum != actual_checksum {
        return Err(CheckpointError::ChecksumMismatch);
    }

    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(CheckpointError::TrailingData);
    }

    let disk: DiskCheckpoint =
        serde_json::from_slice(&payload).map_err(|error| CheckpointError::Serialization {
            message: error.to_string(),
        })?;
    if disk.version != CHECKPOINT_VERSION {
        return Err(CheckpointError::UnsupportedVersion {
            version: disk.version,
        });
    }

    let tablet = MemoryTablet::from_checkpoint_image(descriptor, disk.image)?;
    if tablet.current_sequence() == 0 && disk.wal_anchor_digest.is_some() {
        return Err(CheckpointError::InvalidAnchor);
    }
    if tablet.current_sequence() != 0 && disk.wal_anchor_digest.is_none() {
        return Err(CheckpointError::InvalidAnchor);
    }
    Ok(Some(LoadedCheckpoint {
        tablet,
        wal_anchor_digest: disk.wal_anchor_digest,
    }))
}

fn encode_header(payload_len: u64) -> [u8; HEADER_BYTES] {
    let mut header = [0_u8; HEADER_BYTES];
    header[..2].copy_from_slice(&CHECKPOINT_VERSION.to_le_bytes());
    header[2..10].copy_from_slice(&payload_len.to_le_bytes());
    let checksum = blake3::hash(&header[..HEADER_PREFIX_BYTES]);
    header[HEADER_PREFIX_BYTES..].copy_from_slice(checksum.as_bytes());
    header
}

fn decode_header(header: &[u8; HEADER_BYTES]) -> Result<usize, CheckpointError> {
    let version = u16::from_le_bytes([header[0], header[1]]);
    if version != CHECKPOINT_VERSION {
        return Err(CheckpointError::UnsupportedVersion { version });
    }

    let expected_checksum = blake3::hash(&header[..HEADER_PREFIX_BYTES]);
    if &header[HEADER_PREFIX_BYTES..] != expected_checksum.as_bytes() {
        return Err(CheckpointError::HeaderChecksumMismatch);
    }

    let payload_len = u64::from_le_bytes([
        header[2], header[3], header[4], header[5], header[6], header[7], header[8], header[9],
    ]);
    let payload_len = usize::try_from(payload_len)
        .map_err(|_| CheckpointError::TooLarge { length: usize::MAX })?;
    if payload_len > MAX_CHECKPOINT_BYTES {
        return Err(CheckpointError::TooLarge {
            length: payload_len,
        });
    }
    Ok(payload_len)
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp"))
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

fn checkpoint_read_error(error: io::Error, section: &'static str) -> CheckpointError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        CheckpointError::Truncated { section }
    } else {
        error.into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointError {
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    InvalidHeader,
    HeaderChecksumMismatch,
    ChecksumMismatch,
    UnsupportedVersion {
        version: u16,
    },
    TooLarge {
        length: usize,
    },
    Truncated {
        section: &'static str,
    },
    TrailingData,
    InvalidAnchor,
    Serialization {
        message: String,
    },
    Tablet(TabletError),
}

impl From<io::Error> for CheckpointError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl From<TabletError> for CheckpointError {
    fn from(error: TabletError) -> Self {
        Self::Tablet(error)
    }
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message, .. } => write!(f, "checkpoint I/O error: {message}"),
            Self::InvalidHeader => f.write_str("invalid NuDB checkpoint header"),
            Self::HeaderChecksumMismatch => f.write_str("NuDB checkpoint header checksum mismatch"),
            Self::ChecksumMismatch => f.write_str("NuDB checkpoint payload checksum mismatch"),
            Self::UnsupportedVersion { version } => {
                write!(f, "unsupported NuDB checkpoint version {version}")
            }
            Self::TooLarge { length } => {
                write!(f, "NuDB checkpoint is too large ({length} bytes)")
            }
            Self::Truncated { section } => {
                write!(f, "truncated NuDB checkpoint while reading {section}")
            }
            Self::TrailingData => f.write_str("NuDB checkpoint contains trailing data"),
            Self::InvalidAnchor => {
                f.write_str("NuDB checkpoint WAL anchor does not match its sequence")
            }
            Self::Serialization { message } => {
                write!(f, "NuDB checkpoint serialization error: {message}")
            }
            Self::Tablet(error) => write!(f, "NuDB checkpoint tablet error: {error}"),
        }
    }
}

impl std::error::Error for CheckpointError {}
