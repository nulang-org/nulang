//! Experimental binary group-commit WAL for NulangDB.
//!
//! This lives beside the proven per-record `wal` implementation so the batch
//! path can be benchmarked and hardened before it becomes the canonical WAL.
//! A batch is validated and encoded completely before the first file mutation,
//! then every record in the batch is emitted and covered by one `sync_data`.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::tablet::{TabletId, TabletMutation, TabletWrite};

const FILE_MAGIC: &[u8; 8] = b"NUDBBW01";
const FILE_HEADER_BYTES: usize = 8;
const FRAME_MAGIC: &[u8; 4] = b"NBAT";
const FRAME_VERSION: u16 = 1;
const FRAME_PREFIX_BYTES: usize = 4 + 2 + 4 + 4;
const FRAME_HEADER_BYTES: usize = FRAME_PREFIX_BYTES + 32;
const PAYLOAD_CHECKSUM_BYTES: usize = 32;
const MAX_BATCH_RECORDS: usize = 4096;
const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredBatchRecord {
    tablet_id: TabletId,
    ownership_epoch: u64,
    sequence: u64,
    expected_previous_sequence: u64,
    mutations: Vec<TabletMutation>,
}

impl RecoveredBatchRecord {
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

#[derive(Debug)]
pub struct BinaryBatchWal {
    path: PathBuf,
    file: File,
    records: Vec<RecoveredBatchRecord>,
    tablet_id: Option<TabletId>,
    latest_ownership_epoch: Option<u64>,
    poisoned: bool,
}

impl BinaryBatchWal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, BatchWalError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }

        let existed = path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;

        if file.metadata()?.len() == 0 {
            file.write_all(FILE_MAGIC)?;
            file.sync_data()?;
            if !existed {
                sync_parent_directory(&path)?;
            }
        }

        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0_u8; FILE_HEADER_BYTES];
        file.read_exact(&mut magic)?;
        if &magic != FILE_MAGIC {
            return Err(BatchWalError::InvalidFileHeader);
        }

        let mut records = Vec::new();
        let mut tablet_id = None;
        let mut latest_ownership_epoch = None;
        let mut last_sequence = 0_u64;

        loop {
            let frame_start = file.stream_position()?;
            let mut header = [0_u8; FRAME_HEADER_BYTES];
            let read = read_up_to(&mut file, &mut header)?;
            if read == 0 {
                break;
            }
            if read != header.len() {
                truncate_crash_tail(&mut file, frame_start)?;
                break;
            }

            let (record_count, payload_len) = decode_frame_header(&header, frame_start)?;
            let mut payload = vec![0_u8; payload_len];
            if read_up_to(&mut file, &mut payload)? != payload_len {
                truncate_crash_tail(&mut file, frame_start)?;
                break;
            }

            let mut checksum = [0_u8; PAYLOAD_CHECKSUM_BYTES];
            if read_up_to(&mut file, &mut checksum)? != PAYLOAD_CHECKSUM_BYTES {
                truncate_crash_tail(&mut file, frame_start)?;
                break;
            }
            if checksum != *blake3::hash(&payload).as_bytes() {
                return Err(BatchWalError::PayloadChecksumMismatch { offset: frame_start });
            }

            let decoded = decode_payload(&payload, record_count, frame_start)?;
            for record in decoded {
                validate_chain(
                    &record,
                    tablet_id,
                    latest_ownership_epoch,
                    last_sequence,
                    frame_start,
                )?;
                tablet_id = Some(record.tablet_id);
                latest_ownership_epoch = Some(record.ownership_epoch);
                last_sequence = record.sequence;
                records.push(record);
            }
        }

        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            path,
            file,
            records,
            tablet_id,
            latest_ownership_epoch,
            poisoned: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn records(&self) -> &[RecoveredBatchRecord] {
        &self.records
    }

    pub fn last_sequence(&self) -> u64 {
        self.records.last().map(|r| r.sequence).unwrap_or(0)
    }

    /// Validate the complete batch before any bytes are emitted, then durably
    /// append the whole batch with exactly one `sync_data` durability boundary.
    pub fn append_batch(&mut self, writes: &[TabletWrite]) -> Result<(), BatchWalError> {
        if self.poisoned {
            return Err(BatchWalError::Poisoned);
        }
        if writes.is_empty() {
            return Ok(());
        }
        if writes.len() > MAX_BATCH_RECORDS {
            return Err(BatchWalError::TooManyRecords(writes.len()));
        }

        let records: Vec<_> = writes.iter().map(record_from_write).collect();
        let mut expected_tablet = self.tablet_id;
        let mut latest_epoch = self.latest_ownership_epoch;
        let mut last_sequence = self.last_sequence();

        // No file mutation happens before the complete chain has validated.
        for record in &records {
            validate_chain(record, expected_tablet, latest_epoch, last_sequence, 0)?;
            expected_tablet = Some(record.tablet_id);
            latest_epoch = Some(record.ownership_epoch);
            last_sequence = record.sequence;
        }

        let payload = encode_payload(&records)?;
        if payload.len() > MAX_BATCH_BYTES {
            return Err(BatchWalError::BatchTooLarge(payload.len()));
        }
        let record_count = u32::try_from(records.len())
            .map_err(|_| BatchWalError::TooManyRecords(records.len()))?;
        let payload_len = u32::try_from(payload.len())
            .map_err(|_| BatchWalError::BatchTooLarge(payload.len()))?;
        let header = encode_frame_header(record_count, payload_len);
        let checksum = blake3::hash(&payload);

        self.file.seek(SeekFrom::End(0))?;
        self.poisoned = true;
        self.file.write_all(&header)?;
        self.file.write_all(&payload)?;
        self.file.write_all(checksum.as_bytes())?;
        self.file.sync_data()?;

        self.records.extend(records);
        self.tablet_id = expected_tablet;
        self.latest_ownership_epoch = latest_epoch;
        self.poisoned = false;
        Ok(())
    }
}

fn record_from_write(write: &TabletWrite) -> RecoveredBatchRecord {
    RecoveredBatchRecord {
        tablet_id: write.tablet_id(),
        ownership_epoch: write.ownership_epoch(),
        sequence: write.sequence(),
        expected_previous_sequence: write.expected_previous_sequence(),
        mutations: write.mutations().to_vec(),
    }
}

fn validate_chain(
    record: &RecoveredBatchRecord,
    expected_tablet: Option<TabletId>,
    latest_epoch: Option<u64>,
    last_sequence: u64,
    offset: u64,
) -> Result<(), BatchWalError> {
    if let Some(expected) = expected_tablet {
        if record.tablet_id != expected {
            return Err(BatchWalError::TabletMismatch {
                expected,
                presented: record.tablet_id,
            });
        }
    }
    if let Some(durable) = latest_epoch {
        if record.ownership_epoch < durable {
            return Err(BatchWalError::StaleOwnershipEpoch {
                durable,
                presented: record.ownership_epoch,
            });
        }
    }
    if record.expected_previous_sequence != last_sequence {
        return Err(BatchWalError::SequenceMismatch {
            committed: last_sequence,
            expected_previous: record.expected_previous_sequence,
        });
    }
    let expected = last_sequence
        .checked_add(1)
        .ok_or(BatchWalError::SequenceOverflow)?;
    if record.sequence != expected {
        return Err(BatchWalError::InvalidRecord {
            offset,
            reason: format!(
                "sequence {} does not follow predecessor {}",
                record.sequence, last_sequence
            ),
        });
    }
    Ok(())
}

fn encode_payload(records: &[RecoveredBatchRecord]) -> Result<Vec<u8>, BatchWalError> {
    let mut out = Vec::new();
    for record in records {
        out.extend_from_slice(&record.tablet_id.get().to_le_bytes());
        out.extend_from_slice(&record.ownership_epoch.to_le_bytes());
        out.extend_from_slice(&record.sequence.to_le_bytes());
        out.extend_from_slice(&record.expected_previous_sequence.to_le_bytes());
        let mutation_count = u32::try_from(record.mutations.len())
            .map_err(|_| BatchWalError::InvalidRecord { offset: 0, reason: "too many mutations".into() })?;
        out.extend_from_slice(&mutation_count.to_le_bytes());

        for mutation in &record.mutations {
            match mutation {
                TabletMutation::Put { key, value } => {
                    out.push(1);
                    put_bytes(&mut out, key)?;
                    put_bytes(&mut out, value)?;
                }
                TabletMutation::Delete { key } => {
                    out.push(2);
                    put_bytes(&mut out, key)?;
                }
            }
        }
        if out.len() > MAX_BATCH_BYTES {
            return Err(BatchWalError::BatchTooLarge(out.len()));
        }
    }
    Ok(out)
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), BatchWalError> {
    let len = u32::try_from(bytes.len()).map_err(|_| BatchWalError::BatchTooLarge(bytes.len()))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn decode_payload(
    payload: &[u8],
    record_count: usize,
    offset: u64,
) -> Result<Vec<RecoveredBatchRecord>, BatchWalError> {
    let mut cursor = 0_usize;
    let mut records = Vec::with_capacity(record_count);
    for _ in 0..record_count {
        let tablet_raw = take_u64(payload, &mut cursor, offset)?;
        let tablet_id = TabletId::new(tablet_raw).map_err(|_| BatchWalError::InvalidRecord {
            offset,
            reason: "tablet id must be non-zero".into(),
        })?;
        let ownership_epoch = take_u64(payload, &mut cursor, offset)?;
        if ownership_epoch == 0 {
            return Err(BatchWalError::InvalidRecord {
                offset,
                reason: "ownership epoch must be non-zero".into(),
            });
        }
        let sequence = take_u64(payload, &mut cursor, offset)?;
        let expected_previous_sequence = take_u64(payload, &mut cursor, offset)?;
        let mutation_count = take_u32(payload, &mut cursor, offset)? as usize;
        let mut mutations = Vec::with_capacity(mutation_count);
        for _ in 0..mutation_count {
            let kind = *payload.get(cursor).ok_or_else(|| BatchWalError::InvalidRecord {
                offset,
                reason: "truncated mutation kind".into(),
            })?;
            cursor += 1;
            let key = take_bytes(payload, &mut cursor, offset)?;
            match kind {
                1 => {
                    let value = take_bytes(payload, &mut cursor, offset)?;
                    mutations.push(TabletMutation::Put { key, value });
                }
                2 => mutations.push(TabletMutation::Delete { key }),
                _ => {
                    return Err(BatchWalError::InvalidRecord {
                        offset,
                        reason: format!("unknown mutation kind {kind}"),
                    })
                }
            }
        }
        records.push(RecoveredBatchRecord {
            tablet_id,
            ownership_epoch,
            sequence,
            expected_previous_sequence,
            mutations,
        });
    }
    if cursor != payload.len() {
        return Err(BatchWalError::InvalidRecord {
            offset,
            reason: "batch payload has trailing bytes".into(),
        });
    }
    Ok(records)
}

fn take_u64(payload: &[u8], cursor: &mut usize, offset: u64) -> Result<u64, BatchWalError> {
    let bytes = take_fixed::<8>(payload, cursor, offset)?;
    Ok(u64::from_le_bytes(bytes))
}

fn take_u32(payload: &[u8], cursor: &mut usize, offset: u64) -> Result<u32, BatchWalError> {
    let bytes = take_fixed::<4>(payload, cursor, offset)?;
    Ok(u32::from_le_bytes(bytes))
}

fn take_fixed<const N: usize>(
    payload: &[u8],
    cursor: &mut usize,
    offset: u64,
) -> Result<[u8; N], BatchWalError> {
    let end = cursor.checked_add(N).ok_or_else(|| BatchWalError::InvalidRecord {
        offset,
        reason: "payload offset overflow".into(),
    })?;
    let slice = payload.get(*cursor..end).ok_or_else(|| BatchWalError::InvalidRecord {
        offset,
        reason: "truncated batch payload".into(),
    })?;
    *cursor = end;
    Ok(slice.try_into().unwrap())
}

fn take_bytes(payload: &[u8], cursor: &mut usize, offset: u64) -> Result<Vec<u8>, BatchWalError> {
    let len = take_u32(payload, cursor, offset)? as usize;
    let end = cursor.checked_add(len).ok_or_else(|| BatchWalError::InvalidRecord {
        offset,
        reason: "payload length overflow".into(),
    })?;
    let bytes = payload.get(*cursor..end).ok_or_else(|| BatchWalError::InvalidRecord {
        offset,
        reason: "truncated byte field".into(),
    })?;
    *cursor = end;
    Ok(bytes.to_vec())
}

fn encode_frame_header(record_count: u32, payload_len: u32) -> [u8; FRAME_HEADER_BYTES] {
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    header[..4].copy_from_slice(FRAME_MAGIC);
    header[4..6].copy_from_slice(&FRAME_VERSION.to_le_bytes());
    header[6..10].copy_from_slice(&record_count.to_le_bytes());
    header[10..14].copy_from_slice(&payload_len.to_le_bytes());
    let checksum = blake3::hash(&header[..FRAME_PREFIX_BYTES]);
    header[FRAME_PREFIX_BYTES..].copy_from_slice(checksum.as_bytes());
    header
}

fn decode_frame_header(
    header: &[u8; FRAME_HEADER_BYTES],
    offset: u64,
) -> Result<(usize, usize), BatchWalError> {
    if &header[..4] != FRAME_MAGIC {
        return Err(BatchWalError::InvalidFrameHeader { offset });
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != FRAME_VERSION {
        return Err(BatchWalError::UnsupportedFrameVersion { offset, version });
    }
    let expected = blake3::hash(&header[..FRAME_PREFIX_BYTES]);
    if &header[FRAME_PREFIX_BYTES..] != expected.as_bytes() {
        return Err(BatchWalError::HeaderChecksumMismatch { offset });
    }
    let record_count = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
    let payload_len = u32::from_le_bytes(header[10..14].try_into().unwrap()) as usize;
    if record_count == 0 || record_count > MAX_BATCH_RECORDS {
        return Err(BatchWalError::TooManyRecords(record_count));
    }
    if payload_len > MAX_BATCH_BYTES {
        return Err(BatchWalError::BatchTooLarge(payload_len));
    }
    Ok((record_count, payload_len))
}

fn truncate_crash_tail(file: &mut File, valid_end: u64) -> Result<(), BatchWalError> {
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
pub enum BatchWalError {
    Io { kind: io::ErrorKind, message: String },
    InvalidFileHeader,
    InvalidFrameHeader { offset: u64 },
    UnsupportedFrameVersion { offset: u64, version: u16 },
    HeaderChecksumMismatch { offset: u64 },
    PayloadChecksumMismatch { offset: u64 },
    InvalidRecord { offset: u64, reason: String },
    TooManyRecords(usize),
    BatchTooLarge(usize),
    TabletMismatch { expected: TabletId, presented: TabletId },
    StaleOwnershipEpoch { durable: u64, presented: u64 },
    SequenceMismatch { committed: u64, expected_previous: u64 },
    SequenceOverflow,
    Poisoned,
}

impl From<io::Error> for BatchWalError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for BatchWalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message, .. } => write!(f, "batch WAL I/O error: {message}"),
            Self::InvalidFileHeader => f.write_str("invalid NulangDB binary batch WAL header"),
            Self::InvalidFrameHeader { offset } => write!(f, "invalid batch frame at {offset}"),
            Self::UnsupportedFrameVersion { offset, version } => {
                write!(f, "unsupported batch frame version {version} at {offset}")
            }
            Self::HeaderChecksumMismatch { offset } => write!(f, "batch header checksum mismatch at {offset}"),
            Self::PayloadChecksumMismatch { offset } => write!(f, "batch payload checksum mismatch at {offset}"),
            Self::InvalidRecord { offset, reason } => write!(f, "invalid batch record at {offset}: {reason}"),
            Self::TooManyRecords(count) => write!(f, "batch has too many records ({count})"),
            Self::BatchTooLarge(size) => write!(f, "batch is too large ({size} bytes)"),
            Self::TabletMismatch { expected, presented } => write!(f, "batch tablet mismatch: expected {}, got {}", expected.get(), presented.get()),
            Self::StaleOwnershipEpoch { durable, presented } => write!(f, "batch ownership epoch {presented} is stale; durable epoch is {durable}"),
            Self::SequenceMismatch { committed, expected_previous } => write!(f, "batch predecessor {expected_previous} does not match committed sequence {committed}"),
            Self::SequenceOverflow => f.write_str("batch WAL sequence overflow"),
            Self::Poisoned => f.write_str("batch WAL handle is poisoned; reopen before retrying"),
        }
    }
}

impl std::error::Error for BatchWalError {}
