//! Append-only write-ahead log for the NuDB tablet prototype.
//!
//! The format is intentionally small and self-validating:
//!
//! ```text
//! file := magic ("NUDBWAL1") record*
//! record := payload_len:u32-le payload:[u8; payload_len] blake3:[u8; 32]
//! ```
//!
//! Payloads are versioned JSON today so the correctness contract can evolve
//! independently of a future compact binary codec. Checksums are verified
//! before deserialization, and an incomplete final record is treated as a
//! crash tail and truncated back to the last complete record.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::tablet::{
    MemoryTablet, TabletDescriptor, TabletId, TabletMutation, TabletWrite,
};

const WAL_MAGIC: &[u8; 8] = b"NUDBWAL1";
const WAL_RECORD_VERSION: u16 = 1;
const MAX_WAL_RECORD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DiskWalRecord {
    version: u16,
    tablet_id: u64,
    ownership_epoch: u64,
    sequence: u64,
    expected_previous_sequence: u64,
    mutations: Vec<TabletMutation>,
}

/// One validated record recovered from or appended to a tablet WAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    tablet_id: TabletId,
    ownership_epoch: u64,
    sequence: u64,
    expected_previous_sequence: u64,
    mutations: Vec<TabletMutation>,
}

impl WalRecord {
    fn from_write(write: &TabletWrite) -> Self {
        Self {
            tablet_id: write.tablet_id(),
            ownership_epoch: write.ownership_epoch(),
            sequence: write.sequence(),
            expected_previous_sequence: write.expected_previous_sequence(),
            mutations: write.mutations().to_vec(),
        }
    }

    fn from_disk(disk: DiskWalRecord, offset: u64) -> Result<Self, WalError> {
        if disk.version != WAL_RECORD_VERSION {
            return Err(WalError::UnsupportedRecordVersion {
                offset,
                version: disk.version,
            });
        }
        let tablet_id = TabletId::new(disk.tablet_id).map_err(|_| WalError::InvalidRecord {
            offset,
            reason: "tablet id must be non-zero".to_string(),
        })?;
        if disk.ownership_epoch == 0 {
            return Err(WalError::InvalidRecord {
                offset,
                reason: "ownership epoch must be non-zero".to_string(),
            });
        }

        Ok(Self {
            tablet_id,
            ownership_epoch: disk.ownership_epoch,
            sequence: disk.sequence,
            expected_previous_sequence: disk.expected_previous_sequence,
            mutations: disk.mutations,
        })
    }

    fn to_disk(&self) -> DiskWalRecord {
        DiskWalRecord {
            version: WAL_RECORD_VERSION,
            tablet_id: self.tablet_id.get(),
            ownership_epoch: self.ownership_epoch,
            sequence: self.sequence,
            expected_previous_sequence: self.expected_previous_sequence,
            mutations: self.mutations.clone(),
        }
    }

    pub fn tablet_id(&self) -> TabletId {
        self.tablet_id
    }

    pub fn ownership_epoch(&self) -> u64 {
        self.ownership_epoch
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn expected_previous_sequence(&self) -> u64 {
        self.expected_previous_sequence
    }

    pub fn mutations(&self) -> &[TabletMutation] {
        &self.mutations
    }
}

/// File-backed, per-tablet append-only WAL.
///
/// A WAL starts at tablet sequence 1 and requires each new record to name the
/// current tail as its predecessor. Ownership epochs may advance over time,
/// but all records in one WAL must belong to the same tablet id.
#[derive(Debug)]
pub struct FileWal {
    path: PathBuf,
    file: File,
    records: Vec<WalRecord>,
    record_end_offsets: Vec<u64>,
    tablet_id: Option<TabletId>,
    latest_ownership_epoch: Option<u64>,
}

impl FileWal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WalError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;

        if file.metadata()?.len() == 0 {
            file.write_all(WAL_MAGIC)?;
            file.sync_data()?;
        } else {
            let mut magic = [0_u8; WAL_MAGIC.len()];
            let read = read_up_to(&mut file, &mut magic)?;
            if read != WAL_MAGIC.len() || &magic != WAL_MAGIC {
                return Err(WalError::InvalidHeader);
            }
        }

        file.seek(SeekFrom::Start(WAL_MAGIC.len() as u64))?;

        let mut records = Vec::new();
        let mut record_end_offsets = Vec::new();
        let mut tablet_id = None;
        let mut latest_ownership_epoch = None;
        let mut last_sequence = 0_u64;

        loop {
            let record_start = file.stream_position()?;
            let mut length_bytes = [0_u8; 4];
            let length_read = read_up_to(&mut file, &mut length_bytes)?;
            if length_read == 0 {
                break;
            }
            if length_read != length_bytes.len() {
                truncate_crash_tail(&mut file, record_start)?;
                break;
            }

            let payload_len = u32::from_le_bytes(length_bytes) as usize;
            if payload_len > MAX_WAL_RECORD_BYTES {
                return Err(WalError::RecordTooLarge {
                    offset: record_start,
                    length: payload_len,
                });
            }

            let mut payload = vec![0_u8; payload_len];
            if read_up_to(&mut file, &mut payload)? != payload_len {
                truncate_crash_tail(&mut file, record_start)?;
                break;
            }

            let mut stored_checksum = [0_u8; 32];
            if read_up_to(&mut file, &mut stored_checksum)? != stored_checksum.len() {
                truncate_crash_tail(&mut file, record_start)?;
                break;
            }

            let actual_checksum = *blake3::hash(&payload).as_bytes();
            if stored_checksum != actual_checksum {
                return Err(WalError::ChecksumMismatch {
                    offset: record_start,
                });
            }

            let disk: DiskWalRecord =
                serde_json::from_slice(&payload).map_err(|error| WalError::InvalidRecord {
                    offset: record_start,
                    reason: error.to_string(),
                })?;
            let record = WalRecord::from_disk(disk, record_start)?;
            validate_record_chain(
                &record,
                tablet_id,
                latest_ownership_epoch,
                last_sequence,
                record_start,
            )?;

            tablet_id = Some(record.tablet_id);
            latest_ownership_epoch = Some(record.ownership_epoch);
            last_sequence = record.sequence;
            records.push(record);
            record_end_offsets.push(file.stream_position()?);
        }

        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            path,
            file,
            records,
            record_end_offsets,
            tablet_id,
            latest_ownership_epoch,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn records(&self) -> &[WalRecord] {
        &self.records
    }

    pub fn last_sequence(&self) -> u64 {
        self.records.last().map(WalRecord::sequence).unwrap_or(0)
    }

    /// Absolute file offset immediately after the indexed record.
    pub fn record_end_offset(&self, index: usize) -> Option<u64> {
        self.record_end_offsets.get(index).copied()
    }

    /// Rebuild the single-node MVCC tablet from the durable WAL prefix.
    ///
    /// Historical ownership epochs are retained as log metadata but are not
    /// compared with the descriptor's current owner epoch during replay.
    /// Tablet identity, sequence order, and key-range membership remain
    /// fail-closed.
    pub fn recover_memory_tablet(
        &self,
        descriptor: TabletDescriptor,
    ) -> Result<MemoryTablet, WalError> {
        if let Some(existing) = self.tablet_id {
            if existing != descriptor.id() {
                return Err(WalError::TabletMismatch {
                    expected: descriptor.id(),
                    presented: existing,
                });
            }
        }
        if let Some(durable_epoch) = self.latest_ownership_epoch {
            if descriptor.ownership_epoch() < durable_epoch {
                return Err(WalError::StaleOwnershipEpoch {
                    durable: durable_epoch,
                    presented: descriptor.ownership_epoch(),
                });
            }
        }

        let mut tablet = MemoryTablet::new(descriptor);
        for record in &self.records {
            tablet
                .replay_committed(
                    record.sequence,
                    record.expected_previous_sequence,
                    record.mutations.clone(),
                )
                .map_err(|error| WalError::ReplayRejected {
                    sequence: record.sequence,
                    reason: error.to_string(),
                })?;
        }
        Ok(tablet)
    }

    /// Durably append one prepared tablet write.
    ///
    /// The in-memory tail advances only after the complete record and checksum
    /// have been written and `sync_data` succeeds. If an I/O failure leaves a
    /// partial physical append, reopening the WAL truncates that crash tail.
    pub fn append_write(&mut self, write: &TabletWrite) -> Result<(), WalError> {
        let record = WalRecord::from_write(write);
        let last_sequence = self.last_sequence();

        if let Some(durable_epoch) = latest_ownership_epoch {
        if record.ownership_epoch < durable_epoch {
            return Err(WalError::StaleOwnershipEpoch {
                durable: durable_epoch,
                presented: record.ownership_epoch,
            });
        }
    }

    if record.expected_previous_sequence != last_sequence {
            return Err(WalError::SequenceMismatch {
                committed: last_sequence,
                expected_previous: record.expected_previous_sequence,
            });
        }
        let expected_sequence = last_sequence
            .checked_add(1)
            .ok_or(WalError::SequenceOverflow)?;
        if record.sequence != expected_sequence {
            return Err(WalError::InvalidRecord {
                offset: self.file.stream_position()?,
                reason: format!(
                    "sequence {} does not follow predecessor {}",
                    record.sequence, last_sequence
                ),
            });
        }
        if let Some(existing) = self.tablet_id {
            if record.tablet_id != existing {
                return Err(WalError::TabletMismatch {
                    expected: existing,
                    presented: record.tablet_id,
                });
            }
        }
        if let Some(durable_epoch) = self.latest_ownership_epoch {
            if record.ownership_epoch < durable_epoch {
                return Err(WalError::StaleOwnershipEpoch {
                    durable: durable_epoch,
                    presented: record.ownership_epoch,
                });
            }
        }

        let payload = serde_json::to_vec(&record.to_disk()).map_err(|error| {
            WalError::Serialization {
                message: error.to_string(),
            }
        })?;
        if payload.len() > MAX_WAL_RECORD_BYTES {
            return Err(WalError::RecordTooLarge {
                offset: self.file.stream_position()?,
                length: payload.len(),
            });
        }

        let payload_len = u32::try_from(payload.len()).map_err(|_| WalError::RecordTooLarge {
            offset: self.file.stream_position().unwrap_or(0),
            length: payload.len(),
        })?;
        let checksum = blake3::hash(&payload);

        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&payload_len.to_le_bytes())?;
        self.file.write_all(&payload)?;
        self.file.write_all(checksum.as_bytes())?;
        self.file.sync_data()?;

        let end = self.file.stream_position()?;
        self.tablet_id = Some(record.tablet_id);
        self.latest_ownership_epoch = Some(record.ownership_epoch);
        self.records.push(record);
        self.record_end_offsets.push(end);
        Ok(())
    }
}

fn validate_record_chain(
    record: &WalRecord,
    expected_tablet: Option<TabletId>,
    latest_ownership_epoch: Option<u64>,
    last_sequence: u64,
    offset: u64,
) -> Result<(), WalError> {
    if let Some(expected) = expected_tablet {
        if record.tablet_id != expected {
            return Err(WalError::TabletMismatch {
                expected,
                presented: record.tablet_id,
            });
        }
    }

    if record.expected_previous_sequence != last_sequence {
        return Err(WalError::SequenceMismatch {
            committed: last_sequence,
            expected_previous: record.expected_previous_sequence,
        });
    }

    let expected_sequence = last_sequence
        .checked_add(1)
        .ok_or(WalError::SequenceOverflow)?;
    if record.sequence != expected_sequence {
        return Err(WalError::InvalidRecord {
            offset,
            reason: format!(
                "sequence {} does not follow predecessor {}",
                record.sequence, last_sequence
            ),
        });
    }

    Ok(())
}

fn truncate_crash_tail(file: &mut File, valid_end: u64) -> Result<(), WalError> {
    file.set_len(valid_end)?;
    file.sync_data()?;
    file.seek(SeekFrom::Start(valid_end))?;
    Ok(())
}

fn read_up_to(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buffer.len() {
        match file.read(&mut buffer[total..])? {
            0 => break,
            count => total += count,
        }
    }
    Ok(total)
}

/// WAL validation or I/O failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalError {
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    InvalidHeader,
    UnsupportedRecordVersion {
        offset: u64,
        version: u16,
    },
    InvalidRecord {
        offset: u64,
        reason: String,
    },
    RecordTooLarge {
        offset: u64,
        length: usize,
    },
    ChecksumMismatch {
        offset: u64,
    },
    TabletMismatch {
        expected: TabletId,
        presented: TabletId,
    },
    StaleOwnershipEpoch {
        durable: u64,
        presented: u64,
    },
    SequenceMismatch {
        committed: u64,
        expected_previous: u64,
    },
    SequenceOverflow,
    ReplayRejected {
        sequence: u64,
        reason: String,
    },
    Serialization {
        message: String,
    },
}

impl From<io::Error> for WalError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for WalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message, .. } => write!(f, "WAL I/O error: {message}"),
            Self::InvalidHeader => f.write_str("invalid NuDB WAL header"),
            Self::UnsupportedRecordVersion { offset, version } => write!(
                f,
                "unsupported WAL record version {version} at byte offset {offset}"
            ),
            Self::InvalidRecord { offset, reason } => {
                write!(f, "invalid WAL record at byte offset {offset}: {reason}")
            }
            Self::RecordTooLarge { offset, length } => write!(
                f,
                "WAL record at byte offset {offset} is too large ({length} bytes)"
            ),
            Self::ChecksumMismatch { offset } => {
                write!(f, "WAL checksum mismatch at byte offset {offset}")
            }
            Self::TabletMismatch {
                expected,
                presented,
            } => write!(
                f,
                "WAL tablet mismatch: expected {}, got {}",
                expected.get(),
                presented.get()
            ),
            Self::StaleOwnershipEpoch { durable, presented } => write!(
                f,
                "WAL ownership epoch {presented} is stale; durable epoch is {durable}"
            ),
            Self::SequenceMismatch {
                committed,
                expected_previous,
            } => write!(
                f,
                "WAL predecessor {expected_previous} does not match committed sequence {committed}"
            ),
            Self::SequenceOverflow => f.write_str("WAL sequence overflow"),
            Self::ReplayRejected { sequence, reason } => {
                write!(f, "WAL replay rejected sequence {sequence}: {reason}")
            }
            Self::Serialization { message } => {
                write!(f, "WAL serialization error: {message}")
            }
        }
    }
}

impl std::error::Error for WalError {}
