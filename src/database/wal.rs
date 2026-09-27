//! Append-only write-ahead log for the NuDB tablet prototype.
//!
//! The format is intentionally small and self-validating:
//!
//! ```text
//! file := magic ("NUDBWAL1") record*
//! record := frame_header payload:[u8; payload_len] payload_blake3:[u8; 32]
//! frame_header := "NREC" frame_version:u16 payload_len:u32 header_blake3:[u8; 32]
//! ```
//!
//! Payloads are versioned JSON today so the correctness contract can evolve
//! independently of a future compact binary codec. Both framing metadata and
//! payload bytes are checksummed before deserialization. An incomplete final
//! frame is treated as a crash tail and truncated back to the last complete
//! record, while a complete-but-corrupt frame fails closed.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::tablet::{MemoryTablet, TabletDescriptor, TabletId, TabletMutation, TabletWrite};

const WAL_MAGIC: &[u8; 8] = b"NUDBWAL1";
const WAL_FRAME_MAGIC: &[u8; 4] = b"NREC";
const WAL_FRAME_VERSION: u16 = 1;
const WAL_RECORD_VERSION: u16 = 1;
const WAL_FRAME_PREFIX_BYTES: usize = 4 + 2 + 4;
const WAL_FRAME_HEADER_BYTES: usize = WAL_FRAME_PREFIX_BYTES + 32;
const MAX_WAL_RECORD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppendFailPoint {
    AfterHeader,
    AfterPayload,
    AfterChecksum,
    AfterSync,
}

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
    poisoned: bool,
    #[cfg(test)]
    append_failpoint: Option<AppendFailPoint>,
}

impl FileWal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, WalError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file_existed = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;

        if file.metadata()?.len() == 0 {
            file.write_all(WAL_MAGIC)?;
            file.sync_data()?;
            if !file_existed {
                sync_parent_directory(&path)?;
            }
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
            let mut header = [0_u8; WAL_FRAME_HEADER_BYTES];
            let header_read = read_up_to(&mut file, &mut header)?;
            if header_read == 0 {
                break;
            }
            if header_read != header.len() {
                truncate_crash_tail(&mut file, record_start)?;
                break;
            }

            let payload_len = decode_frame_header(&header, record_start)?;

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
            poisoned: false,
            #[cfg(test)]
            append_failpoint: None,
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
        if self.poisoned {
            return Err(WalError::Poisoned);
        }

        let record = WalRecord::from_write(write);
        let last_sequence = self.last_sequence();

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

        let payload =
            serde_json::to_vec(&record.to_disk()).map_err(|error| WalError::Serialization {
                message: error.to_string(),
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
        let header = encode_frame_header(payload_len);

        // Seeking does not mutate the file. Once the first frame byte may have
        // been emitted, every early return leaves this handle poisoned. The
        // only safe way to continue is to reopen, which validates/truncates the
        // physical tail before accepting another append.
        self.file.seek(SeekFrom::End(0))?;
        self.poisoned = true;

        self.file.write_all(&header)?;
        self.maybe_fail_append_for_test(AppendFailPoint::AfterHeader)?;
        self.file.write_all(&payload)?;
        self.maybe_fail_append_for_test(AppendFailPoint::AfterPayload)?;
        self.file.write_all(checksum.as_bytes())?;
        self.maybe_fail_append_for_test(AppendFailPoint::AfterChecksum)?;
        self.file.sync_data()?;
        self.maybe_fail_append_for_test(AppendFailPoint::AfterSync)?;

        let end = self.file.stream_position()?;
        self.tablet_id = Some(record.tablet_id);
        self.latest_ownership_epoch = Some(record.ownership_epoch);
        self.records.push(record);
        self.record_end_offsets.push(end);
        self.poisoned = false;
        Ok(())
    }

    #[cfg(test)]
    fn set_append_failpoint_for_test(&mut self, point: AppendFailPoint) {
        self.append_failpoint = Some(point);
    }

    #[cfg(test)]
    fn maybe_fail_append_for_test(&mut self, point: AppendFailPoint) -> io::Result<()> {
        if self.append_failpoint == Some(point) {
            self.append_failpoint = None;
            return Err(io::Error::other(format!(
                "injected NuDB WAL append failure at {point:?}"
            )));
        }
        Ok(())
    }

    #[cfg(not(test))]
    fn maybe_fail_append_for_test(&mut self, _point: AppendFailPoint) -> io::Result<()> {
        Ok(())
    }
}

fn encode_frame_header(payload_len: u32) -> [u8; WAL_FRAME_HEADER_BYTES] {
    let mut header = [0_u8; WAL_FRAME_HEADER_BYTES];
    header[..4].copy_from_slice(WAL_FRAME_MAGIC);
    header[4..6].copy_from_slice(&WAL_FRAME_VERSION.to_le_bytes());
    header[6..10].copy_from_slice(&payload_len.to_le_bytes());
    let checksum = blake3::hash(&header[..WAL_FRAME_PREFIX_BYTES]);
    header[WAL_FRAME_PREFIX_BYTES..].copy_from_slice(checksum.as_bytes());
    header
}

fn decode_frame_header(
    header: &[u8; WAL_FRAME_HEADER_BYTES],
    offset: u64,
) -> Result<usize, WalError> {
    if &header[..4] != WAL_FRAME_MAGIC {
        return Err(WalError::InvalidFrameHeader {
            offset,
            reason: "record magic mismatch".to_string(),
        });
    }

    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != WAL_FRAME_VERSION {
        return Err(WalError::UnsupportedFrameVersion { offset, version });
    }

    let expected_checksum = blake3::hash(&header[..WAL_FRAME_PREFIX_BYTES]);
    if &header[WAL_FRAME_PREFIX_BYTES..] != expected_checksum.as_bytes() {
        return Err(WalError::HeaderChecksumMismatch { offset });
    }

    let payload_len =
        u32::from_le_bytes([header[6], header[7], header[8], header[9]]) as usize;
    if payload_len > MAX_WAL_RECORD_BYTES {
        return Err(WalError::RecordTooLarge {
            offset,
            length: payload_len,
        });
    }

    Ok(payload_len)
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
            offset,
            reason: format!(
                "sequence {} does not follow predecessor {}",
                record.sequence, last_sequence
            ),
        });
    }

    Ok(())
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
    InvalidFrameHeader {
        offset: u64,
        reason: String,
    },
    UnsupportedFrameVersion {
        offset: u64,
        version: u16,
    },
    HeaderChecksumMismatch {
        offset: u64,
    },
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
    Poisoned,
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
            Self::InvalidFrameHeader { offset, reason } => {
                write!(f, "invalid WAL frame header at byte offset {offset}: {reason}")
            }
            Self::UnsupportedFrameVersion { offset, version } => write!(
                f,
                "unsupported WAL frame version {version} at byte offset {offset}"
            ),
            Self::HeaderChecksumMismatch { offset } => {
                write!(f, "WAL frame header checksum mismatch at byte offset {offset}")
            }
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
            Self::Poisoned => f.write_str(
                "NuDB WAL handle is poisoned after an ambiguous append; reopen before retrying",
            ),
        }
    }
}

impl std::error::Error for WalError {}


#[cfg(test)]
mod hardening_tests {
    use super::*;
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_WAL: AtomicU64 = AtomicU64::new(1);

    fn temp_wal(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_nudb_wal_hardening_{name}_{}_{}.wal",
            std::process::id(),
            NEXT_WAL.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn descriptor() -> TabletDescriptor {
        TabletDescriptor::new(
            TabletId::new(91).unwrap(),
            super::super::tablet::KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            1,
        )
        .unwrap()
    }

    fn write(previous: u64, key: &[u8]) -> TabletWrite {
        TabletWrite::prepare(
            &descriptor(),
            1,
            previous,
            previous,
            vec![TabletMutation::Put {
                key: key.to_vec(),
                value: b"value".to_vec(),
            }],
        )
        .unwrap()
    }

    #[test]
    fn corrupted_record_header_is_not_treated_as_a_torn_tail() {
        let path = temp_wal("header_corruption");
        let _ = fs::remove_file(&path);

        {
            let mut wal = FileWal::open(&path).unwrap();
            wal.append_write(&write(0, b"k")).unwrap();
        }

        {
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            // File magic (8) + record magic (4) + record version (2) => payload length.
            file.seek(SeekFrom::Start(14)).unwrap();
            let mut length = [0_u8; 4];
            std::io::Read::read_exact(&mut file, &mut length).unwrap();
            length[0] ^= 0x40;
            file.seek(SeekFrom::Start(14)).unwrap();
            file.write_all(&length).unwrap();
            file.sync_all().unwrap();
        }

        assert!(matches!(
            FileWal::open(&path).unwrap_err(),
            WalError::HeaderChecksumMismatch { .. }
        ));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn append_io_failure_poisons_live_wal_until_reopen() {
        let path = temp_wal("poison");
        let _ = fs::remove_file(&path);

        let mut wal = FileWal::open(&path).unwrap();
        wal.append_write(&write(0, b"k1")).unwrap();
        wal.set_append_failpoint_for_test(AppendFailPoint::AfterPayload);

        assert!(matches!(
            wal.append_write(&write(1, b"k2")),
            Err(WalError::Io { .. })
        ));
        assert_eq!(wal.append_write(&write(1, b"k3")).unwrap_err(), WalError::Poisoned);

        drop(wal);

        let mut reopened = FileWal::open(&path).unwrap();
        assert_eq!(reopened.last_sequence(), 1);
        reopened.append_write(&write(1, b"k4")).unwrap();
        assert_eq!(reopened.last_sequence(), 2);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn injected_error_after_sync_is_ambiguous_but_live_handle_stays_poisoned() {
        let path = temp_wal("after_sync");
        let _ = fs::remove_file(&path);

        let mut wal = FileWal::open(&path).unwrap();
        wal.set_append_failpoint_for_test(AppendFailPoint::AfterSync);

        assert!(matches!(
            wal.append_write(&write(0, b"k")),
            Err(WalError::Io { .. })
        ));
        assert_eq!(wal.append_write(&write(0, b"retry")).unwrap_err(), WalError::Poisoned);

        drop(wal);

        // The injected failure happens after sync_data: reopening resolves the
        // ambiguous outcome by recovering the durable record.
        let reopened = FileWal::open(&path).unwrap();
        assert_eq!(reopened.last_sequence(), 1);
        assert_eq!(reopened.records().len(), 1);

        let _ = fs::remove_file(path);
    }
}
