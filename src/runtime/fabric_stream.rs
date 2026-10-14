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

pub(crate) const FABRIC_STREAM_INITIAL_EPOCH: u64 = 1;

fn initial_stream_epoch() -> u64 {
    FABRIC_STREAM_INITIAL_EPOCH
}

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

/// A read-only estimate of whole log segments that *could* be trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricStreamRetentionPreview {
    pub stream: String,
    pub requested_keep_from_sequence: u64,
    pub first_retained_sequence: Option<u64>,
    pub eligible_segment_count: usize,
    pub eligible_bytes: u64,
    /// Lowest named consumer cursor, if any.
    pub minimum_consumer_cursor: Option<u64>,
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

/// Durable logical trim floor, fsynced before deletion of obsolete files.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RetentionFile {
    version: u16,
    first_retained_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitFile {
    version: u16,
    committed_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamReplicationPolicy {
    pub partition: u16,
    #[serde(default = "initial_stream_epoch")]
    pub epoch: u64,
    pub leader: u64,
    pub membership_fingerprint: u64,
    pub replication_factor: usize,
    pub replicas: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplicationPolicyFile {
    version: u16,
    policy: FabricStreamReplicationPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochProposalState {
    pub proposal_hash: String,
    pub from_policy: FabricStreamReplicationPolicy,
    pub to_policy: FabricStreamReplicationPolicy,
    pub candidate_tail: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochVoteState {
    pub voter: u64,
    pub epoch: u64,
    pub proposal_hash: String,
    pub tail: u64,
    pub committed_sequence: u64,
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochTransitionState {
    pub proposal: FabricStreamEpochProposalState,
    pub votes: BTreeMap<u64, FabricStreamEpochVoteState>,
    pub finalized: bool,
    pub quorum_committed_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochPromise {
    pub epoch: u64,
    pub proposal_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EpochPromiseFile {
    version: u16,
    promise: FabricStreamEpochPromise,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EpochTransitionFile {
    version: u16,
    transition: FabricStreamEpochTransitionState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamPendingIntent {
    pub partition: u16,
    #[serde(default = "initial_stream_epoch")]
    pub epoch: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplicaProgressFile {
    version: u16,
    replicas: BTreeMap<u64, u64>,
}

impl Default for ReplicaProgressFile {
    fn default() -> Self {
        Self {
            version: STREAM_FORMAT_VERSION,
            replicas: BTreeMap::new(),
        }
    }
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

    pub fn stream_names(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || !entry.path().join("meta.json").is_file() {
                continue;
            }
            let name = entry.file_name().into_string().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric stream directory name is not valid UTF-8",
                )
            })?;
            validate_name("stream", &name)?;
            names.push(name);
        }
        names.sort();
        Ok(names)
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

    /// Preview whole-segment retention eligibility without deleting anything.
    ///
    /// Deliberately standalone-only: replicated retention needs quorum and
    /// follower catch-up proofs. Existing consumer cursors and producer state
    /// must not be silently invalidated by physical segment removal.
    pub fn preview_retention(
        &mut self,
        name: &str,
        keep_from_sequence: u64,
    ) -> io::Result<FabricStreamRetentionPreview> {
        self.ensure_state(name)?;
        let next_sequence = self
            .states
            .get(name)
            .expect("state recovered")
            .next_sequence;
        let dir = self.stream_dir(name);
        let floor = read_retention_floor(&dir.join("retention.json"))?;
        if keep_from_sequence < floor || keep_from_sequence > next_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Fabric keep-from sequence must be in {floor}..={next_sequence}"),
            ));
        }
        if self.replication_policy(name)?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "replicated Fabric retention needs a quorum-safe trim frontier",
            ));
        }
        // Local idempotent producer checkpoints refer to exact log records.
        // Do not claim an eligibility proof until their migration/expiry
        // semantics have been implemented alongside retention.
        if dir.join("producer_state.json").exists() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Fabric retention cannot discard local producer checkpoint history",
            ));
        }

        let cursors = read_cursors(&dir.join("cursors.json"))?;
        let tail = next_sequence.saturating_sub(1);
        if cursors.cursors.values().any(|&cursor| cursor > tail) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric retention preview found a consumer cursor beyond the local tail",
            ));
        }
        let minimum_consumer_cursor = cursors.cursors.values().copied().min();
        if let Some(minimum) = minimum_consumer_cursor {
            if minimum < floor.saturating_sub(1) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric consumer cursor predates committed retention floor",
                ));
            }
            if keep_from_sequence.saturating_sub(1) > minimum {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("Fabric retention would pass consumer cursor {minimum}"),
                ));
            }
        }

        let segments: Vec<_> = list_segments(&dir)?
            .into_iter()
            .filter(|(base, _)| *base >= floor)
            .collect();
        if floor > 1 && segments.first().map(|(base, _)| *base) != Some(floor) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric retention floor has no matching retained segment",
            ));
        }
        let mut eligible_segment_count = 0;
        let mut eligible_bytes = 0u64;
        // Always retain the last segment. The current append state may still
        // reference it even if all of its records precede the requested floor.
        for index in 0..segments.len().saturating_sub(1) {
            let (base, path) = &segments[index];
            let next_base = segments[index + 1].0;
            if next_base > keep_from_sequence {
                break;
            }
            let records = decode_segment(path, *base, false)?;
            if records.last().map(|record| record.sequence) != next_base.checked_sub(1) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric retention preview found an invalid segment boundary",
                ));
            }
            let bytes = fs::metadata(path)?.len();
            eligible_bytes = eligible_bytes.checked_add(bytes).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "Fabric retention byte count overflow")
            })?;
            eligible_segment_count += 1;
        }
        Ok(FabricStreamRetentionPreview {
            stream: name.to_string(),
            requested_keep_from_sequence: keep_from_sequence,
            first_retained_sequence: segments
                .get(eligible_segment_count)
                .map(|(base, _)| *base),
            eligible_segment_count,
            eligible_bytes,
            minimum_consumer_cursor,
        })
    }

    /// Persist a logical trim fence before deleting standalone log segments.
    /// Retrying after an interrupted cleanup removes the obsolete prefix.
    /// Requires an exclusive stream-root lock (provided by the separate
    /// single-writer ownership PR) before safe production use.
    pub fn apply_retention(
        &mut self,
        name: &str,
        keep_from_sequence: u64,
    ) -> io::Result<FabricStreamRetentionPreview> {
        let preview = self.preview_retention(name, keep_from_sequence)?;
        let dir = self.stream_dir(name);
        let previous_floor = read_retention_floor(&dir.join("retention.json"))?;
        let new_floor = preview.first_retained_sequence.unwrap_or(previous_floor);
        if new_floor < previous_floor {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric retention floor cannot move backwards",
            ));
        }
        if new_floor > previous_floor {
            // write_json_atomic includes file fsync, rename, and directory fsync.
            // This MUST precede every physical unlink.
            write_json_atomic(
                &dir.join("retention.json"),
                &RetentionFile {
                    version: STREAM_FORMAT_VERSION,
                    first_retained_sequence: new_floor,
                },
            )?;
        }

        let mut removed = false;
        for (base, path) in list_segments(&dir)? {
            if base < new_floor {
                fs::remove_file(path)?;
                removed = true;
            }
        }
        if removed {
            sync_dir(&dir)?;
        }
        Ok(preview)
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
        let dir = self.stream_dir(name);
        let floor = read_retention_floor(&dir.join("retention.json"))?;
        if start_sequence < floor {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Fabric sequence {start_sequence} is below retained floor {floor}"),
            ));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut result = Vec::with_capacity(limit.min(256));
        for (base, path) in list_segments(&dir)? {
            if base < floor {
                continue;
            }
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

    pub(crate) fn replication_policy(
        &mut self,
        name: &str,
    ) -> io::Result<Option<FabricStreamReplicationPolicy>> {
        self.ensure_state(name)?;
        read_replication_policy(&self.stream_dir(name).join("replication_policy.json"))
    }

    /// Atomically establish the stream's durable replication policy.
    ///
    /// This layer only establishes epoch 1. Any different existing policy
    /// fails closed until a future quorum-backed epoch transition protocol
    /// explicitly replaces it.
    pub(crate) fn establish_replication_policy(
        &mut self,
        name: &str,
        policy: FabricStreamReplicationPolicy,
    ) -> io::Result<FabricStreamReplicationPolicy> {
        self.ensure_state(name)?;
        if policy.epoch == 0
            || policy.replication_factor == 0
            || policy.replicas.len() != policy.replication_factor
            || !policy.replicas.contains(&policy.leader)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Fabric stream replication policy",
            ));
        }

        if read_retention_floor(&self.stream_dir(name).join("retention.json"))? > 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Fabric physical retention requires migration before enabling replication",
            ));
        }
        let path = self.stream_dir(name).join("replication_policy.json");
        if let Some(existing) = read_replication_policy(&path)? {
            if existing == policy {
                return Ok(existing);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Fabric stream replication policy conflict: durable epoch {} cannot be reinterpreted as epoch {}",
                    existing.epoch, policy.epoch
                ),
            ));
        }

        let file = ReplicationPolicyFile {
            version: STREAM_FORMAT_VERSION,
            policy: policy.clone(),
        };
        write_json_atomic(&path, &file)?;
        sync_dir(&self.stream_dir(name))?;
        Ok(policy)
    }

    pub(crate) fn epoch_promise(
        &mut self,
        name: &str,
    ) -> io::Result<Option<FabricStreamEpochPromise>> {
        self.ensure_state(name)?;
        read_epoch_promise(&self.stream_dir(name).join("epoch_promise.json"))
    }

    /// Durably promise not to accept an older stream epoch.
    ///
    /// Repeating the same proposal is idempotent. A conflicting proposal for
    /// the same epoch, a stale epoch, or skipping directly past the next epoch
    /// is rejected.
    pub(crate) fn promise_epoch(
        &mut self,
        name: &str,
        epoch: u64,
        proposal_hash: &str,
    ) -> io::Result<FabricStreamEpochPromise> {
        self.ensure_state(name)?;
        if proposal_hash.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric epoch proposal hash cannot be empty",
            ));
        }
        let policy = self.replication_policy(name)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Fabric replication policy is not established",
            )
        })?;
        if epoch <= policy.epoch {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Fabric epoch promise {epoch} must be greater than installed epoch {}",
                    policy.epoch
                ),
            ));
        }

        let path = self.stream_dir(name).join("epoch_promise.json");
        if let Some(existing) = read_epoch_promise(&path)? {
            if existing.epoch == epoch && existing.proposal_hash == proposal_hash {
                return Ok(existing);
            }
            if existing.epoch >= epoch {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "Fabric stream already promised epoch {} to a different or newer proposal",
                        existing.epoch
                    ),
                ));
            }
            // A strictly higher term supersedes an abandoned lower promise.
            // The higher promise fences every older term immediately.
        }

        let promise = FabricStreamEpochPromise {
            epoch,
            proposal_hash: proposal_hash.to_string(),
        };
        write_json_atomic(
            &path,
            &EpochPromiseFile {
                version: STREAM_FORMAT_VERSION,
                promise: promise.clone(),
            },
        )?;
        sync_dir(&self.stream_dir(name))?;
        Ok(promise)
    }

    pub(crate) fn begin_epoch_transition(
        &mut self,
        name: &str,
        proposal: FabricStreamEpochProposalState,
    ) -> io::Result<FabricStreamEpochTransitionState> {
        self.ensure_state(name)?;
        if proposal.proposal_hash.is_empty()
            || proposal.to_policy.epoch <= proposal.from_policy.epoch
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Fabric epoch transition proposal",
            ));
        }
        let current = self.replication_policy(name)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Fabric replication policy is not established",
            )
        })?;
        if current != proposal.from_policy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch transition source policy does not match durable policy",
            ));
        }

        let path = self.stream_dir(name).join("epoch_transition.json");
        if let Some(existing) = read_epoch_transition(&path)? {
            if existing.proposal == proposal {
                return Ok(existing);
            }
            let supersedes_abandoned = !existing.finalized
                && existing.proposal.from_policy == proposal.from_policy
                && proposal.to_policy.epoch > existing.proposal.to_policy.epoch;
            let follows_finalized =
                existing.finalized && existing.proposal.to_policy == proposal.from_policy;
            if !supersedes_abandoned && !follows_finalized {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a different Fabric epoch transition cannot be superseded by this proposal",
                ));
            }
            // Higher terms may supersede an abandoned proposal; finalized
            // transitions roll forward only from the installed to_policy.
        }

        let state = FabricStreamEpochTransitionState {
            proposal,
            votes: BTreeMap::new(),
            finalized: false,
            quorum_committed_sequence: 0,
        };
        write_json_atomic(
            &path,
            &EpochTransitionFile {
                version: STREAM_FORMAT_VERSION,
                transition: state.clone(),
            },
        )?;
        sync_dir(&self.stream_dir(name))?;
        Ok(state)
    }

    pub(crate) fn epoch_transition(
        &mut self,
        name: &str,
    ) -> io::Result<Option<FabricStreamEpochTransitionState>> {
        self.ensure_state(name)?;
        read_epoch_transition(&self.stream_dir(name).join("epoch_transition.json"))
    }

    pub(crate) fn record_epoch_vote(
        &mut self,
        name: &str,
        vote: FabricStreamEpochVoteState,
    ) -> io::Result<FabricStreamEpochTransitionState> {
        self.ensure_state(name)?;
        let path = self.stream_dir(name).join("epoch_transition.json");
        let mut transition = read_epoch_transition(&path)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Fabric epoch transition is not in progress",
            )
        })?;
        if transition.finalized {
            return Ok(transition);
        }
        if vote.epoch != transition.proposal.to_policy.epoch
            || vote.proposal_hash != transition.proposal.proposal_hash
            || !transition
                .proposal
                .from_policy
                .replicas
                .contains(&vote.voter)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch vote does not match the active proposal",
            ));
        }
        if let Some(existing) = transition.votes.get(&vote.voter) {
            if existing == &vote {
                return Ok(transition);
            }
            if existing.accepted {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "affirmative Fabric epoch vote is immutable",
                ));
            }
            if existing.epoch != vote.epoch
                || existing.proposal_hash != vote.proposal_hash
                || existing.voter != vote.voter
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Fabric replica vote conflicts with the active epoch proposal",
                ));
            }
            // A prior rejection may be replaced after the replica catches up
            // to the exact candidate tail.
        }
        transition.votes.insert(vote.voter, vote);
        write_json_atomic(
            &path,
            &EpochTransitionFile {
                version: STREAM_FORMAT_VERSION,
                transition: transition.clone(),
            },
        )?;
        sync_dir(&self.stream_dir(name))?;
        Ok(transition)
    }

    pub(crate) fn finalize_epoch_transition(
        &mut self,
        name: &str,
        quorum_committed_sequence: u64,
    ) -> io::Result<FabricStreamEpochTransitionState> {
        self.ensure_state(name)?;
        let path = self.stream_dir(name).join("epoch_transition.json");
        let mut transition = read_epoch_transition(&path)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Fabric epoch transition is not in progress",
            )
        })?;
        if transition.finalized {
            if transition.quorum_committed_sequence == quorum_committed_sequence {
                return Ok(transition);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric finalized epoch transition has a different committed boundary",
            ));
        }
        transition.finalized = true;
        transition.quorum_committed_sequence = quorum_committed_sequence;
        write_json_atomic(
            &path,
            &EpochTransitionFile {
                version: STREAM_FORMAT_VERSION,
                transition: transition.clone(),
            },
        )?;
        sync_dir(&self.stream_dir(name))?;
        Ok(transition)
    }

    /// Atomically replace the durable replication policy after a matching
    /// promise has fenced the old epoch.
    pub(crate) fn install_epoch_policy(
        &mut self,
        name: &str,
        from_policy: &FabricStreamReplicationPolicy,
        to_policy: &FabricStreamReplicationPolicy,
        proposal_hash: &str,
    ) -> io::Result<()> {
        self.ensure_state(name)?;
        if to_policy.epoch <= from_policy.epoch {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric epoch policy must advance to a higher term",
            ));
        }

        let current = self.replication_policy(name)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Fabric replication policy is not established",
            )
        })?;
        if current == *to_policy {
            return Ok(());
        }
        if current != *from_policy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch transition source policy changed",
            ));
        }
        let promise = self.epoch_promise(name)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric epoch policy cannot advance without a durable promise",
            )
        })?;
        if promise.epoch != to_policy.epoch || promise.proposal_hash != proposal_hash {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric epoch policy does not match the durable promise",
            ));
        }

        write_json_atomic(
            &self.stream_dir(name).join("replication_policy.json"),
            &ReplicationPolicyFile {
                version: STREAM_FORMAT_VERSION,
                policy: to_policy.clone(),
            },
        )?;
        sync_dir(&self.stream_dir(name))
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
        if intent.epoch == 0
            || intent.replication_factor == 0
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
        Ok(
            read_replication(&self.stream_dir(name).join("replication.json"))?
                .pending
                .into_values()
                .collect(),
        )
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

    pub(crate) fn replica_progress(&mut self, name: &str, replica: u64) -> io::Result<u64> {
        self.ensure_state(name)?;
        Ok(
            read_replica_progress(&self.stream_dir(name).join("replica_progress.json"))?
                .replicas
                .get(&replica)
                .copied()
                .unwrap_or(0),
        )
    }

    pub(crate) fn record_replica_progress(
        &mut self,
        name: &str,
        replica: u64,
        sequence: u64,
    ) -> io::Result<()> {
        self.ensure_state(name)?;
        if sequence == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric replica progress sequence must be non-zero",
            ));
        }
        let path = self.stream_dir(name).join("replica_progress.json");
        let mut progress = read_replica_progress(&path)?;
        let current = progress.replicas.get(&replica).copied().unwrap_or(0);
        if sequence <= current {
            return Ok(());
        }
        progress.replicas.insert(replica, sequence);
        write_json_atomic(&path, &progress)?;
        sync_dir(&self.stream_dir(name))
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

        let floor = read_retention_floor(&self.stream_dir(name).join("retention.json"))?;
        if sequence < floor.saturating_sub(1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric cursor cannot commit before retained history",
            ));
        }
        let path = self.stream_dir(name).join("cursors.json");
        let mut cursors = read_cursors(&path)?;
        if cursors.cursors.get(consumer).is_some_and(|&cursor| cursor < floor.saturating_sub(1)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric persisted consumer cursor predates retained history",
            ));
        }
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

    /// Read-only standalone stream retention estimate; never deletes records.
    pub fn fabric_stream_preview_retention(
        &mut self,
        name: &str,
        keep_from_sequence: u64,
    ) -> io::Result<FabricStreamRetentionPreview> {
        self.fabric_stream_store_mut()?
            .preview_retention(name, keep_from_sequence)
    }

    /// Experimental standalone-only crash-fenced physical retention.
    pub fn fabric_stream_apply_retention(
        &mut self,
        name: &str,
        keep_from_sequence: u64,
    ) -> io::Result<FabricStreamRetentionPreview> {
        self.fabric_stream_store_mut()?
            .apply_retention(name, keep_from_sequence)
    }

    pub fn fabric_stream_names(&mut self) -> io::Result<Vec<String>> {
        self.fabric_stream_store_mut()?.stream_names()
    }

    pub fn fabric_stream_committed_sequence(&mut self, name: &str) -> io::Result<u64> {
        self.fabric_stream_store_mut()?.committed_sequence(name)
    }

    /// Return the durable replication epoch, if replication policy has been established.
    pub fn fabric_stream_epoch(&mut self, name: &str) -> io::Result<Option<u64>> {
        Ok(self
            .fabric_stream_store_mut()?
            .replication_policy(name)?
            .map(|policy| policy.epoch))
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

    pub(crate) fn fabric_stream_replication_policy(
        &mut self,
        name: &str,
    ) -> io::Result<Option<FabricStreamReplicationPolicy>> {
        self.fabric_stream_store_mut()?.replication_policy(name)
    }

    pub(crate) fn fabric_stream_establish_replication_policy(
        &mut self,
        name: &str,
        policy: FabricStreamReplicationPolicy,
    ) -> io::Result<FabricStreamReplicationPolicy> {
        self.fabric_stream_store_mut()?
            .establish_replication_policy(name, policy)
    }

    pub(crate) fn fabric_stream_epoch_promise(
        &mut self,
        name: &str,
    ) -> io::Result<Option<FabricStreamEpochPromise>> {
        self.fabric_stream_store_mut()?.epoch_promise(name)
    }

    pub(crate) fn fabric_stream_promise_epoch(
        &mut self,
        name: &str,
        epoch: u64,
        proposal_hash: &str,
    ) -> io::Result<FabricStreamEpochPromise> {
        self.fabric_stream_store_mut()?
            .promise_epoch(name, epoch, proposal_hash)
    }

    pub(crate) fn fabric_stream_begin_epoch_transition_state(
        &mut self,
        name: &str,
        proposal: FabricStreamEpochProposalState,
    ) -> io::Result<FabricStreamEpochTransitionState> {
        self.fabric_stream_store_mut()?
            .begin_epoch_transition(name, proposal)
    }

    pub(crate) fn fabric_stream_epoch_transition_state(
        &mut self,
        name: &str,
    ) -> io::Result<Option<FabricStreamEpochTransitionState>> {
        self.fabric_stream_store_mut()?.epoch_transition(name)
    }

    pub(crate) fn fabric_stream_record_epoch_vote(
        &mut self,
        name: &str,
        vote: FabricStreamEpochVoteState,
    ) -> io::Result<FabricStreamEpochTransitionState> {
        self.fabric_stream_store_mut()?
            .record_epoch_vote(name, vote)
    }

    pub(crate) fn fabric_stream_finalize_epoch_transition_state(
        &mut self,
        name: &str,
        quorum_committed_sequence: u64,
    ) -> io::Result<FabricStreamEpochTransitionState> {
        self.fabric_stream_store_mut()?
            .finalize_epoch_transition(name, quorum_committed_sequence)
    }

    pub(crate) fn fabric_stream_install_epoch_policy(
        &mut self,
        name: &str,
        from_policy: &FabricStreamReplicationPolicy,
        to_policy: &FabricStreamReplicationPolicy,
        proposal_hash: &str,
    ) -> io::Result<()> {
        self.fabric_stream_store_mut()?.install_epoch_policy(
            name,
            from_policy,
            to_policy,
            proposal_hash,
        )
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

    pub(crate) fn fabric_stream_apply_transition_repair_record(
        &mut self,
        name: &str,
        sequence: u64,
        payload: &[u8],
    ) -> io::Result<()> {
        let appended = self
            .fabric_stream_store_mut()?
            .append_replica(name, sequence, payload)?;
        if !appended {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Fabric epoch repair record already exists",
            ));
        }
        Ok(())
    }

    pub(crate) fn fabric_stream_replica_progress(
        &mut self,
        name: &str,
        replica: u64,
    ) -> io::Result<u64> {
        self.fabric_stream_store_mut()?
            .replica_progress(name, replica)
    }

    pub(crate) fn fabric_stream_record_replica_progress(
        &mut self,
        name: &str,
        replica: u64,
        sequence: u64,
    ) -> io::Result<()> {
        self.fabric_stream_store_mut()?
            .record_replica_progress(name, replica, sequence)
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

    let floor = read_retention_floor(&dir.join("retention.json"))?;
    let segments: Vec<_> = list_segments(dir)?
        .into_iter()
        .filter(|(base, _)| *base >= floor)
        .collect();
    if floor > 1 && segments.first().map(|(base, _)| *base) != Some(floor) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Fabric retained log suffix missing at durable retention floor",
        ));
    }
    let mut expected_sequence = floor;
    let mut current_segment_base = floor;
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

fn read_epoch_promise(path: &Path) -> io::Result<Option<FabricStreamEpochPromise>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    let file: EpochPromiseFile = serde_json::from_slice(&bytes).map_err(json_error)?;
    if file.version != STREAM_FORMAT_VERSION
        || file.promise.epoch == 0
        || file.promise.proposal_hash.is_empty()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid durable Fabric epoch promise",
        ));
    }
    Ok(Some(file.promise))
}

fn read_epoch_transition(path: &Path) -> io::Result<Option<FabricStreamEpochTransitionState>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    let file: EpochTransitionFile = serde_json::from_slice(&bytes).map_err(json_error)?;
    if file.version != STREAM_FORMAT_VERSION
        || file.transition.proposal.proposal_hash.is_empty()
        || file.transition.proposal.to_policy.epoch == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid durable Fabric epoch transition state",
        ));
    }
    Ok(Some(file.transition))
}

fn read_replication_policy(path: &Path) -> io::Result<Option<FabricStreamReplicationPolicy>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    let file: ReplicationPolicyFile = serde_json::from_slice(&bytes).map_err(json_error)?;
    if file.version != STREAM_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported Fabric replication policy version {}",
                file.version
            ),
        ));
    }
    if file.policy.epoch == 0
        || file.policy.replication_factor == 0
        || file.policy.replicas.len() != file.policy.replication_factor
        || !file.policy.replicas.contains(&file.policy.leader)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid durable Fabric replication policy",
        ));
    }
    Ok(Some(file.policy))
}

fn read_replica_progress(path: &Path) -> io::Result<ReplicaProgressFile> {
    if !path.exists() {
        return Ok(ReplicaProgressFile::default());
    }
    let bytes = fs::read(path)?;
    let progress: ReplicaProgressFile = serde_json::from_slice(&bytes).map_err(json_error)?;
    if progress.version != STREAM_FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported Fabric replica progress version {}",
                progress.version
            ),
        ));
    }
    Ok(progress)
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

fn read_retention_floor(path: &Path) -> io::Result<u64> {
    if !path.exists() {
        return Ok(1);
    }
    let bytes = fs::read(path)?;
    let file: RetentionFile = serde_json::from_slice(&bytes).map_err(json_error)?;
    if file.version != STREAM_FORMAT_VERSION || file.first_retained_sequence == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid durable Fabric retention floor",
        ));
    }
    Ok(file.first_retained_sequence)
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
    fn retention_preview_only_plans_complete_segments_with_consumers_caught_up() {
        let root = test_dir("retention-preview");
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream(
                "events",
                FabricStreamConfig {
                    segment_max_bytes: 96,
                },
            )
            .unwrap();
        for payload in [b'a', b'b', b'c', b'd'] {
            store.append("events", &[payload; 48]).unwrap();
        }
        store.commit_cursor("events", "worker", 3).unwrap();
        let before = store.read_from("events", 1, 10).unwrap();
        let plan = store.preview_retention("events", 4).unwrap();
        assert_eq!(plan.stream, "events");
        assert_eq!(plan.requested_keep_from_sequence, 4);
        assert_eq!(plan.first_retained_sequence, Some(4));
        assert_eq!(plan.eligible_segment_count, 3);
        assert!(plan.eligible_bytes > 0);
        assert_eq!(plan.minimum_consumer_cursor, Some(3));
        assert_eq!(store.read_from("events", 1, 10).unwrap(), before);
        assert_eq!(store.preview_retention("events", 1).unwrap().eligible_segment_count, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retention_preview_fails_closed_when_consumer_or_replication_is_unsafe() {
        let root = test_dir("retention-unsafe");
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream(
                "events",
                FabricStreamConfig {
                    segment_max_bytes: 96,
                },
            )
            .unwrap();
        for _ in 0..3 {
            store.append("events", &[1; 48]).unwrap();
        }
        store.commit_cursor("events", "worker", 1).unwrap();
        assert_eq!(
            store.preview_retention("events", 3).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            store.preview_retention("events", 0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            store.preview_retention("events", 5).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        // A corrupted cursor beyond the local tail must not be treated as
        // proof that discarded records were fully processed.
        write_json_atomic(
            &root.join("events").join("cursors.json"),
            &CursorFile {
                version: STREAM_FORMAT_VERSION,
                cursors: BTreeMap::from([("worker".to_string(), 99)]),
            },
        )
        .unwrap();
        assert_eq!(
            store.preview_retention("events", 2).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        store
            .establish_replication_policy(
                "events",
                FabricStreamReplicationPolicy {
                    partition: 0,
                    epoch: FABRIC_STREAM_INITIAL_EPOCH,
                    leader: 10,
                    membership_fingerprint: 44,
                    replication_factor: 2,
                    replicas: vec![10, 11],
                },
            )
            .unwrap();
        assert_eq!(
            store.preview_retention("events", 1).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn physical_retention_survives_reopen_and_preserves_absolute_sequences() {
        let root = test_dir("retention-physical-reopen");
        {
            let mut store = FileFabricStreamStore::open(&root).unwrap();
            store
                .create_stream(
                    "events",
                    FabricStreamConfig {
                        segment_max_bytes: 96,
                    },
                )
                .unwrap();
            for value in 1..=4 {
                store.append("events", &[value; 48]).unwrap();
            }
            store.commit_cursor("events", "worker", 3).unwrap();
            let applied = store.apply_retention("events", 4).unwrap();
            assert_eq!(applied.eligible_segment_count, 3);
            assert_eq!(applied.first_retained_sequence, Some(4));
            assert!(store.read_from("events", 1, 1).is_err());
            assert_eq!(store.read_from("events", 4, 10).unwrap()[0].sequence, 4);
            assert_eq!(list_segments(&root.join("events")).unwrap().len(), 1);
        }

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        assert_eq!(reopened.read_from("events", 4, 10).unwrap().len(), 1);
        assert_eq!(reopened.append("events", b"after-retention").unwrap(), 5);
        assert_eq!(reopened.cursor("events", "worker").unwrap(), 3);
        assert_eq!(reopened.read_consumer("events", "worker", 10).unwrap().len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retention_floor_committed_before_deletion_replays_only_live_suffix() {
        let root = test_dir("retention-interrupted-deletion");
        {
            let mut store = FileFabricStreamStore::open(&root).unwrap();
            store
                .create_stream(
                    "events",
                    FabricStreamConfig {
                        segment_max_bytes: 96,
                    },
                )
                .unwrap();
            for _ in 0..3 {
                store.append("events", &[7; 48]).unwrap();
            }
            store.commit_cursor("events", "worker", 2).unwrap();
            // Simulate a crash after the durable trim floor is persisted but
            // before the obsolete segment files have been removed.
            write_json_atomic(
                &root.join("events").join("retention.json"),
                &RetentionFile {
                    version: STREAM_FORMAT_VERSION,
                    first_retained_sequence: 3,
                },
            )
            .unwrap();
        }
        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        assert_eq!(reopened.read_from("events", 1, 1).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(reopened.read_from("events", 3, 10).unwrap().len(), 1);
        assert_eq!(reopened.append("events", b"four").unwrap(), 4);
        assert_eq!(reopened.apply_retention("events", 3).unwrap().eligible_segment_count, 0);
        assert_eq!(list_segments(&root.join("events")).unwrap().len(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retention_requires_explicit_cursor_start_for_new_consumers() {
        let root = test_dir("retention-new-consumer");
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream(
                "events",
                FabricStreamConfig {
                    segment_max_bytes: 96,
                },
            )
            .unwrap();
        for _ in 0..3 {
            store.append("events", &[5; 48]).unwrap();
        }
        store.commit_cursor("events", "existing", 2).unwrap();
        store.apply_retention("events", 3).unwrap();
        assert_eq!(
            store.read_consumer("events", "new-consumer", 10).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(store.commit_cursor("events", "new-consumer", 1).is_err());
        store.commit_cursor("events", "new-consumer", 2).unwrap();
        assert_eq!(store.read_consumer("events", "new-consumer", 10).unwrap()[0].sequence, 3);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retention_fails_closed_if_committed_suffix_is_missing() {
        let root = test_dir("retention-missing-suffix");
        {
            let mut store = FileFabricStreamStore::open(&root).unwrap();
            store
                .create_stream(
                    "events",
                    FabricStreamConfig {
                        segment_max_bytes: 96,
                    },
                )
                .unwrap();
            for _ in 0..3 {
                store.append("events", &[9; 48]).unwrap();
            }
            store.commit_cursor("events", "worker", 2).unwrap();
            store.apply_retention("events", 3).unwrap();
        }
        let retained = segment_path(&root.join("events"), 3);
        fs::remove_file(retained).unwrap();
        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        assert_eq!(
            reopened.read_from("events", 3, 1).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runtime_retention_preview_does_not_mutate_replay_or_cursors() {
        let root = test_dir("runtime-retention-preview");
        let mut runtime = Runtime::new();
        runtime.fabric_stream_open(&root).unwrap();
        runtime
            .fabric_stream_create(
                "audit",
                FabricStreamConfig {
                    segment_max_bytes: 96,
                },
            )
            .unwrap();
        runtime.fabric_stream_append("audit", &[1; 48]).unwrap();
        runtime.fabric_stream_append("audit", &[2; 48]).unwrap();
        runtime.fabric_stream_commit_cursor("audit", "consumer", 1).unwrap();

        let before = runtime.fabric_stream_read("audit", 1, 10).unwrap();
        let preview = runtime.fabric_stream_preview_retention("audit", 2).unwrap();
        assert_eq!(preview.eligible_segment_count, 1);
        assert_eq!(runtime.fabric_stream_read("audit", 1, 10).unwrap(), before);
        assert_eq!(runtime.fabric_stream_cursor("audit", "consumer").unwrap(), 1);
        drop(runtime);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retention_preview_never_approves_partial_last_segment_or_producer_state() {
        let root = test_dir("retention-partial");
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream("events", FabricStreamConfig::default())
            .unwrap();
        for _ in 0..3 {
            store.append("events", b"x").unwrap();
        }
        store.commit_cursor("events", "worker", 3).unwrap();
        let plan = store.preview_retention("events", 4).unwrap();
        assert_eq!(plan.eligible_segment_count, 0);
        assert_eq!(plan.first_retained_sequence, Some(1));

        // Retention must not destroy records protected by producer checkpoints
        // introduced on a separate draft branch.
        fs::write(root.join("events").join("producer_state.json"), b"{}").unwrap();
        assert_eq!(
            store.preview_retention("events", 3).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn epoch_promise_is_durable_and_single_proposal_per_epoch() {
        let root = test_dir("epoch-promise");
        let policy = FabricStreamReplicationPolicy {
            partition: 0,
            epoch: FABRIC_STREAM_INITIAL_EPOCH,
            leader: 10,
            membership_fingerprint: 44,
            replication_factor: 2,
            replicas: vec![10, 11],
        };
        {
            let mut store = FileFabricStreamStore::open(&root).unwrap();
            store
                .create_stream("events", FabricStreamConfig::default())
                .unwrap();
            store
                .establish_replication_policy("events", policy)
                .unwrap();
            let promised = store.promise_epoch("events", 2, "proposal-a").unwrap();
            assert_eq!(promised.epoch, 2);
            assert_eq!(
                store.promise_epoch("events", 2, "proposal-a").unwrap(),
                promised
            );
            assert!(store.promise_epoch("events", 2, "proposal-b").is_err());
            let superseding = store.promise_epoch("events", 3, "proposal-c").unwrap();
            assert_eq!(superseding.epoch, 3);
        }

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        let promise = reopened.epoch_promise("events").unwrap().unwrap();
        assert_eq!(promise.epoch, 3);
        assert_eq!(promise.proposal_hash, "proposal-c");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replication_policy_is_durable_and_conflicts_fail_closed() {
        let root = test_dir("replication-policy");
        let policy = FabricStreamReplicationPolicy {
            partition: 0,
            epoch: FABRIC_STREAM_INITIAL_EPOCH,
            leader: 10,
            membership_fingerprint: 44,
            replication_factor: 2,
            replicas: vec![10, 11],
        };
        {
            let mut store = FileFabricStreamStore::open(&root).unwrap();
            store
                .create_stream("events", FabricStreamConfig::default())
                .unwrap();
            assert_eq!(
                store
                    .establish_replication_policy("events", policy.clone())
                    .unwrap(),
                policy
            );
        }

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        assert_eq!(
            reopened.replication_policy("events").unwrap(),
            Some(policy.clone())
        );
        let mut conflicting = policy;
        conflicting.epoch = 2;
        assert!(reopened
            .establish_replication_policy("events", conflicting)
            .is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn replica_progress_is_monotonic_and_persists() {
        let root = test_dir("replica-progress");
        {
            let mut store = FileFabricStreamStore::open(&root).unwrap();
            store
                .create_stream("events", FabricStreamConfig::default())
                .unwrap();
            store.record_replica_progress("events", 42, 3).unwrap();
            store.record_replica_progress("events", 42, 2).unwrap();
            assert_eq!(store.replica_progress("events", 42).unwrap(), 3);
        }

        let mut reopened = FileFabricStreamStore::open(&root).unwrap();
        assert_eq!(reopened.replica_progress("events", 42).unwrap(), 3);
        reopened.record_replica_progress("events", 42, 5).unwrap();
        assert_eq!(reopened.replica_progress("events", 42).unwrap(), 5);
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
                epoch: FABRIC_STREAM_INITIAL_EPOCH,
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
                    epoch: FABRIC_STREAM_INITIAL_EPOCH,
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
