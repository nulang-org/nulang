//! Durable local storage for Nulang Fabric streams.
//!
//! This is the first stream-storage slice: an append-only segmented log with
//! monotonic sequence numbers, per-record checksums, crash-tail repair,
//! persisted consumer cursors, and replay. Replication, retention, consumer
//! groups, and ACK/NACK redelivery build on this storage contract later.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::runtime::Runtime;

const STREAM_FORMAT_VERSION: u16 = 1;
const SEGMENT_MAGIC: &[u8; 4] = b"FSTR";
const SEGMENT_HEADER_LEN: usize = 4 + 2 + 8;
const RECORD_HEADER_LEN: usize = 8 + 4 + 16;
const MIN_SEGMENT_BYTES: u64 = (SEGMENT_HEADER_LEN + RECORD_HEADER_LEN + 1) as u64;
const DEFAULT_SEGMENT_MAX_BYTES: u64 = 4 * 1024 * 1024;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FabricStreamConfig {
    /// Maximum segment size before rotation. A single record larger than this
    /// limit is still accepted into its own segment.
    pub segment_max_bytes: u64,
}

impl Default for FabricStreamConfig {
    fn default() -> Self {
        Self {
            segment_max_bytes: DEFAULT_SEGMENT_MAX_BYTES,
        }
    }
}

impl FabricStreamConfig {
    fn validate(self) -> io::Result<Self> {
        if self.segment_max_bytes < MIN_SEGMENT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Fabric stream segment_max_bytes must be at least {MIN_SEGMENT_BYTES}"),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricStreamRecord {
    pub sequence: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricStreamInfo {
    pub name: String,
    pub segment_max_bytes: u64,
    pub segment_count: usize,
    pub next_sequence: u64,
    pub last_sequence: Option<u64>,
    /// Highest sequence known committed by the stream's replication policy.
    /// Local/raw appends may exist beyond this boundary.
    pub committed_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StreamMetadata {
    version: u16,
    config: FabricStreamConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorFile {
    version: u16,
    cursors: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitFile {
    version: u16,
    committed_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamPendingIntent {
    pub partition: u16,
    pub leader: u64,
    pub membership_fingerprint: u64,
    pub replication_factor: usize,
    pub replicas: Vec<u64>,
    pub sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplicationFile {
    version: u16,
    pending: BTreeMap<u64, FabricStreamPendingIntent>,
}

impl Default for ReplicationFile {
    fn default() -> Self {
        Self {
            version: STREAM_FORMAT_VERSION,
            pending: BTreeMap::new(),
        }
    }
}

impl Default for CommitFile {
    fn default() -> Self {
        Self {
            version: STREAM_FORMAT_VERSION,
            committed_sequence: 0,
        }
    }
}

impl Default for CursorFile {
    fn default() -> Self {
        Self {
            version: STREAM_FORMAT_VERSION,
            cursors: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct StreamState {
    config: FabricStreamConfig,
    next_sequence: u64,
    current_segment_base: u64,
    current_segment_len: u64,
}

/// File-backed segmented Fabric stream store.
///
/// Every append is flushed and `sync_data`'d before success is reported.
/// Stream metadata and cursor files use atomic temp-file + rename replacement.
/// On open, a truncated final record is repaired back to the last complete
/// frame; checksum or sequence corruption fails closed.
#[derive(Debug)]
pub struct FileFabricStreamStore {
    root: PathBuf,
    states: HashMap<String, StreamState>,
}

impl FileFabricStreamStore {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            states: HashMap::new(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create_stream(&mut self, name: &str, config: FabricStreamConfig) -> io::Result<()> {
        validate_name("stream", name)?;
        let config = config.validate()?;
        let dir = self.stream_dir(name);
        let metadata_path = dir.join("meta.json");
        if metadata_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("Fabric stream `{name}` already exists"),
            ));
        }

        fs::create_dir_all(&dir)?;
        let metadata = StreamMetadata {
            version: STREAM_FORMAT_VERSION,
            config,
        };
        write_json_atomic(&metadata_path, &metadata)?;
        sync_dir(&dir)?;
        self.states.insert(
            name.to_string(),
            StreamState {
                config,
                next_sequence: 1,
                current_segment_base: 1,
                current_segment_len: 0,
            },
        );
        Ok(())
    }

    pub fn stream_info(&mut self, name: &str) -> io::Result<FabricStreamInfo> {
        self.ensure_state(name)?;
        let state = self
            .states
            .get(name)
            .expect("stream state must exist after ensure_state");
        let segment_count = list_segments(&self.stream_dir(name))?.len();
        let committed_sequence =
            read_commit(&self.stream_dir(name).join("commit.json"))?.committed_sequence;
        Ok(FabricStreamInfo {
            name: name.to_string(),
            segment_max_bytes: state.config.segment_max_bytes,
            segment_count,
            next_sequence: state.next_sequence,
            last_sequence: state.next_sequence.checked_sub(1).filter(|&seq| seq > 0),
            committed_sequence,
        })
    }

    pub fn append(&mut self, name: &str, payload: &[u8]) -> io::Result<u64> {
        self.ensure_state(name)?;
        let sequence = self
            .states
            .get(name)
            .expect("stream state must exist after ensure_state")
            .next_sequence;
        self.append_exact(name, sequence, payload)?;
        Ok(sequence)
    }

    /// Apply a replicated record at an exact sequence.
    ///
    /// Returns `Ok(true)` when a new record was appended and `Ok(false)`
    /// when the same sequence+payload was already present (idempotent retry).
    /// Sequence gaps and conflicting duplicates fail closed.
    pub fn append_replica(
        &mut self,
        name: &str,
        sequence: u64,
        payload: &[u8],
    ) -> io::Result<bool> {
        self.ensure_state(name)?;
        let next_sequence = self
            .states
            .get(name)
            .expect("stream state must exist after ensure_state")
            .next_sequence;

        if sequence < next_sequence {
            let existing = self.read_from(name, sequence, 1)?;
            return match existing.first() {
                Some(record) if record.sequence == sequence && record.payload == payload => Ok(false),
                Some(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Fabric replica conflict at sequence {sequence}: existing payload differs"
                    ),
                )),
                None => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Fabric replica sequence {sequence} is below next sequence {next_sequence} but missing from the log"
                    ),
                )),
            };
        }
        if sequence > next_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Fabric replica sequence gap: got {sequence}, expected {next_sequence}"),
            ));
        }

        self.append_exact(name, sequence, payload)?;
        Ok(true)
    }

    pub fn stream_config(&mut self, name: &str) -> io::Result<FabricStreamConfig> {
        self.ensure_state(name)?;
        Ok(self
            .states
            .get(name)
            .expect("stream state must exist after ensure_state")
            .config)
    }

    fn append_exact(&mut self, name: &str, sequence: u64, payload: &[u8]) -> io::Result<()> {
        if payload.len() > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric stream record exceeds u32 payload limit",
            ));
        }

        let (config, mut segment_base, mut segment_len) = {
            let state = self
                .states
                .get(name)
                .expect("stream state must exist before append_exact");
            if state.next_sequence != sequence {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Fabric append sequence mismatch: got {sequence}, expected {}",
                        state.next_sequence
                    ),
                ));
            }
            (
                state.config,
                state.current_segment_base,
                state.current_segment_len,
            )
        };
        let frame = encode_record(sequence, payload);

        let dir = self.stream_dir(name);
        if segment_len == 0 {
            segment_base = sequence;
            let path = segment_path(&dir, segment_base);
            if let Err(error) = create_segment(&path, segment_base) {
                self.states.remove(name);
                return Err(error);
            }
            segment_len = SEGMENT_HEADER_LEN as u64;
        } else if segment_len > SEGMENT_HEADER_LEN as u64
            && segment_len.saturating_add(frame.len() as u64) > config.segment_max_bytes
        {
            segment_base = sequence;
            let path = segment_path(&dir, segment_base);
            if let Err(error) = create_segment(&path, segment_base) {
                self.states.remove(name);
                return Err(error);
            }
            segment_len = SEGMENT_HEADER_LEN as u64;
        }

        let path = segment_path(&dir, segment_base);
        let append_result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new().append(true).open(&path)?;
            file.write_all(&frame)?;
            file.flush()?;
            file.sync_data()?;
            Ok(())
        })();

        if let Err(error) = append_result {
            // The write may have reached disk before an fsync error. Force the
            // next operation to rescan/recover rather than trusting cached state.
            self.states.remove(name);
            return Err(error);
        }

        let state = self
            .states
            .get_mut(name)
            .expect("stream state must still exist after successful append");
        state.next_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Fabric sequence overflow"))?;
        state.current_segment_base = segment_base;
        state.current_segment_len = segment_len + frame.len() as u64;
        Ok(())
    }

    pub fn read_from(
        &mut self,
        name: &str,
        start_sequence: u64,
        limit: usize,
    ) -> io::Result<Vec<FabricStreamRecord>> {
        self.ensure_state(name)?;
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut result = Vec::with_capacity(limit.min(256));
        for (base, path) in list_segments(&self.stream_dir(name))? {
            let records = decode_segment(&path, base, false)?;
            for record in records {
                if record.sequence < start_sequence {
                    continue;
                }
                result.push(record);
                if result.len() == limit {
                    return Ok(result);
                }
            }
        }
        Ok(result)
    }

    /// Highest sequence made visible by the stream replication policy.
    pub fn committed_sequence(&mut self, name: &str) -> io::Result<u64> {
        self.ensure_state(name)?;
        Ok(read_commit(&self.stream_dir(name).join("commit.json"))?.committed_sequence)
    }

    /// Persist a monotonic committed boundary.
    ///
    /// The boundary may never move past the local durable tail. This method
    /// does not decide quorum; it only durably records a decision made by the
    /// replication layer.
    pub fn commit_through(&mut self, name: &str, sequence: u64) -> io::Result<()> {
        self.ensure_state(name)?;
        let tail = self
            .states
            .get(name)
            .and_then(|state| state.next_sequence.checked_sub(1))
            .filter(|&seq| seq > 0)
            .unwrap_or(0);
        if sequence > tail {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Fabric committed sequence {sequence} is beyond local stream tail {tail}"),
            ));
        }

        let path = self.stream_dir(name).join("commit.json");
        let mut commit = read_commit(&path)?;
        if sequence < commit.committed_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Fabric committed sequence cannot move backwards from {} to {sequence}",
                    commit.committed_sequence
                ),
            ));
        }
        if sequence == commit.committed_sequence {
            return Ok(());
        }
        commit.committed_sequence = sequence;
        write_json_atomic(&path, &commit)?;
        sync_dir(&self.stream_dir(name))
    }

    /// Read only records at or below the persisted committed boundary.
    pub fn read_committed(
        &mut self,
        name: &str,
        start_sequence: u64,
        limit: usize,
    ) -> io::Result<Vec<FabricStreamRecord>> {
        let committed = self.committed_sequence(name)?;
        if committed == 0 || start_sequence > committed || limit == 0 {
            return Ok(Vec::new());
        }
        let mut records = self.read_from(name, start_sequence, limit)?;
        records.retain(|record| record.sequence <= committed);
        Ok(records)
    }

    /// Persist replication intent before the matching leader append.
    ///
    /// Reserving first closes the crash window where an uncommitted durable
    /// record could exist without enough metadata to reconstruct replication.
    pub(crate) fn reserve_replication_intent(
        &mut self,
        name: &str,
        intent: FabricStreamPendingIntent,
    ) -> io::Result<()> {
        self.ensure_state(name)?;
        let next_sequence = self
            .states
            .get(name)
            .expect("stream state must exist after ensure_state")
            .next_sequence;
        if intent.sequence != next_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Fabric replication intent sequence {} must equal next stream sequence {}",
                    intent.sequence, next_sequence
                ),
            ));
        }
        if intent.replication_factor == 0
            || intent.replicas.len() != intent.replication_factor
            || !intent.replicas.contains(&intent.leader)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Fabric replication intent replica set",
            ));
        }

        let path = self.stream_dir(name).join("replication.json");
        let mut file = read_replication(&path)?;
        if let Some(existing) = file.pending.get(&intent.sequence) {
            if existing == &intent {
                return Ok(());
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "conflicting Fabric replication intent already exists",
            ));
        }
        file.pending.insert(intent.sequence, intent);
        write_json_atomic(&path, &file)?;
        sync_dir(&self.stream_dir(name))
    }

    pub(crate) fn pending_replication_intents(
        &mut self,
        name: &str,
    ) -> io::Result<Vec<FabricStreamPendingIntent>> {
        self.ensure_state(name)?;
        Ok(read_replication(&self.stream_dir(name).join("replication.json"))?
            .pending
            .into_values()
            .collect())
    }

    pub(crate) fn remove_replication_intent(
        &mut self,
        name: &str,
        sequence: u64,
    ) -> io::Result<bool> {
        self.ensure_state(name)?;
        let path = self.stream_dir(name).join("replication.json");
        let mut file = read_replication(&path)?;
        let removed = file.pending.remove(&sequence).is_some();
        if removed {
            write_json_atomic(&path, &file)?;
            sync_dir(&self.stream_dir(name))?;
        }
        Ok(removed)
    }

    pub(crate) fn append_reserved_replica(
        &mut self,
        name: &str,
        sequence: u64,
        payload: &[u8],
    ) -> io::Result<()> {
        let intents = self.pending_replication_intents(name)?;
        if !intents.iter().any(|intent| intent.sequence == sequence) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Fabric reserved append has no durable replication intent",
            ));
        }
        let appended = self.append_replica(name, sequence, payload)?;
        if !appended {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Fabric reserved append sequence already exists",
            ));
        }
        Ok(())
    }

    /// Read records after the consumer's last committed sequence.
    pub fn read_consumer(
        &mut self,
        name: &str,
        consumer: &str,
        limit: usize,
    ) -> io::Result<Vec<FabricStreamRecord>> {
        let cursor = self.cursor(name, consumer)?;
        self.read_from(name, cursor.saturating_add(1), limit)
    }

    /// Persist a consumer's last fully processed sequence.
    ///
    /// Cursors are monotonic. Committing beyond the stream tail or moving a
    /// cursor backwards is rejected.
    pub fn commit_cursor(&mut self, name: &str, consumer: &str, sequence: u64) -> io::Result<()> {
        validate_name("consumer", consumer)?;
        self.ensure_state(name)?;
        let last_sequence = self
            .states
            .get(name)
            .and_then(|state| state.next_sequence.checked_sub(1))
            .filter(|&seq| seq > 0)
            .unwrap_or(0);
        if sequence > last_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Fabric cursor {sequence} is beyond stream tail {last_sequence}"),
            ));
        }

        let path = self.stream_dir(name).join("cursors.json");
        let mut cursors = read_cursors(&path)?;
        let current = cursors.cursors.get(consumer).copied().unwrap_or(0);
        if sequence < current {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Fabric cursor cannot move backwards from {current} to {sequence}"),
            ));
        }
        cursors.cursors.insert(consumer.to_string(), sequence);
        write_json_atomic(&path, &cursors)?;
        sync_dir(&self.stream_dir(name))
    }

    pub fn cursor(&mut self, name: &str, consumer: &str) -> io::Result<u64> {
        validate_name("consumer", consumer)?;
        self.ensure_state(name)?;
        let path = self.stream_dir(name).join("cursors.json");
        Ok(read_cursors(&path)?
            .cursors
            .get(consumer)
            .copied()
            .unwrap_or(0))
    }

    fn stream_dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn ensure_state(&mut self, name: &str) -> io::Result<()> {
        validate_name("stream", name)?;
        if self.states.contains_key(name) {
            return Ok(());
        }
        let state = recover_stream(&self.stream_dir(name))?;
        self.states.insert(name.to_string(), state);
        Ok(())
    }
}

impl Runtime {
    /// Enable durable local Fabric stream storage rooted at `path`.
    pub fn fabric_stream_open(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        self.distributed.fabric_streams = Some(FileFabricStreamStore::open(path)?);
        Ok(())
    }

    pub fn fabric_stream_create(
        &mut self,
        name: &str,
        config: FabricStreamConfig,
    ) -> io::Result<()> {
        self.fabric_stream_store_mut()?.create_stream(name, config)
    }

    pub fn fabric_stream_append(&mut self, name: &str, payload: &[u8]) -> io::Result<u64> {
        self.fabric_stream_store_mut()?.append(name, payload)
    }

    pub fn fabric_stream_read(
        &mut self,
        name: &str,
        start_sequence: u64,
        limit: usize,
    ) -> io::Result<Vec<FabricStreamRecord>> {
        self.fabric_stream_store_mut()?
            .read_from(name, start_sequence, limit)
    }

    pub fn fabric_stream_read_consumer(
        &mut self,
        name: &str,
        consumer: &str,
        limit: usize,
    ) -> io::Result<Vec<FabricStreamRecord>> {
        self.fabric_stream_store_mut()?
            .read_consumer(name, consumer, limit)
    }

    pub fn fabric_stream_commit_cursor(
        &mut self,
        name: &str,
        consumer: &str,
        sequence: u64,
    ) -> io::Result<()> {
        self.fabric_stream_store_mut()?
            .commit_cursor(name, consumer, sequence)
    }

    pub fn fabric_stream_cursor(&mut self, name: &str, consumer: &str) -> io::Result<u64> {
        self.fabric_stream_store_mut()?.cursor(name, consumer)
    }

    pub fn fabric_stream_info(&mut self, name: &str) -> io::Result<FabricStreamInfo> {
        self.fabric_stream_store_mut()?.stream_info(name)
    }

    pub fn fabric_stream_committed_sequence(&mut self, name: &str) -> io::Result<u64> {
        self.fabric_stream_store_mut()?.committed_sequence(name)
    }

    pub fn fabric_stream_read_committed(
        &mut self,
        name: &str,
        start_sequence: u64,
        limit: usize,
    ) -> io::Result<Vec<FabricStreamRecord>> {
        self.fabric_stream_store_mut()?
            .read_committed(name, start_sequence, limit)
    }

    pub(crate) fn fabric_stream_commit_through(
        &mut self,
        name: &str,
        sequence: u64,
    ) -> io::Result<()> {
        self.fabric_stream_store_mut()?
            .commit_through(name, sequence)
    }

    pub(crate) fn fabric_stream_reserve_replication_intent(
        &mut self,
        name: &str,
        intent: FabricStreamPendingIntent,
    ) -> io::Result<()> {
        self.fabric_stream_store_mut()?
            .reserve_replication_intent(name, intent)
    }

    pub(crate) fn fabric_stream_pending_replication_intents(
        &mut self,
        name: &str,
    ) -> io::Result<Vec<FabricStreamPendingIntent>> {
        self.fabric_stream_store_mut()?
            .pending_replication_intents(name)
    }

    pub(crate) fn fabric_stream_remove_replication_intent(
        &mut self,
        name: &str,
        sequence: u64,
    ) -> io::Result<bool> {
        self.fabric_stream_store_mut()?
            .remove_replication_intent(name, sequence)
    }

    pub(crate) fn fabric_stream_append_reserved_replica(
        &mut self,
        name: &str,
        sequence: u64,
        payload: &[u8],
    ) -> io::Result<()> {
        self.fabric_stream_store_mut()?
            .append_reserved_replica(name, sequence, payload)
    }

    pub fn fabric_stream_config(&mut self, name: &str) -> io::Result<FabricStreamConfig> {
        self.fabric_stream_store_mut()?.stream_config(name)
    }

    fn fabric_stream_store_mut(&mut self) -> io::Result<&mut FileFabricStreamStore> {
        self.distributed.fabric_streams.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream storage is not open; call fabric_stream_open first",
            )
        })
    }
}

fn recover_stream(dir: &Path) -> io::Result<StreamState> {
    let metadata_path = dir.join("meta.json");
    let metadata_bytes = fs::read(&metadata_path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "Fabric stream metadata not found at {}",
                    metadata_path.display()
                ),
            )
        } else {
            error
        }
    })?;
    let metadata: StreamMetadata = serde_json::from_slice(&metadata_bytes).map_err(json_error)?;
    if metadata.version != STREAM_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported Fabric stream metadata version {}",
                metadata.version
            ),
        ));
    }
    let config = metadata.config.validate()?;

    let segments = list_segments(dir)?;
    let mut expected_sequence = 1_u64;
    let mut current_segment_base = 1_u64;
    let mut current_segment_len = 0_u64;

    for (index, (base, path)) in segments.iter().enumerate() {
        if *base != expected_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Fabric segment {} starts at {}, expected {}",
                    path.display(),
                    base,
                    expected_sequence
                ),
            ));
        }
        let repair_tail = index + 1 == segments.len();
        let records = decode_segment(path, *base, repair_tail)?;
        expected_sequence = records
            .last()
            .map(|record| record.sequence.saturating_add(1))
            .unwrap_or(*base);
        current_segment_base = *base;
        current_segment_len = fs::metadata(path)?.len();
    }

    Ok(StreamState {
        config,
        next_sequence: expected_sequence,
        current_segment_base,
        current_segment_len,
    })
}

fn create_segment(path: &Path, base_sequence: u64) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(SEGMENT_MAGIC)?;
    file.write_all(&STREAM_FORMAT_VERSION.to_be_bytes())?;
    file.write_all(&base_sequence.to_be_bytes())?;
    file.flush()?;
    file.sync_data()
}

fn encode_record(sequence: u64, payload: &[u8]) -> Vec<u8> {
    let mut checksum_hasher = blake3::Hasher::new();
    checksum_hasher.update(&sequence.to_be_bytes());
    checksum_hasher.update(payload);
    let checksum = checksum_hasher.finalize();

    let mut frame = Vec::with_capacity(RECORD_HEADER_LEN + payload.len());
    frame.extend_from_slice(&sequence.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&checksum.as_bytes()[..16]);
    frame.extend_from_slice(payload);
    frame
}

fn decode_segment(
    path: &Path,
    expected_base: u64,
    repair_tail: bool,
) -> io::Result<Vec<FabricStreamRecord>> {
    let bytes = fs::read(path)?;
    if bytes.len() < SEGMENT_HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("truncated Fabric segment header: {}", path.display()),
        ));
    }
    if &bytes[..4] != SEGMENT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Fabric segment magic: {}", path.display()),
        ));
    }
    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if version != STREAM_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported Fabric segment version {version}"),
        ));
    }
    let base = u64::from_be_bytes(
        bytes[6..14]
            .try_into()
            .expect("segment header slice has exact base-sequence length"),
    );
    if base != expected_base {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Fabric segment header base {base} disagrees with filename base {expected_base}"
            ),
        ));
    }

    let mut offset = SEGMENT_HEADER_LEN;
    let mut expected_sequence = base;
    let mut records = Vec::new();
    while offset < bytes.len() {
        let record_start = offset;
        if bytes.len() - offset < RECORD_HEADER_LEN {
            if repair_tail {
                truncate_segment(path, record_start)?;
                break;
            }
            return Err(truncated_record_error(path));
        }

        let sequence = u64::from_be_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .expect("record sequence slice has exact length"),
        );
        offset += 8;
        let payload_len = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("record length slice has exact length"),
        ) as usize;
        offset += 4;
        let checksum: [u8; 16] = bytes[offset..offset + 16]
            .try_into()
            .expect("record checksum slice has exact length");
        offset += 16;

        let Some(payload_end) = offset.checked_add(payload_len) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric record length overflow",
            ));
        };
        if payload_end > bytes.len() {
            if repair_tail {
                truncate_segment(path, record_start)?;
                break;
            }
            return Err(truncated_record_error(path));
        }
        if sequence != expected_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Fabric sequence gap in {}: got {}, expected {}",
                    path.display(),
                    sequence,
                    expected_sequence
                ),
            ));
        }

        let payload = &bytes[offset..payload_end];
        let mut hasher = blake3::Hasher::new();
        hasher.update(&sequence.to_be_bytes());
        hasher.update(payload);
        let digest = hasher.finalize();
        if digest.as_bytes()[..16] != checksum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Fabric checksum mismatch at sequence {sequence} in {}",
                    path.display()
                ),
            ));
        }

        records.push(FabricStreamRecord {
            sequence,
            payload: payload.to_vec(),
        });
        expected_sequence = expected_sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Fabric sequence overflow")
        })?;
        offset = payload_end;
    }
    Ok(records)
}

fn truncate_segment(path: &Path, len: usize) -> io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(len as u64)?;
    file.sync_all()
}

fn truncated_record_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("truncated Fabric record in {}", path.display()),
    )
}

fn list_segments(dir: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let mut segments = Vec::new();
    if !dir.exists() {
        return Ok(segments);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("seg") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid segment filename")
            })?;
        let base = stem.parse::<u64>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Fabric segment filename {}", path.display()),
            )
        })?;
        segments.push((base, path));
    }
    segments.sort_by_key(|(base, _)| *base);
    Ok(segments)
}

fn segment_path(dir: &Path, base_sequence: u64) -> PathBuf {
    dir.join(format!("{base_sequence:020}.seg"))
}

fn read_replication(path: &Path) -> io::Result<ReplicationFile> {
    if !path.exists() {
        return Ok(ReplicationFile::default());
    }
    let bytes = fs::read(path)?;
    let replication: ReplicationFile = serde_json::from_slice(&bytes).map_err(json_error)?;
    if replication.version != STREAM_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported Fabric replication metadata version {}",
                replication.version
            ),
        ));
    }
    Ok(replication)
}

fn read_commit(path: &Path) -> io::Result<CommitFile> {
    if !path.exists() {
        return Ok(CommitFile::default());
    }
    let bytes = fs::read(path)?;
    let commit: CommitFile = serde_json::from_slice(&bytes).map_err(json_error)?;
    if commit.version != STREAM_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported Fabric commit version {}", commit.version),
        ));
    }
    Ok(commit)
}

fn read_cursors(path: &Path) -> io::Result<CursorFile> {
    if !path.exists() {
        return Ok(CursorFile::default());
    }
    let bytes = fs::read(path)?;
    let cursors: CursorFile = serde_json::from_slice(&bytes).map_err(json_error)?;
    if cursors.version != STREAM_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported Fabric cursor version {}", cursors.version),
        ));
    }
    Ok(cursors)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(json_error)?;
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("tmp-{}-{counter}", std::process::id()));

    let write_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        if let Some(parent) = path.parent() {
            sync_dir(parent)?;
        }
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn validate_name(kind: &str, name: &str) -> io::Result<()> {
    if name.is_empty() || name.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Fabric {kind} name must contain 1..=128 characters"),
        ));
    }
    if name == "." || name == ".." || name.starts_with('.') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid Fabric {kind} name `{name}`"),
        ));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Fabric {kind} name `{name}` may contain only ASCII letters, digits, '.', '_', and '-'"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nulang-fabric-stream-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn segmented_append_replay_and_rotation() {
        let root = test_dir("rotation");
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream(
                "orders",
                FabricStreamConfig {
                    segment_max_bytes: 96,
                },
            )
            .unwrap();

        assert_eq!(store.append("orders", b"one").unwrap(), 1);
        assert_eq!(store.append("orders", &[2; 48]).unwrap(), 2);
        assert_eq!(store.append("orders", b"three").unwrap(), 3);

        let records = store.read_from("orders", 2, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(records[1].payload, b"three");

        let info = store.stream_info("orders").unwrap();
        assert!(info.segment_count >= 2);
        assert_eq!(info.last_sequence, Some(3));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reopen_preserves_records_and_consumer_cursor() {
        let root = test_dir("reopen");
        {
            let mut store = FileFabricStreamStore::open(&root).unwrap();
            store
                .create_stream("events", FabricStreamConfig::default())
                .unwrap();
            store.append("events", b"a").unwrap();
            store.append("events", b"b").unwrap();
            store.commit_cursor("events", "billing", 1).unwrap();
        }

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        assert_eq!(reopened.cursor("events", "billing").unwrap(), 1);
        let replay = reopened.read_consumer("events", "billing", 10).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].sequence, 2);
        assert_eq!(replay[0].payload, b"b");
        assert_eq!(reopened.append("events", b"c").unwrap(), 3);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_truncates_only_incomplete_final_record() {
        let root = test_dir("tail-repair");
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream("events", FabricStreamConfig::default())
            .unwrap();
        store.append("events", b"a").unwrap();
        store.append("events", b"b").unwrap();

        let segments = list_segments(&root.join("events")).unwrap();
        let last = &segments.last().unwrap().1;
        let mut file = OpenOptions::new().append(true).open(last).unwrap();
        file.write_all(&[0xAA, 0xBB, 0xCC]).unwrap();
        file.sync_all().unwrap();
        drop(file);
        drop(store);

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        assert_eq!(reopened.append("events", b"c").unwrap(), 3);
        let all = reopened.read_from("events", 1, 10).unwrap();
        assert_eq!(
            all.iter().map(|record| record.sequence).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_rejects_checksum_corruption() {
        let root = test_dir("checksum");
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream("events", FabricStreamConfig::default())
            .unwrap();
        store.append("events", b"original").unwrap();
        drop(store);

        let segments = list_segments(&root.join("events")).unwrap();
        let path = &segments[0].1;
        let mut bytes = fs::read(path).unwrap();
        let payload_offset = SEGMENT_HEADER_LEN + RECORD_HEADER_LEN;
        bytes[payload_offset] ^= 0xFF;
        fs::write(path, bytes).unwrap();

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        let error = reopened
            .read_from("events", 1, 10)
            .expect_err("checksum corruption must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("checksum mismatch"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replica_append_is_exact_and_idempotent() {
        let root = test_dir("replica");
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream("events", FabricStreamConfig::default())
            .unwrap();

        assert!(store.append_replica("events", 1, b"a").unwrap());
        assert!(!store.append_replica("events", 1, b"a").unwrap());
        assert!(store.append_replica("events", 1, b"different").is_err());
        assert!(store.append_replica("events", 3, b"gap").is_err());
        assert!(store.append_replica("events", 2, b"b").unwrap());

        let records = store.read_from("events", 1, 10).unwrap();
        assert_eq!(
            records
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cursor_is_monotonic_and_bounded_by_tail() {
        let root = test_dir("cursor");
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream("events", FabricStreamConfig::default())
            .unwrap();
        store.append("events", b"a").unwrap();
        store.append("events", b"b").unwrap();

        store.commit_cursor("events", "worker", 2).unwrap();
        assert!(store.commit_cursor("events", "worker", 1).is_err());
        assert!(store.commit_cursor("events", "worker", 3).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replication_intent_persists_before_append_and_can_be_removed() {
        let root = test_dir("replication-intent");
        {
            let mut store = FileFabricStreamStore::open(&root).unwrap();
            store
                .create_stream("events", FabricStreamConfig::default())
                .unwrap();
            let intent = FabricStreamPendingIntent {
                partition: 0,
                leader: 10,
                membership_fingerprint: 44,
                replication_factor: 2,
                replicas: vec![10, 11],
                sequence: 1,
            };
            store
                .reserve_replication_intent("events", intent.clone())
                .unwrap();
            assert_eq!(
                store.pending_replication_intents("events").unwrap(),
                vec![intent]
            );
        }

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        assert_eq!(
            reopened
                .pending_replication_intents("events")
                .unwrap()
                .len(),
            1
        );
        assert!(reopened.remove_replication_intent("events", 1).unwrap());
        assert!(reopened
            .pending_replication_intents("events")
            .unwrap()
            .is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reserved_replica_append_requires_matching_durable_intent() {
        let root = test_dir("reserved-append");
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream("events", FabricStreamConfig::default())
            .unwrap();
        assert!(store.append_reserved_replica("events", 1, b"a").is_err());

        store
            .reserve_replication_intent(
                "events",
                FabricStreamPendingIntent {
                    partition: 0,
                    leader: 10,
                    membership_fingerprint: 44,
                    replication_factor: 2,
                    replicas: vec![10, 11],
                    sequence: 1,
                },
            )
            .unwrap();
        store.append_reserved_replica("events", 1, b"a").unwrap();
        assert_eq!(store.read_from("events", 1, 10).unwrap().len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn committed_boundary_is_persisted_and_hides_uncommitted_tail() {
        let root = test_dir("commit-boundary");
        {
            let mut store = FileFabricStreamStore::open(&root).unwrap();
            store
                .create_stream("events", FabricStreamConfig::default())
                .unwrap();
            store.append("events", b"one").unwrap();
            store.append("events", b"two").unwrap();
            store.commit_through("events", 1).unwrap();

            let committed = store.read_committed("events", 1, 10).unwrap();
            assert_eq!(committed.len(), 1);
            assert_eq!(committed[0].sequence, 1);
        }

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        assert_eq!(reopened.committed_sequence("events").unwrap(), 1);
        let committed = reopened.read_committed("events", 1, 10).unwrap();
        assert_eq!(committed.len(), 1);
        assert_eq!(reopened.read_from("events", 1, 10).unwrap().len(), 2);
        assert!(reopened.commit_through("events", 3).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_stream_api_survives_reopen() {
        let root = test_dir("runtime");
        let mut runtime = Runtime::new();
        runtime.fabric_stream_open(&root).unwrap();
        runtime
            .fabric_stream_create("audit", FabricStreamConfig::default())
            .unwrap();
        assert_eq!(runtime.fabric_stream_append("audit", b"first").unwrap(), 1);
        runtime
            .fabric_stream_commit_cursor("audit", "consumer-a", 1)
            .unwrap();

        let mut restarted = Runtime::new();
        restarted.fabric_stream_open(&root).unwrap();
        assert_eq!(
            restarted
                .fabric_stream_cursor("audit", "consumer-a")
                .unwrap(),
            1
        );
        assert_eq!(
            restarted.fabric_stream_append("audit", b"second").unwrap(),
            2
        );
        let _ = fs::remove_dir_all(root);
    }
}
