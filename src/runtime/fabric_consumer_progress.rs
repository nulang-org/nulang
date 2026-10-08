//! Durable, hash-chained **local** consumer metadata journal.
//!
//! This is a storage/state-machine building block, not a network consensus
//! protocol. In particular, this module does not authenticate replica ACK
//! origins and is NOT connected to the public Fabric ACK APIs. A caller must
//! verify every replica's identity and fsync acknowledgement through the
//! authenticated cluster transport before supplying a certificate here.
//! No cluster-durable success may be inferred merely from proposing locally.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::runtime::{MessagePriority, NodeId, NodeStatus, Packet, Runtime};

pub(crate) const FABRIC_CONSUMER_PROGRESS_PREPARE_BEHAVIOR: &str =
    "__nulang_fabric_consumer_progress_prepare_v1";
pub(crate) const FABRIC_CONSUMER_PROGRESS_ACK_BEHAVIOR: &str =
    "__nulang_fabric_consumer_progress_ack_v1";

const JOURNAL_FORMAT_VERSION: u16 = 1;
const MAX_FRAME_BYTES: usize = 1_048_576;
const MAX_ACK_GAPS: usize = 1024;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricConsumerProgressPolicy {
    pub epoch: u64,
    pub leader: u64,
    pub replicas: Vec<u64>,
}

impl FabricConsumerProgressPolicy {
    fn validate(&self) -> io::Result<()> {
        if self.epoch == 0
            || self.replicas.is_empty()
            || !self.replicas.contains(&self.leader)
        {
            return Err(invalid("invalid consumer-progress replication policy"));
        }
        let members: BTreeSet<u64> = self.replicas.iter().copied().collect();
        if members.len() != self.replicas.len() {
            return Err(invalid("duplicate consumer-progress replica member"));
        }
        Ok(())
    }

    fn validate_quorum(&self, acknowledge: &[u64]) -> io::Result<()> {
        self.validate()?;
        let votes: BTreeSet<u64> = acknowledge.iter().copied().collect();
        if votes.len() != acknowledge.len()
            || votes.len() <= self.replicas.len() / 2
            || !votes.contains(&self.leader)
            || !votes.iter().all(|v| self.replicas.contains(v))
        {
            return Err(invalid("consumer-progress quorum certificate is not valid"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricConsumerProgressChange {
    pub stream: String,
    pub partition: u16,
    pub consumer: String,
    pub epoch: u64,
    pub generation: u64,
    pub metadata_sequence: u64,
    pub previous_metadata_sequence: u64,
    pub committed_cursor: u64,
    pub acked_gaps: Vec<u64>,
}

impl FabricConsumerProgressChange {
    fn validate(
        &self,
        policy: &FabricConsumerProgressPolicy,
        stream_committed_through: u64,
    ) -> io::Result<()> {
        policy.validate()?;
        if self.stream.is_empty()
            || self.stream.len() > 128
            || !self.stream.bytes().all(valid_name_char)
            || self.consumer.is_empty()
            || self.consumer.len() > 128
            || !self.consumer.bytes().all(valid_name_char)
            || self.epoch != policy.epoch
            || self.generation == 0
            || self.metadata_sequence == 0
            || self.committed_cursor > stream_committed_through
            || self.acked_gaps.len() > MAX_ACK_GAPS
        {
            return Err(invalid("invalid consumer-progress metadata or record boundary"));
        }

        let mut last = self.committed_cursor;
        for gap in &self.acked_gaps {
            if *gap <= last || *gap > stream_committed_through {
                return Err(invalid("consumer-progress ACK gaps must be ordered and committed"));
            }
            last = *gap;
        }
        Ok(())
    }

    fn key(&self) -> (String, u16, String) {
        (self.stream.clone(), self.partition, self.consumer.clone())
    }
}

fn valid_name_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum FabricConsumerProgressEvent {
    Prepare {
        change: FabricConsumerProgressChange,
        policy: FabricConsumerProgressPolicy,
        stream_committed_through: u64,
    },
    Commit {
        metadata_sequence: u64,
        acknowledgers: Vec<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FabricConsumerProgressFrame {
    version: u16,
    previous_hash: String,
    integrity_hash: String,
    event: FabricConsumerProgressEvent,
}

impl FabricConsumerProgressFrame {
    fn hash(
        version: u16,
        previous_hash: &str,
        event: &FabricConsumerProgressEvent,
    ) -> io::Result<String> {
        let message = serde_json::to_vec(&(version, previous_hash, event))
            .map_err(|error| invalid(error.to_string()))?;
        Ok(blake3::hash(&message).to_hex().to_string())
    }

    fn new(previous_hash: String, event: FabricConsumerProgressEvent) -> io::Result<Self> {
        let integrity_hash = Self::hash(JOURNAL_FORMAT_VERSION, &previous_hash, &event)?;
        Ok(Self {
            version: JOURNAL_FORMAT_VERSION,
            previous_hash,
            integrity_hash,
            event,
        })
    }

    fn validate(&self, expected_previous_hash: &str) -> io::Result<()> {
        if self.version != JOURNAL_FORMAT_VERSION
            || self.previous_hash != expected_previous_hash
            || self.integrity_hash
                != Self::hash(self.version, &self.previous_hash, &self.event)?
        {
            return Err(invalid("Fabric consumer-progress journal integrity violation"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct PendingConsumerProgress {
    change: FabricConsumerProgressChange,
    policy: FabricConsumerProgressPolicy,
    stream_committed_through: u64,
}

/// Local append-only journal, ready for later authenticated replica transport.
///
/// Only committed frames affect observable state. A pending prepare survives
/// restart, but does not advance the consumer cursor. Hash-chain tampering or a
/// partial final frame **fails closed**, pending an explicit repair protocol.
#[derive(Debug)]
pub(crate) struct FileFabricConsumerProgressJournal {
    root: PathBuf,
    last_hash: String,
    latest_sequence: u64,
    poisoned: bool,
    scope: Option<(String, u16)>,
    pending: Option<PendingConsumerProgress>,
    committed: BTreeMap<(String, u16, String), FabricConsumerProgressChange>,
    committed_policy: Option<FabricConsumerProgressPolicy>,
}

impl FileFabricConsumerProgressJournal {
    pub(crate) fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let mut journal = Self {
            root,
            last_hash: String::new(),
            latest_sequence: 0,
            poisoned: false,
            scope: None,
            pending: None,
            committed: BTreeMap::new(),
            committed_policy: None,
        };
        let path = journal.root.join("consumer_progress.log");
        if path.exists() {
            let mut reader = BufReader::new(File::open(&path)?);
            let mut line = Vec::new();
            loop {
                line.clear();
                let read = reader.read_until(b'\n', &mut line)?;
                if read == 0 {
                    break;
                }
                // A torn tail, empty interior frame, or oversized record
                // must be repaired from a trusted source, never ignored.
                if line.len() > MAX_FRAME_BYTES
                    || line.last() != Some(&b'\n')
                    || line.len() == 1
                {
                    return Err(invalid("invalid or truncated consumer-progress frame"));
                }
                line.pop();
                let frame: FabricConsumerProgressFrame = serde_json::from_slice(&line)
                    .map_err(|error| invalid(error.to_string()))?;
                frame.validate(&journal.last_hash)?;
                journal.apply_event(&frame.event)?;
                journal.last_hash = frame.integrity_hash;
            }
        }
        Ok(journal)
    }

    pub(crate) fn pending_sequence(&self) -> Option<u64> {
        self.pending.as_ref().map(|pending| pending.change.metadata_sequence)
    }

    pub(crate) fn last_committed_metadata_sequence(&self) -> u64 {
        self.latest_sequence
    }

    /// Recover the exact pending proposal for idempotent network retry.
    pub(crate) fn pending_change(
        &self,
    ) -> Option<(&FabricConsumerProgressChange, &FabricConsumerProgressPolicy, u64)> {
        self.pending.as_ref().map(|pending| {
            (&pending.change, &pending.policy, pending.stream_committed_through)
        })
    }

    /// Recover ACK gaps as well as the committed contiguous cursor.
    pub(crate) fn committed_change(
        &self,
        stream: &str,
        partition: u16,
        consumer: &str,
    ) -> Option<&FabricConsumerProgressChange> {
        self.committed
            .get(&(stream.to_string(), partition, consumer.to_string()))
    }

    /// Committed *local* progress only. Never treat an absent consumer as a
    /// cluster-proven cursor of zero following leader promotion.
    pub(crate) fn committed_cursor(&self, stream: &str, partition: u16, consumer: &str) -> u64 {
        self.committed
            .get(&(stream.to_string(), partition, consumer.to_string()))
            .map(|entry| entry.committed_cursor)
            .unwrap_or(0)
    }

    fn check_prepare(
        &self,
        change: &FabricConsumerProgressChange,
        policy: &FabricConsumerProgressPolicy,
        stream_committed_through: u64,
    ) -> io::Result<()> {
        change.validate(policy, stream_committed_through)?;
        if self.scope.as_ref().is_some_and(|(stream, partition)| {
            stream != &change.stream || *partition != change.partition
        }) {
            return Err(invalid(
                "consumer-progress journal cannot mix streams or partitions",
            ));
        }
        let next_sequence = self.latest_sequence.checked_add(1)
            .ok_or_else(|| invalid("consumer-progress metadata index overflow"))?;
        if change.metadata_sequence != next_sequence
            || change.previous_metadata_sequence != self.latest_sequence
        {
            return Err(invalid("consumer-progress metadata predecessor does not match"));
        }

        // Epoch changes require a separate old-quorum recovery/transition
        // protocol, which is not implemented in this storage slice.
        if self.committed_policy.as_ref().is_some_and(|old| old != policy) {
            return Err(invalid(
                "consumer-progress epoch/policy transition requires quorum recovery",
            ));
        }

        if let Some(old) = self.committed.get(&change.key()) {
            if change.generation != old.generation
                || change.committed_cursor < old.committed_cursor
                || old.acked_gaps.iter().any(|gap| {
                    *gap > change.committed_cursor && !change.acked_gaps.contains(gap)
                })
            {
                return Err(invalid("consumer progress would regress or discard ACK gaps"));
            }
        }
        Ok(())
    }

    fn apply_event(&mut self, event: &FabricConsumerProgressEvent) -> io::Result<()> {
        match event {
            FabricConsumerProgressEvent::Prepare {
                change,
                policy,
                stream_committed_through,
            } => {
                if self.pending.is_some() {
                    return Err(invalid("consumer-progress journal has unresolved prepare"));
                }
                self.check_prepare(change, policy, *stream_committed_through)?;
                if self.scope.is_none() {
                    self.scope = Some((change.stream.clone(), change.partition));
                }
                self.pending = Some(PendingConsumerProgress {
                    change: change.clone(),
                    policy: policy.clone(),
                    stream_committed_through: *stream_committed_through,
                });
            }
            FabricConsumerProgressEvent::Commit {
                metadata_sequence,
                acknowledgers,
            } => {
                let pending = self.pending.as_ref().ok_or_else(|| {
                    invalid("consumer-progress commit has no durable predecessor")
                })?;
                if *metadata_sequence != pending.change.metadata_sequence {
                    return Err(invalid("consumer-progress commit index mismatch"));
                }
                pending.policy.validate_quorum(acknowledgers)?;
                let pending = self.pending.take().expect("validated pending metadata");
                self.latest_sequence = *metadata_sequence;
                self.committed_policy = Some(pending.policy);
                self.committed.insert(pending.change.key(), pending.change);
            }
        }
        Ok(())
    }

    fn ensure_writable(&self) -> io::Result<()> {
        if self.poisoned {
            return Err(invalid(
                "consumer-progress journal has an uncertain write; reopen and recover first",
            ));
        }
        Ok(())
    }

    fn append_event(&mut self, event: FabricConsumerProgressEvent) -> io::Result<()> {
        self.ensure_writable()?;
        let frame = FabricConsumerProgressFrame::new(self.last_hash.clone(), event)?;
        let mut bytes = serde_json::to_vec(&frame)
            .map_err(|error| invalid(error.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(invalid("consumer-progress journal frame exceeds limit"));
        }
        let path = self.root.join("consumer_progress.log");
        let created = !path.exists();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_data()) {
            // An uncertain local fsync result requires reopening before any
            // further operation; do not act on cached state.
            self.poisoned = true;
            return Err(error);
        }
        if created {
            if let Err(error) = File::open(&self.root).and_then(|dir| dir.sync_all()) {
                self.poisoned = true;
                return Err(error);
            }
        }
        if let Err(error) = self.apply_event(&frame.event) {
            self.poisoned = true;
            return Err(error);
        }
        self.last_hash = frame.integrity_hash;
        Ok(())
    }

    /// Durably stage a record but do not publish its cursor as committed.
    pub(crate) fn prepare(
        &mut self,
        change: FabricConsumerProgressChange,
        policy: FabricConsumerProgressPolicy,
        stream_committed_through: u64,
    ) -> io::Result<()> {
        self.ensure_writable()?;
        if let Some(pending) = &self.pending {
            // Same exact prepare is idempotent and must not add another frame.
            if pending.change == change
                && pending.policy == policy
                && pending.stream_committed_through == stream_committed_through
            {
                return Ok(());
            }
            return Err(invalid("different consumer-progress record already pending"));
        }
        self.check_prepare(&change, &policy, stream_committed_through)?;
        self.append_event(FabricConsumerProgressEvent::Prepare {
            change,
            policy,
            stream_committed_through,
        })
    }

    /// Storage-only quorum-certificate validation and durable commit.
    ///
    /// Calling code MUST authenticate member IDs and verify that each vote
    /// represents a replica fsync of this exact record. This function does
    /// not do network IO, authenticate peers, or prove a live quorum itself.
    pub(crate) fn commit_with_acknowledgers(
        &mut self,
        metadata_sequence: u64,
        acknowledgers: &[u64],
    ) -> io::Result<()> {
        self.ensure_writable()?;
        let pending = self.pending.as_ref().ok_or_else(|| {
            invalid("consumer-progress commit has no pending durable record")
        })?;
        if pending.change.metadata_sequence != metadata_sequence {
            return Err(invalid("consumer-progress commit index mismatch"));
        }
        pending.policy.validate_quorum(acknowledgers)?;
        self.append_event(FabricConsumerProgressEvent::Commit {
            metadata_sequence,
            acknowledgers: acknowledgers.to_vec(),
        })
    }
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FabricConsumerProgressPrepareWire {
    version: u16,
    change: FabricConsumerProgressChange,
    policy: FabricConsumerProgressPolicy,
    stream_committed_through: u64,
    digest: String,
}

impl FabricConsumerProgressPrepareWire {
    fn calculate_digest(&self) -> io::Result<String> {
        let bytes = serde_json::to_vec(&(
            self.version,
            &self.change,
            &self.policy,
            self.stream_committed_through,
        )).map_err(|error| invalid(error.to_string()))?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }

    fn new(
        change: FabricConsumerProgressChange,
        policy: FabricConsumerProgressPolicy,
        stream_committed_through: u64,
    ) -> io::Result<Self> {
        let mut wire = Self {
            version: JOURNAL_FORMAT_VERSION,
            change,
            policy,
            stream_committed_through,
            digest: String::new(),
        };
        wire.change.validate(&wire.policy, stream_committed_through)?;
        wire.digest = wire.calculate_digest()?;
        Ok(wire)
    }

    fn verify(&self) -> io::Result<()> {
        if self.version != JOURNAL_FORMAT_VERSION
            || self.digest != self.calculate_digest()?
        {
            return Err(invalid("Fabric consumer-progress prepare digest or version mismatch"));
        }
        self.change.validate(&self.policy, self.stream_committed_through)
    }

    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        self.verify()?;
        let bytes = serde_json::to_vec(self).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(invalid("Fabric consumer-progress prepare wire frame too large"));
        }
        Ok(bytes)
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(invalid("Fabric consumer-progress prepare wire frame too large"));
        }
        let wire: Self =
            serde_json::from_slice(bytes).map_err(|error| invalid(error.to_string()))?;
        wire.verify()?;
        Ok(wire)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FabricConsumerProgressAckWire {
    version: u16,
    stream: String,
    partition: u16,
    epoch: u64,
    metadata_sequence: u64,
    digest: String,
    replica: u64,
}

impl FabricConsumerProgressAckWire {
    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(invalid("Fabric consumer-progress ACK wire frame too large"));
        }
        Ok(bytes)
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(invalid("Fabric consumer-progress ACK wire frame too large"));
        }
        let ack: Self =
            serde_json::from_slice(bytes).map_err(|error| invalid(error.to_string()))?;
        if ack.version != JOURNAL_FORMAT_VERSION
            || ack.stream.is_empty()
            || ack.epoch == 0
            || ack.metadata_sequence == 0
            || ack.digest.len() != 64
        {
            return Err(invalid("invalid Fabric consumer-progress ACK envelope"));
        }
        Ok(ack)
    }
}

impl Runtime {
    fn consumer_progress_directory(&mut self, stream: &str) -> io::Result<PathBuf> {
        let root = self.distributed.fabric_streams.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "Fabric stream storage not open")
        })?.root().to_path_buf();
        // Validate and recover the stream before joining its filesystem path.
        self.fabric_stream_info(stream)?;
        Ok(root.join(stream).join("consumer_progress"))
    }

    fn consumer_progress_installed_policy(
        &mut self,
        stream: &str,
    ) -> io::Result<FabricConsumerProgressPolicy> {
        let installed = self.fabric_stream_replication_policy(stream)?.ok_or_else(|| {
            invalid("consumer-progress metadata requires installed stream replication policy")
        })?;
        if let Some(promise) = self.fabric_stream_epoch_promise(stream)? {
            if promise.epoch > installed.epoch {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "consumer-progress metadata fenced by promised newer stream epoch",
                ));
            }
        }
        Ok(FabricConsumerProgressPolicy {
            epoch: installed.epoch,
            leader: installed.leader,
            replicas: installed.replicas,
        })
    }

    fn consumer_progress_validate_leader(
        &mut self,
        stream: &str,
    ) -> io::Result<FabricConsumerProgressPolicy> {
        let policy = self.consumer_progress_installed_policy(stream)?;
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "consumer-progress leader requires distribution")
        })?;
        if local.0 != policy.leader {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "consumer-progress prepare requires installed stream leader",
            ));
        }
        let healthy = self.distributed.cluster.as_ref()
            .and_then(|cluster| cluster.get_node(local))
            .map(|member| matches!(member.status, NodeStatus::Healthy | NodeStatus::Joining))
            .unwrap_or(false);
        if !healthy {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "consumer-progress leader must be locally healthy",
            ));
        }
        Ok(policy)
    }

    /// Stage a storage-only prepare on the leader. It is not a committed ACK.
    pub(crate) fn fabric_consumer_progress_stage(
        &mut self,
        change: FabricConsumerProgressChange,
    ) -> io::Result<()> {
        if change.partition != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "consumer-progress metadata currently supports only physical partition 0",
            ));
        }
        let policy = self.consumer_progress_validate_leader(&change.stream)?;
        let committed_through = self.fabric_stream_committed_sequence(&change.stream)?;
        let path = self.consumer_progress_directory(&change.stream)?;
        let wire = FabricConsumerProgressPrepareWire::new(
            change,
            policy,
            committed_through,
        )?;
        let mut journal = FileFabricConsumerProgressJournal::open(path)?;
        journal.prepare(
            wire.change.clone(),
            wire.policy.clone(),
            wire.stream_committed_through,
        )?;
        let key = (
            wire.change.stream,
            wire.change.partition,
            wire.change.metadata_sequence,
            wire.digest,
        );
        self.distributed
            .fabric_consumer_progress_receipts
            .entry(key)
            .or_default()
            .insert(NodeId(wire.policy.leader));
        Ok(())
    }

    /// Dispatch an already fsynced leader prepare to healthy installed followers.
    /// An enqueue is not a follower fsync and never advances the ACK cursor.
    pub(crate) fn fabric_consumer_progress_dispatch_prepare(
        &mut self,
        stream: &str,
        partition: u16,
        metadata_sequence: u64,
    ) -> io::Result<usize> {
        let policy = self.consumer_progress_validate_leader(stream)?;
        let path = self.consumer_progress_directory(stream)?;
        let journal = FileFabricConsumerProgressJournal::open(path)?;
        let (change, pending_policy, stage_bound) = journal.pending_change().ok_or_else(|| {
            invalid("no pending consumer-progress prepare to dispatch")
        })?;
        if change.partition != partition
            || change.metadata_sequence != metadata_sequence
            || pending_policy != &policy
        {
            return Err(invalid("consumer-progress prepare does not match installed policy"));
        }
        let wire = FabricConsumerProgressPrepareWire::new(
            change.clone(), policy.clone(), stage_bound,
        )?;
        let bytes = wire.to_wire_bytes()?;
        let local = NodeId(policy.leader);
        let mut destinations = Vec::new();
        let cluster = self.distributed.cluster.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "consumer-progress dispatch requires cluster")
        })?;
        for replica in &policy.replicas {
            let id = NodeId(*replica);
            if id == local {
                continue;
            }
            if let Some(member) = cluster.get_node(id).filter(|member| {
                matches!(member.status, NodeStatus::Healthy | NodeStatus::Joining)
            }) {
                destinations.push((id, member.address));
            }
        }
        let transport = self.distributed.transport.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "consumer-progress dispatch requires transport")
        })?;
        for (target, address) in &destinations {
            transport.send(*target, *address, Packet::ActorMessage {
                target_actor: 0,
                behavior_name: FABRIC_CONSUMER_PROGRESS_PREPARE_BEHAVIOR.to_string(),
                content_hash: None,
                required_protocol_id: None,
                payload: Vec::new(),
                string_table: Vec::new(),
                object_table: vec![(0, bytes.clone())],
                sender_actor: 0,
                sender_node: local,
                priority: MessagePriority::System,
                trace_id: None,
            });
        }
        Ok(destinations.len())
    }

    /// Follower validates the installed epoch/policy and fsyncs the exact
    /// proposed change before producing a correlatable application receipt.
    pub(crate) fn fabric_consumer_progress_apply_prepare_from_peer(
        &mut self,
        wire: &FabricConsumerProgressPrepareWire,
        from: NodeId,
    ) -> io::Result<FabricConsumerProgressAckWire> {
        wire.verify()?;
        if wire.change.partition != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "consumer-progress follower supports only physical partition 0",
            ));
        }
        let installed = self.consumer_progress_installed_policy(&wire.change.stream)?;
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "consumer-progress follower requires distribution")
        })?;
        if from.0 != wire.policy.leader
            || wire.policy != installed
            || !installed.replicas.contains(&local.0)
            || local.0 == installed.leader
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unauthorized consumer-progress prepare sender, epoch, or replica",
            ));
        }
        let local_commit = self.fabric_stream_committed_sequence(&wire.change.stream)?;
        if local_commit < wire.stream_committed_through {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "consumer-progress follower has not caught up to the requested commit prefix",
            ));
        }
        let path = self.consumer_progress_directory(&wire.change.stream)?;
        let mut journal = FileFabricConsumerProgressJournal::open(path)?;
        journal.prepare(
            wire.change.clone(), wire.policy.clone(), wire.stream_committed_through,
        )?;
        Ok(FabricConsumerProgressAckWire {
            version: JOURNAL_FORMAT_VERSION,
            stream: wire.change.stream.clone(),
            partition: wire.change.partition,
            epoch: wire.change.epoch,
            metadata_sequence: wire.change.metadata_sequence,
            digest: wire.digest.clone(),
            replica: local.0,
        })
    }

    /// Validate a transport-authenticated sender and exact local pending
    /// record before retaining its fsync receipt. This only *observes* votes.
    pub(crate) fn fabric_consumer_progress_record_replica_receipt(
        &mut self,
        receipt: &FabricConsumerProgressAckWire,
        from: NodeId,
    ) -> io::Result<()> {
        let policy = self.consumer_progress_validate_leader(&receipt.stream)?;
        let local = NodeId(policy.leader);
        if receipt.replica != from.0
            || from == local
            || !policy.replicas.contains(&from.0)
            || receipt.epoch != policy.epoch
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied, "unauthorized consumer-progress replica receipt",
            ));
        }
        let path = self.consumer_progress_directory(&receipt.stream)?;
        let journal = FileFabricConsumerProgressJournal::open(path)?;
        let (change, pending_policy, stage_bound) = journal.pending_change().ok_or_else(|| {
            invalid("consumer-progress replica receipt has no pending leader proposal")
        })?;
        if receipt.metadata_sequence != change.metadata_sequence
            || receipt.partition != change.partition
            || pending_policy != &policy
        {
            return Err(invalid("consumer-progress replica receipt does not match pending metadata"));
        }
        let wire = FabricConsumerProgressPrepareWire::new(
            change.clone(),
            policy,
            stage_bound,
        )?;
        if receipt.digest != wire.digest {
            return Err(invalid("consumer-progress replica receipt digest mismatch"));
        }
        let key = (
            receipt.stream.clone(), receipt.partition,
            receipt.metadata_sequence, receipt.digest.clone(),
        );
        // Sender identity is validated against incoming transport before this
        // method runs. These volatile observations are NOT durable certificates.
        self.distributed.fabric_consumer_progress_receipts
            .entry(key)
            .or_insert_with(|| HashSet::from([local]))
            .insert(from);
        Ok(())
    }

    pub(crate) fn fabric_consumer_progress_observed_votes(
        &mut self,
        stream: &str,
        partition: u16,
        metadata_sequence: u64,
    ) -> io::Result<Vec<NodeId>> {
        let policy = self.consumer_progress_validate_leader(stream)?;
        let path = self.consumer_progress_directory(stream)?;
        let journal = FileFabricConsumerProgressJournal::open(path)?;
        let (change, _, stage_bound) = journal.pending_change().ok_or_else(|| {
            invalid("consumer-progress votes requested without pending prepare")
        })?;
        if change.partition != partition || change.metadata_sequence != metadata_sequence {
            return Err(invalid("consumer-progress vote query mismatched pending proposal"));
        }
        let wire = FabricConsumerProgressPrepareWire::new(
            change.clone(), policy, stage_bound,
        )?;
        let key = (stream.to_string(), partition, metadata_sequence, wire.digest);
        let mut votes: Vec<NodeId> = self.distributed.fabric_consumer_progress_receipts
            .get(&key).map(|set| set.iter().copied().collect()).unwrap_or_default();
        votes.sort_by_key(|id| id.0);
        Ok(votes)
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nulang-fabric-consumer-progress-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn policy(replicas: Vec<u64>) -> FabricConsumerProgressPolicy {
        FabricConsumerProgressPolicy {
            epoch: 1,
            leader: 10,
            replicas,
        }
    }

    fn change(index: u64, previous: u64, cursor: u64) -> FabricConsumerProgressChange {
        FabricConsumerProgressChange {
            stream: "orders".to_string(),
            partition: 0,
            consumer: "billing".to_string(),
            epoch: 1,
            generation: 1,
            metadata_sequence: index,
            previous_metadata_sequence: previous,
            committed_cursor: cursor,
            acked_gaps: Vec::new(),
        }
    }

    #[test]
    fn pending_metadata_is_not_visible_as_committed_after_restart() {
        let root = temp_root("pending");
        let group = policy(vec![10, 11, 12]);
        {
            let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
            journal.prepare(change(1, 0, 2), group.clone(), 5).unwrap();
            assert_eq!(journal.pending_sequence(), Some(1));
            assert_eq!(journal.committed_cursor("orders", 0, "billing"), 0);
            assert!(journal.commit_with_acknowledgers(1, &[10]).is_err());
            assert!(journal.commit_with_acknowledgers(1, &[10, 10]).is_err());
        }

        let mut reopened = FileFabricConsumerProgressJournal::open(&root).unwrap();
        assert_eq!(reopened.pending_sequence(), Some(1));
        assert_eq!(reopened.committed_cursor("orders", 0, "billing"), 0);
        reopened.commit_with_acknowledgers(1, &[10, 11]).unwrap();
        assert_eq!(reopened.pending_sequence(), None);
        assert_eq!(reopened.committed_cursor("orders", 0, "billing"), 2);

        let after_restart = FileFabricConsumerProgressJournal::open(&root).unwrap();
        assert_eq!(after_restart.committed_cursor("orders", 0, "billing"), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn quorum_certificate_cannot_use_nonmembers_or_skip_leader() {
        let root = temp_root("certificate");
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(change(1, 0, 1), policy(vec![10, 11, 12]), 1).unwrap();
        for invalid in [&[11, 12][..], &[10, 99][..], &[10][..], &[10, 10][..]] {
            assert!(journal.commit_with_acknowledgers(1, invalid).is_err());
            assert_eq!(journal.committed_cursor("orders", 0, "billing"), 0);
        }
        journal.commit_with_acknowledgers(1, &[10, 12]).unwrap();
        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn metadata_sequence_and_consumer_cursor_never_go_backwards() {
        let root = temp_root("monotonic");
        let group = policy(vec![10, 11, 12]);
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(change(1, 0, 3), group.clone(), 5).unwrap();
        // The exact duplicate is idempotent; a different uncommitted proposal
        // must not replace the original pending record.
        journal.prepare(change(1, 0, 3), group.clone(), 5).unwrap();
        assert!(journal.prepare(change(1, 0, 4), group.clone(), 5).is_err());
        journal.commit_with_acknowledgers(1, &[10, 11]).unwrap();

        assert!(journal.prepare(change(3, 1, 4), group.clone(), 5).is_err());
        assert!(journal.prepare(change(2, 0, 4), group.clone(), 5).is_err());
        assert!(journal.prepare(change(2, 1, 2), group.clone(), 5).is_err());
        journal.prepare(change(2, 1, 4), group.clone(), 5).unwrap();
        journal.commit_with_acknowledgers(2, &[10, 12]).unwrap();

        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 4);
        assert_eq!(journal.last_committed_metadata_sequence(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn gaps_must_be_sorted_bounded_and_preserved_until_consumed() {
        let root = temp_root("gaps");
        let group = policy(vec![10, 11]);
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        let mut first = change(1, 0, 1);
        first.acked_gaps = vec![3, 5];
        journal.prepare(first, group.clone(), 5).unwrap();
        journal.commit_with_acknowledgers(1, &[10, 11]).unwrap();

        let mut lost_gap = change(2, 1, 2);
        lost_gap.acked_gaps = vec![5];
        assert!(journal.prepare(lost_gap, group.clone(), 5).is_err());

        let mut invalid_order = change(2, 1, 2);
        invalid_order.acked_gaps = vec![5, 3];
        assert!(journal.prepare(invalid_order, group.clone(), 5).is_err());

        let mut past_tail = change(2, 1, 2);
        past_tail.acked_gaps = vec![3, 6];
        assert!(journal.prepare(past_tail, group.clone(), 5).is_err());

        let mut second = change(2, 1, 3);
        second.acked_gaps = vec![5];
        journal.prepare(second, group, 5).unwrap();
        journal.commit_with_acknowledgers(2, &[10, 11]).unwrap();
        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 3);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_epochs_and_policy_membership_fail_before_appending() {
        let root = temp_root("epochs");
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        let mut bad_epoch = change(1, 0, 1);
        bad_epoch.epoch = 2;
        assert!(journal.prepare(bad_epoch, policy(vec![10, 11]), 1).is_err());
        assert!(journal.prepare(change(1, 0, 1), policy(vec![10, 10]), 1).is_err());
        assert!(journal.prepare(change(1, 0, 1), policy(vec![11, 12]), 1).is_err());
        assert!(journal.prepare(change(1, 0, 2), policy(vec![10, 11]), 1).is_err());
        assert_eq!(journal.pending_sequence(), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tampering_and_partial_journal_tail_fail_closed_on_open() {
        let root = temp_root("corruption");
        let group = policy(vec![10, 11]);
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(change(1, 0, 1), group, 1).unwrap();
        drop(journal);
        let path = root.join("consumer_progress.log");
        let mut bytes = fs::read(&path).unwrap();
        let pos = bytes.iter().position(|byte| *byte == b'1').unwrap();
        bytes[pos] = b'9';
        fs::write(&path, &bytes).unwrap();
        assert!(FileFabricConsumerProgressJournal::open(&root).is_err());

        fs::write(&path, b"{\"version\":1").unwrap();
        assert!(FileFabricConsumerProgressJournal::open(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }


    #[test]
    fn committed_ack_gaps_and_pending_proposals_survive_reopen() {
        let root = temp_root("restore");
        let group = policy(vec![10, 11, 12]);
        {
            let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
            let mut first = change(1, 0, 1);
            first.acked_gaps = vec![3, 5];
            journal.prepare(first, group.clone(), 5).unwrap();
            journal.commit_with_acknowledgers(1, &[10, 11]).unwrap();

            let mut pending = change(2, 1, 2);
            pending.acked_gaps = vec![3, 5];
            journal.prepare(pending, group, 5).unwrap();
        }

        let reopened = FileFabricConsumerProgressJournal::open(&root).unwrap();
        assert_eq!(reopened.pending_sequence(), Some(2));
        assert_eq!(reopened.pending_change().unwrap().0.committed_cursor, 2);
        let committed = reopened.committed_change("orders", 0, "billing").unwrap();
        assert_eq!(committed.committed_cursor, 1);
        assert_eq!(committed.acked_gaps, vec![3, 5]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn policy_change_is_rejected_without_explicit_epoch_recovery() {
        let root = temp_root("policy");
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(change(1, 0, 1), policy(vec![10, 11]), 1).unwrap();
        journal.commit_with_acknowledgers(1, &[10, 11]).unwrap();

        let mut next = change(2, 1, 2);
        next.epoch = 2;
        let changed_policy = FabricConsumerProgressPolicy {
            epoch: 2,
            leader: 11,
            replicas: vec![11, 12],
        };
        assert!(journal.prepare(next, changed_policy, 2).is_err());
        assert_eq!(journal.last_committed_metadata_sequence(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn journal_rejects_cross_stream_or_partition_history() {
        let root = temp_root("scope");
        let group = policy(vec![10, 11]);
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(change(1, 0, 1), group.clone(), 1).unwrap();
        journal.commit_with_acknowledgers(1, &[10, 11]).unwrap();

        let mut different_stream = change(2, 1, 2);
        different_stream.stream = "payments".to_string();
        assert!(journal.prepare(different_stream, group.clone(), 2).is_err());

        let mut different_partition = change(2, 1, 2);
        different_partition.partition = 1;
        assert!(journal.prepare(different_partition, group.clone(), 2).is_err());

        let mut same_partition_other_consumer = change(2, 1, 1);
        same_partition_other_consumer.consumer = "analytics".to_string();
        journal.prepare(same_partition_other_consumer, group, 2).unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn two_replica_metadata_requires_both_durable_votes() {
        let root = temp_root("rf2");
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(change(1, 0, 1), policy(vec![10, 11]), 1).unwrap();
        assert!(journal.commit_with_acknowledgers(1, &[10]).is_err());
        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 0);
        journal.commit_with_acknowledgers(1, &[10, 11]).unwrap();
        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 1);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn follower_prepare_fsync_ack_is_observed_but_not_committed() {
        use std::collections::{HashMap, HashSet};
        use std::net::SocketAddr;
        use std::sync::Arc;
        use crate::runtime::{
            DeterministicNetworkTransport, FabricStreamConfig, IncomingPacket, NodeId,
            OutgoingPacket, Runtime,
        };

        type Bus = Arc<parking_lot::Mutex<HashMap<
            NodeId,
            (
                std::sync::mpsc::SyncSender<IncomingPacket>,
                std::sync::mpsc::SyncSender<OutgoingPacket>,
            ),
        >>>;
        fn node(addr: SocketAddr, bus: Bus) -> Runtime {
            let mut runtime = Runtime::new();
            runtime.install_virtual_clock();
            let transport = DeterministicNetworkTransport::bind_with_bus(addr, bus).unwrap();
            transport.register_on_bus();
            runtime.enable_distribution_with_transport(Box::new(transport)).unwrap();
            runtime
        }
        let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let a_addr: SocketAddr = "127.0.0.1:39211".parse().unwrap();
        let b_addr: SocketAddr = "127.0.0.1:39212".parse().unwrap();
        let a_id = NodeId::new(&a_addr);
        let b_id = NodeId::new(&b_addr);
        let mut a = node(a_addr, bus.clone());
        let mut b = node(b_addr, bus);
        a.distributed.cluster.as_mut().unwrap().handle_heartbeat(b_id, b_addr);
        b.distributed.cluster.as_mut().unwrap().handle_heartbeat(a_id, a_addr);
        let placement = a.fabric_stream_placement("orders", 0, 2).unwrap();
        let a_root = temp_root("network-a");
        let b_root = temp_root("network-b");
        a.fabric_stream_open(&a_root).unwrap();
        b.fabric_stream_open(&b_root).unwrap();
        let (leader, follower, leader_root, follower_root) = if placement.leader == a_id {
            (&mut a, &mut b, &a_root, &b_root)
        } else {
            (&mut b, &mut a, &b_root, &a_root)
        };

        leader.fabric_stream_create("orders", FabricStreamConfig::default()).unwrap();
        leader.fabric_stream_replicated_append("orders", 0, 2, b"event").unwrap();
        follower.process_network();
        leader.process_network();
        follower.process_network();
        assert_eq!(leader.fabric_stream_committed_sequence("orders").unwrap(), 1);
        assert_eq!(follower.fabric_stream_committed_sequence("orders").unwrap(), 1);

        let change = FabricConsumerProgressChange {
            stream: "orders".into(),
            partition: 0,
            consumer: "billing".into(),
            epoch: 1,
            generation: 1,
            metadata_sequence: 1,
            previous_metadata_sequence: 0,
            committed_cursor: 1,
            acked_gaps: vec![],
        };
        let mut nonphysical = change.clone();
        nonphysical.partition = 1;
        assert!(leader.fabric_consumer_progress_stage(nonphysical).is_err());
        leader.fabric_consumer_progress_stage(change).unwrap();
        assert_eq!(
            leader.fabric_consumer_progress_observed_votes("orders", 0, 1).unwrap().len(), 1
        );
        let installed = leader.consumer_progress_installed_policy("orders").unwrap();
        let wire = FabricConsumerProgressPrepareWire::new(
            FileFabricConsumerProgressJournal::open(
                leader_root.join("orders").join("consumer_progress")
            ).unwrap().pending_change().unwrap().0.clone(),
            installed,
            1,
        ).unwrap();
        let follower_id = if placement.leader == a_id { b_id } else { a_id };
        assert!(follower.fabric_consumer_progress_apply_prepare_from_peer(
            &wire, follower_id,
        ).is_err());
        assert!(follower.fabric_consumer_progress_apply_prepare_from_peer(
            &wire, NodeId(999_999),
        ).is_err());

        // A transport partition can still enqueue a send, but it must not
        // create an application fsync receipt or committed consumer cursor.
        leader.distributed.transport.as_mut().unwrap()
            .set_partition(HashSet::from([follower_id]));
        assert_eq!(
            leader.fabric_consumer_progress_dispatch_prepare("orders", 0, 1).unwrap(),
            1
        );
        follower.process_network();
        leader.process_network();
        assert_eq!(
            leader.fabric_consumer_progress_observed_votes("orders", 0, 1).unwrap().len(),
            1
        );
        leader.distributed.transport.as_mut().unwrap()
            .set_partition(HashSet::new());
        assert_eq!(
            leader.fabric_consumer_progress_dispatch_prepare("orders", 0, 1).unwrap(),
            1
        );
        follower.process_network();
        leader.process_network();
        let observed = leader.fabric_consumer_progress_observed_votes("orders", 0, 1).unwrap();
        assert_eq!(observed.len(), 2);
        assert!(observed.contains(&placement.leader));
        assert!(observed.contains(&follower_id));
        let bogus = FabricConsumerProgressAckWire {
            version: JOURNAL_FORMAT_VERSION,
            stream: "orders".to_string(),
            partition: 0,
            epoch: 1,
            metadata_sequence: 1,
            digest: "f".repeat(64),
            replica: follower_id.0,
        };
        assert!(leader
            .fabric_consumer_progress_record_replica_receipt(&bogus, follower_id)
            .is_err());

        let local = FileFabricConsumerProgressJournal::open(
            leader_root.join("orders").join("consumer_progress")
        ).unwrap();
        let remote = FileFabricConsumerProgressJournal::open(
            follower_root.join("orders").join("consumer_progress")
        ).unwrap();
        assert_eq!(local.pending_sequence(), Some(1));
        assert_eq!(remote.pending_sequence(), Some(1));
        assert_eq!(local.pending_change().unwrap().2, 1);
        assert_eq!(remote.pending_change().unwrap().2, 1);
        assert_eq!(local.committed_cursor("orders", 0, "billing"), 0);
        assert_eq!(remote.committed_cursor("orders", 0, "billing"), 0);

        // Persisted follower-fsync evidence is sufficient to request a local
        // metadata commit, but only after the exact quorum receipts arrive.
        assert_eq!(
            leader.fabric_consumer_progress_commit_observed("orders", 0, 1).unwrap(),
            1
        );
        follower.process_network();
        let leader_after = FileFabricConsumerProgressJournal::open(
            leader_root.join("orders").join("consumer_progress")
        ).unwrap();
        let follower_after = FileFabricConsumerProgressJournal::open(
            follower_root.join("orders").join("consumer_progress")
        ).unwrap();
        assert_eq!(leader_after.committed_cursor("orders", 0, "billing"), 1);
        assert_eq!(follower_after.committed_cursor("orders", 0, "billing"), 1);
        let _ = fs::remove_dir_all(a_root);
        let _ = fs::remove_dir_all(b_root);
    }

    #[test]
    fn pending_prepare_rejects_changed_commit_boundary_and_digest() {
        let root = temp_root("fixed-boundary");
        let group = policy(vec![10, 11]);
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        let proposal = change(1, 0, 1);
        journal.prepare(proposal.clone(), group.clone(), 1).unwrap();
        // A retry must reproduce the exact same on-disk Prepare frame.
        assert!(journal.prepare(proposal.clone(), group.clone(), 2).is_err());

        let original =
            FabricConsumerProgressPrepareWire::new(proposal.clone(), group.clone(), 1).unwrap();
        let altered =
            FabricConsumerProgressPrepareWire::new(proposal, group, 2).unwrap();
        assert_ne!(original.digest, altered.digest);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepare_wire_rejects_tampering_and_unmatched_sender() {
        let change = change(1, 0, 1);
        let policy = policy(vec![10, 11]);
        let mut wire = FabricConsumerProgressPrepareWire::new(
            change, policy, 1
        ).unwrap();
        assert!(FabricConsumerProgressPrepareWire::from_wire_bytes(
            &wire.to_wire_bytes().unwrap()
        ).is_ok());
        wire.change.committed_cursor = 2;
        assert!(wire.verify().is_err());
    }

    #[test]
    fn persisted_peer_receipt_survives_reopen_but_unconfirmed_prepare_stays_pending() {
        let root = temp_root("persist-votes");
        let group = policy(vec![10, 11, 12]);
        let proposal = change(1, 0, 2);
        let wire = FabricConsumerProgressPrepareWire::new(
            proposal.clone(), group.clone(), 2,
        ).unwrap();
        {
            let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
            journal.prepare(proposal, group, 2).unwrap();
            assert!(journal.commit_observed_quorum(1).is_err());
            assert!(journal.record_verified_receipt(99, &wire.digest).is_err());
            assert!(journal.record_verified_receipt(11, "0").is_err());
            journal.record_verified_receipt(11, &wire.digest).unwrap();
            journal.record_verified_receipt(11, &wire.digest).unwrap();
            assert_eq!(journal.verified_voters(), vec![10, 11]);
        }
        let mut recovered = FileFabricConsumerProgressJournal::open(&root).unwrap();
        assert_eq!(recovered.pending_sequence(), Some(1));
        assert_eq!(recovered.verified_voters(), vec![10, 11]);
        assert_eq!(recovered.committed_cursor("orders", 0, "billing"), 0);
        recovered.commit_observed_quorum(1).unwrap();
        assert_eq!(recovered.committed_cursor("orders", 0, "billing"), 2);
        let after = FileFabricConsumerProgressJournal::open(&root).unwrap();
        assert_eq!(after.committed_cursor("orders", 0, "billing"), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn quorum_proof_cannot_mix_votes_from_changed_pending_record() {
        let root = temp_root("receipt-scope");
        let group = policy(vec![10, 11]);
        let proposed = change(1, 0, 1);
        let wire = FabricConsumerProgressPrepareWire::new(
            proposed.clone(), group.clone(), 1,
        ).unwrap();
        let mut journal = FileFabricConsumerProgressJournal::open(&root).unwrap();
        journal.prepare(proposed.clone(), group, 1).unwrap();
        let mut changed = proposed;
        changed.committed_cursor = 0;
        let other = FabricConsumerProgressPrepareWire::new(
            changed, policy(vec![10, 11]), 1,
        ).unwrap();
        assert_ne!(wire.digest, other.digest);
        assert!(journal.record_verified_receipt(11, &other.digest).is_err());
        assert!(journal.commit_observed_quorum(1).is_err());
        assert_eq!(journal.committed_cursor("orders", 0, "billing"), 0);
        let _ = fs::remove_dir_all(root);
    }

}
