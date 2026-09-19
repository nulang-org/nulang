//! Queue-level ownership and policy synchronization for replicated Fabric queues.
//!
//! A queue owns multiple internal Fabric streams (currently payload and
//! mutation metadata). They must share one epoch, leader, replica set, and
//! membership fingerprint. Deriving placement independently from each stream
//! name is unsafe because rendezvous hashing could choose different leaders.
//!
//! Queue policy installation therefore uses one logical placement key and a
//! reserved NUL0 system message. Replicated queue traffic must not start until
//! the leader has application-level policy acknowledgements from every replica.

use std::collections::{HashMap, HashSet};
use std::io;

use serde::{Deserialize, Serialize};

use super::fabric_queue::{
    decode_queue_consumer_group_config, decode_queue_created_mutation, decode_queue_envelope_bytes,
    decode_queue_lease_mutation, decode_queue_operation, encode_queue_created_mutation,
    encode_queue_envelope, queue_mutation_stream_name, queue_stream_name,
    validate_consumer_group_name, validate_consumer_name, validate_operation_id,
    validate_queue_name, FabricQueueAddOptions, FabricQueueConfig, FabricQueueConsumerGroupConfig,
    FabricQueueConsumerGroupInfo, FabricQueueDeadLetterPlan, FabricQueueDelivery,
    FabricQueueNackResult, FabricQueueOperation, FabricQueueOperationKind,
};
use super::fabric_stream::{FabricStreamReplicationPolicy, FABRIC_STREAM_INITIAL_EPOCH};
use super::{
    FabricStreamConfig, FabricStreamReplicationStatus, MessagePriority, NodeId, NodeStatus, Packet,
    Runtime,
};

const QUEUE_PLACEMENT_PREFIX: &str = "__queue_owner.";
pub(crate) const FABRIC_QUEUE_POLICY_BEHAVIOR: &str = "__nulang_fabric_queue_policy_v1";
pub(crate) const FABRIC_QUEUE_POLICY_ACK_BEHAVIOR: &str = "__nulang_fabric_queue_policy_ack_v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueuePlacement {
    pub queue: String,
    pub partition: u16,
    pub epoch: u64,
    pub leader: NodeId,
    pub replicas: Vec<NodeId>,
    pub membership_fingerprint: u64,
}

impl FabricQueuePlacement {
    pub fn replication_factor(&self) -> usize {
        self.replicas.len()
    }

    fn from_policy(queue: &str, policy: &FabricStreamReplicationPolicy) -> Self {
        Self {
            queue: queue.to_string(),
            partition: policy.partition,
            epoch: policy.epoch,
            leader: NodeId(policy.leader),
            replicas: policy.replicas.iter().copied().map(NodeId).collect(),
            membership_fingerprint: policy.membership_fingerprint,
        }
    }

    fn to_policy(&self) -> FabricStreamReplicationPolicy {
        FabricStreamReplicationPolicy {
            partition: self.partition,
            epoch: self.epoch,
            leader: self.leader.0,
            membership_fingerprint: self.membership_fingerprint,
            replication_factor: self.replicas.len(),
            replicas: self.replicas.iter().map(|node| node.0).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FabricQueuePolicySyncReport {
    pub intended_remote: usize,
    pub dispatched: usize,
    pub unavailable: usize,
    pub acknowledgements: usize,
    pub required: usize,
    pub ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FabricQueueReplicatedCreateResult {
    pub policy: FabricQueuePolicySyncReport,
    pub mutation_sequence: Option<u64>,
    pub replication: Option<FabricStreamReplicationStatus>,
    pub created: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FabricQueueReplicatedAddResult {
    pub policy: FabricQueuePolicySyncReport,
    pub sequence: Option<u64>,
    pub replication: Option<FabricStreamReplicationStatus>,
    pub deduplicated: bool,
    pub enqueued: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueReplicatedAcquireResult {
    pub policy: FabricQueuePolicySyncReport,
    pub mutation_sequence: Option<u64>,
    pub replication: Option<FabricStreamReplicationStatus>,
    pub delivery: Option<FabricQueueDelivery>,
    pub resumed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FabricQueueReplicatedGroupConfigResult {
    pub policy: FabricQueuePolicySyncReport,
    pub mutation_sequence: Option<u64>,
    pub replication: Option<FabricStreamReplicationStatus>,
    pub configured: bool,
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueReplicatedDeadLetterResult {
    pub source_policy: FabricQueuePolicySyncReport,
    pub target_queue: String,
    pub target_sequence: Option<u64>,
    pub target_replication: Option<FabricStreamReplicationStatus>,
    pub source_mutation_sequence: Option<u64>,
    pub source_replication: Option<FabricStreamReplicationStatus>,
    pub result: Option<FabricQueueNackResult>,
    pub forwarded: bool,
    pub resumed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FabricQueueReplicatedAckResult {
    pub policy: FabricQueuePolicySyncReport,
    pub mutation_sequence: Option<u64>,
    pub replication: Option<FabricStreamReplicationStatus>,
    pub completed: bool,
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueReplicatedNackResult {
    pub policy: FabricQueuePolicySyncReport,
    pub mutation_sequence: Option<u64>,
    pub replication: Option<FabricStreamReplicationStatus>,
    pub result: Option<FabricQueueNackResult>,
    pub resumed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FabricQueueReplicatedRenewResult {
    pub policy: FabricQueuePolicySyncReport,
    pub mutation_sequence: Option<u64>,
    pub replication: Option<FabricStreamReplicationStatus>,
    pub lease_until_ms: Option<u64>,
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricQueueReplicatedReapResult {
    pub policy: FabricQueuePolicySyncReport,
    pub mutation_sequence: Option<u64>,
    pub replication: Option<FabricStreamReplicationStatus>,
    pub expired_sequence: Option<u64>,
    pub result: Option<FabricQueueNackResult>,
    pub resumed: bool,
}

#[derive(Debug, Clone)]
struct PendingQueuePolicy {
    placement: FabricQueuePlacement,
    acknowledgements: HashSet<NodeId>,
}

#[derive(Debug, Default)]
pub(crate) struct FabricQueuePolicySyncState {
    pending: HashMap<(String, u64), PendingQueuePolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricQueuePolicyInstall {
    queue: String,
    partition: u16,
    epoch: u64,
    leader: u64,
    membership_fingerprint: u64,
    replication_factor: usize,
    replicas: Vec<u64>,
}

impl FabricQueuePolicyInstall {
    fn from_placement(placement: &FabricQueuePlacement) -> Self {
        Self {
            queue: placement.queue.clone(),
            partition: placement.partition,
            epoch: placement.epoch,
            leader: placement.leader.0,
            membership_fingerprint: placement.membership_fingerprint,
            replication_factor: placement.replicas.len(),
            replicas: placement.replicas.iter().map(|node| node.0).collect(),
        }
    }

    fn placement(&self) -> io::Result<FabricQueuePlacement> {
        validate_queue_name(&self.queue)?;
        if self.partition != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical Fabric queue partitions are not implemented yet; use partition 0",
            ));
        }
        if self.epoch == 0
            || self.replication_factor == 0
            || self.replication_factor != self.replicas.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Fabric queue policy envelope",
            ));
        }
        let mut unique = HashSet::with_capacity(self.replicas.len());
        if self.replicas.iter().any(|node| !unique.insert(*node)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric queue policy contains duplicate replicas",
            ));
        }
        if self.replicas.first().copied() != Some(self.leader) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric queue policy leader must be the first replica",
            ));
        }

        Ok(FabricQueuePlacement {
            queue: self.queue.clone(),
            partition: self.partition,
            epoch: self.epoch,
            leader: NodeId(self.leader),
            replicas: self.replicas.iter().copied().map(NodeId).collect(),
            membership_fingerprint: self.membership_fingerprint,
        })
    }

    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        let message: Self = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        message.placement()?;
        Ok(message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricQueuePolicyAck {
    queue: String,
    partition: u16,
    epoch: u64,
    leader: u64,
    membership_fingerprint: u64,
    replication_factor: usize,
    replica: u64,
    accepted: bool,
}

impl FabricQueuePolicyAck {
    pub(crate) fn accepted(install: &FabricQueuePolicyInstall, replica: NodeId) -> Self {
        Self {
            queue: install.queue.clone(),
            partition: install.partition,
            epoch: install.epoch,
            leader: install.leader,
            membership_fingerprint: install.membership_fingerprint,
            replication_factor: install.replication_factor,
            replica: replica.0,
            accepted: true,
        }
    }

    pub(crate) fn rejected(install: &FabricQueuePolicyInstall, replica: NodeId) -> Self {
        let mut ack = Self::accepted(install, replica);
        ack.accepted = false;
        ack
    }

    pub(crate) fn leader(&self) -> NodeId {
        NodeId(self.leader)
    }

    pub(crate) fn replica(&self) -> NodeId {
        NodeId(self.replica)
    }

    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        let ack: Self = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        validate_queue_name(&ack.queue)?;
        if ack.partition != 0
            || ack.epoch == 0
            || ack.replication_factor == 0
            || ack.replica == ack.leader
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Fabric queue policy ACK",
            ));
        }
        Ok(ack)
    }
}

impl Runtime {
    /// Return the installed queue-level placement if both internal streams are
    /// governed by the same replication policy.
    pub fn fabric_queue_replication_placement(
        &mut self,
        queue: &str,
    ) -> io::Result<Option<FabricQueuePlacement>> {
        validate_queue_name(queue)?;
        let payload = queue_stream_name(queue);
        let mutations = queue_mutation_stream_name(queue);
        let payload_policy = self.fabric_stream_replication_policy(&payload)?;
        let mutation_policy = self.fabric_stream_replication_policy(&mutations)?;

        match (payload_policy, mutation_policy) {
            (None, None) => Ok(None),
            (Some(left), Some(right)) if left == right => {
                Ok(Some(FabricQueuePlacement::from_policy(queue, &left)))
            }
            (Some(_), Some(_)) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Fabric queue {queue:?} has divergent payload and mutation replication policies"
                ),
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Fabric queue {queue:?} has a partially installed replication policy"),
            )),
        }
    }

    /// Establish the local queue policy, broadcast it to followers, and begin
    /// collecting application-level installation acknowledgements.
    ///
    /// RF=1 is immediately ready. RF>1 remains gated until every configured
    /// replica acknowledges the exact epoch/policy.
    pub fn fabric_queue_begin_replication(
        &mut self,
        queue: &str,
        partition: u16,
        replication_factor: usize,
    ) -> io::Result<FabricQueuePolicySyncReport> {
        let placement =
            self.fabric_queue_bootstrap_replication_policy(queue, partition, replication_factor)?;
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric queue replication requires distribution",
            )
        })?;
        if placement.leader != local {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric queue leader is {:?}, local node is {:?}",
                    placement.leader, local
                ),
            ));
        }

        let key = (queue.to_string(), placement.epoch);
        let pending = self
            .distributed
            .fabric_queue_policy_sync
            .pending
            .entry(key)
            .or_insert_with(|| PendingQueuePolicy {
                placement: placement.clone(),
                acknowledgements: HashSet::new(),
            });
        if pending.placement != placement {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pending Fabric queue policy differs from durable queue policy",
            ));
        }
        pending.acknowledgements.insert(local);

        let install = FabricQueuePolicyInstall::from_placement(&placement);
        let bytes = install.to_wire_bytes()?;
        let cluster = self.distributed.cluster.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric queue replication requires cluster membership",
            )
        })?;

        let mut report = FabricQueuePolicySyncReport {
            required: placement.replicas.len(),
            ..FabricQueuePolicySyncReport::default()
        };
        let mut targets = Vec::new();
        for replica in &placement.replicas {
            if *replica == local {
                continue;
            }
            report.intended_remote += 1;
            match cluster.get_node(*replica) {
                Some(info) if matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining) => {
                    targets.push((*replica, info.address));
                }
                _ => report.unavailable += 1,
            }
        }

        if !targets.is_empty() {
            let transport = self.distributed.transport.as_mut().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "Fabric queue replication requires a network transport",
                )
            })?;
            for (replica, address) in targets {
                transport.send(
                    replica,
                    address,
                    Packet::ActorMessage {
                        target_actor: 0,
                        behavior_name: FABRIC_QUEUE_POLICY_BEHAVIOR.to_string(),
                        content_hash: None,
                        payload: Vec::new(),
                        string_table: Vec::new(),
                        object_table: vec![(0, bytes.clone())],
                        sender_actor: 0,
                        sender_node: local,
                        priority: MessagePriority::System,
                        trace_id: None,
                    },
                );
                report.dispatched += 1;
            }
        }

        let status = self.fabric_queue_policy_sync_status(queue)?;
        report.acknowledgements = status.acknowledgements;
        report.ready = status.ready;
        Ok(report)
    }

    /// Retry-safe replicated queue creation.
    ///
    /// The shared queue policy is synchronized first. Once every configured
    /// replica has acknowledged that policy, QueueCreated is appended through
    /// the existing Fabric stream quorum path. Repeated calls resume sequence 1
    /// instead of appending duplicate creation records.
    pub fn fabric_queue_create_replicated(
        &mut self,
        queue: &str,
        config: FabricQueueConfig,
        partition: u16,
        replication_factor: usize,
    ) -> io::Result<FabricQueueReplicatedCreateResult> {
        let policy = self.fabric_queue_begin_replication(queue, partition, replication_factor)?;
        if !policy.ready {
            return Ok(FabricQueueReplicatedCreateResult {
                policy,
                mutation_sequence: None,
                replication: None,
                created: false,
            });
        }

        let mutation_stream = queue_mutation_stream_name(queue);
        let expected = encode_queue_created_mutation(&config)?;
        let info = self.fabric_stream_info(&mutation_stream)?;

        if info.last_sequence.is_some() {
            let record = self
                .fabric_stream_read(&mutation_stream, 1, 1)?
                .into_iter()
                .next()
                .filter(|record| record.sequence == 1)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Fabric queue mutation stream is missing sequence 1",
                    )
                })?;
            let existing = decode_queue_created_mutation(&record.payload)?;
            if existing != config {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Fabric queue {queue:?} already exists with different config"),
                ));
            }

            if info.committed_sequence >= 1 {
                let replication =
                    self.fabric_stream_replication_status(&mutation_stream, partition, 1)?;
                return Ok(FabricQueueReplicatedCreateResult {
                    policy,
                    mutation_sequence: Some(1),
                    replication: Some(replication),
                    created: true,
                });
            }

            // Reconstruct the durable pending ticket after a leader restart and
            // re-dispatch sequence 1. Exact-sequence replica application is
            // idempotent, so duplicate retries are safe.
            self.fabric_stream_retry_pending(&mutation_stream, partition)?;
            let replication =
                self.fabric_stream_replication_status(&mutation_stream, partition, 1)
                    .map_err(|error| {
                        if error.kind() == io::ErrorKind::NotFound {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "uncommitted Fabric QueueCreated record has no recoverable replication intent",
                            )
                        } else {
                            error
                        }
                    })?;
            return Ok(FabricQueueReplicatedCreateResult {
                policy,
                mutation_sequence: Some(1),
                replication: Some(replication),
                created: replication.committed,
            });
        }

        let appended = self.fabric_stream_replicated_append(
            &mutation_stream,
            partition,
            replication_factor,
            &expected,
        )?;
        if appended.sequence != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Fabric QueueCreated must be mutation sequence 1, got {}",
                    appended.sequence
                ),
            ));
        }
        Ok(FabricQueueReplicatedCreateResult {
            policy,
            mutation_sequence: Some(appended.sequence),
            replication: Some(appended.status),
            created: appended.status.committed,
        })
    }

    /// Replicate one immutable queue payload. A job becomes consumer-visible
    /// only after its payload sequence is quorum committed.
    ///
    /// Supplying a stable job_id makes retries idempotent: an existing raw
    /// payload record with that id is resumed and its durable replication
    /// intent is retried instead of appending a duplicate record.
    pub fn fabric_queue_add_replicated(
        &mut self,
        queue: &str,
        name: &str,
        payload: &[u8],
        options: FabricQueueAddOptions,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedAddResult> {
        let policy = self.fabric_queue_begin_replication(queue, partition, replication_factor)?;
        if !policy.ready {
            return Ok(FabricQueueReplicatedAddResult {
                policy,
                sequence: None,
                replication: None,
                deduplicated: false,
                enqueued: false,
            });
        }

        self.fabric_queue_require_committed_creation(queue)?;
        let payload_stream = queue_stream_name(queue);
        // Encode before dedup scanning so validation is identical for new and
        // retried calls.
        let bytes = encode_queue_envelope(queue, name, payload, &options, now_ms)?;

        if let Some(job_id) = options.job_id.as_deref() {
            let mut next = 1u64;
            let mut found = None;
            loop {
                let records = self.fabric_stream_read(&payload_stream, next, 1024)?;
                if records.is_empty() {
                    break;
                }
                for record in &records {
                    let envelope = decode_queue_envelope_bytes(&record.payload)?;
                    if envelope.job_id.as_deref() == Some(job_id) {
                        let existing_delay = envelope
                            .available_at_ms
                            .saturating_sub(envelope.created_at_ms);
                        if envelope.name != name
                            || envelope.payload.as_slice() != payload
                            || envelope.priority != options.priority
                            || existing_delay != options.delay_ms
                            || envelope.max_attempts != options.max_attempts
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "Fabric queue job id {job_id:?} was reused with different immutable job content"
                                ),
                            ));
                        }
                        if found.is_some() {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "Fabric queue {queue:?} contains duplicate durable job id {job_id:?}"
                                ),
                            ));
                        }
                        found = Some(record.sequence);
                    }
                    next = record.sequence.saturating_add(1);
                }
                if records.len() < 1024 {
                    break;
                }
            }

            if let Some(sequence) = found {
                let committed = self.fabric_stream_committed_sequence(&payload_stream)?;
                if sequence > committed {
                    self.fabric_stream_retry_pending(&payload_stream, partition)?;
                }
                let replication =
                    self.fabric_stream_replication_status(&payload_stream, partition, sequence)
                        .map_err(|error| {
                            if error.kind() == io::ErrorKind::NotFound {
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    format!(
                                        "uncommitted Fabric queue job {sequence} has no recoverable replication intent"
                                    ),
                                )
                            } else {
                                error
                            }
                        })?;
                return Ok(FabricQueueReplicatedAddResult {
                    policy,
                    sequence: Some(sequence),
                    replication: Some(replication),
                    deduplicated: true,
                    enqueued: replication.committed,
                });
            }
        }

        let appended = self.fabric_stream_replicated_append(
            &payload_stream,
            partition,
            replication_factor,
            &bytes,
        )?;
        Ok(FabricQueueReplicatedAddResult {
            policy,
            sequence: Some(appended.sequence),
            replication: Some(appended.status),
            deduplicated: false,
            enqueued: appended.status.committed,
        })
    }

    /// Prepare a DLQ target using the source queue's exact ownership policy.
    ///
    /// Co-ownership keeps source and target writes on one leader/replica set,
    /// which is required for the crash-safe handoff protocol below.
    pub fn fabric_queue_prepare_dead_letter_target_replicated(
        &mut self,
        source_queue: &str,
        target_queue: &str,
        target_config: FabricQueueConfig,
        partition: u16,
        replication_factor: usize,
    ) -> io::Result<FabricQueueReplicatedCreateResult> {
        validate_queue_name(source_queue)?;
        validate_queue_name(target_queue)?;
        if source_queue == target_queue {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric queue cannot use itself as its dead-letter target",
            ));
        }

        let source_policy =
            self.fabric_queue_begin_replication(source_queue, partition, replication_factor)?;
        if !source_policy.ready {
            return Ok(FabricQueueReplicatedCreateResult {
                policy: source_policy,
                mutation_sequence: None,
                replication: None,
                created: false,
            });
        }
        let source_config = self.fabric_queue_require_committed_creation(source_queue)?;
        if source_config.dead_letter_queue.as_deref() != Some(target_queue) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Fabric queue {source_queue:?} is not configured to dead-letter into {target_queue:?}"
                ),
            ));
        }

        let source_placement = self
            .fabric_queue_replication_placement(source_queue)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "source Fabric queue replication policy disappeared",
                )
            })?;
        let target_placement = FabricQueuePlacement {
            queue: target_queue.to_string(),
            partition: source_placement.partition,
            epoch: source_placement.epoch,
            leader: source_placement.leader,
            replicas: source_placement.replicas.clone(),
            membership_fingerprint: source_placement.membership_fingerprint,
        };
        self.fabric_queue_install_exact_policy(&target_placement)?;
        self.fabric_queue_create_replicated(
            target_queue,
            target_config,
            partition,
            replication_factor,
        )
    }

    fn fabric_queue_forward_dead_letter_target(
        &mut self,
        source_queue: &str,
        plan: &FabricQueueDeadLetterPlan,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedAddResult> {
        let source_placement = self
            .fabric_queue_replication_placement(source_queue)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "source queue policy missing")
            })?;
        let target_placement = self
            .fabric_queue_replication_placement(&plan.target_queue)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "Fabric DLQ target {:?} is not prepared; call fabric_queue_prepare_dead_letter_target_replicated first",
                        plan.target_queue
                    ),
                )
            })?;
        if source_placement.partition != target_placement.partition
            || source_placement.epoch != target_placement.epoch
            || source_placement.leader != target_placement.leader
            || source_placement.replicas != target_placement.replicas
            || source_placement.membership_fingerprint != target_placement.membership_fingerprint
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Fabric DLQ target {:?} does not share source queue ownership",
                    plan.target_queue
                ),
            ));
        }
        self.fabric_queue_require_committed_creation(&plan.target_queue)?;
        self.fabric_queue_add_replicated(
            &plan.target_queue,
            &plan.target_name,
            &plan.target_payload,
            plan.target_options.clone(),
            partition,
            replication_factor,
            now_ms,
        )
    }

    /// Crash-safe DLQ handoff for an exhausted active delivery.
    ///
    /// This is deliberately not described as a cross-queue transaction. The
    /// target payload reaches quorum first using a deterministic job id. Only
    /// then may the source queue append its DeadLettered mutation. A crash
    /// between those phases is retry-safe: target enqueue deduplicates, and the
    /// source remains Active until terminalization itself reaches quorum.
    pub fn fabric_queue_dead_letter_replicated(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: &str,
        error: Option<&str>,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedDeadLetterResult> {
        validate_consumer_name(consumer)?;
        validate_operation_id(operation_id)?;
        let source_policy =
            self.fabric_queue_begin_replication(queue, partition, replication_factor)?;
        let source_config = self.fabric_queue_require_committed_creation(queue)?;
        let target_queue = source_config.dead_letter_queue.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Fabric queue {queue:?} has no dead-letter target"),
            )
        })?;

        if let Some((mutation_sequence, operation)) =
            self.fabric_queue_find_operation(&queue_mutation_stream_name(queue), operation_id)?
        {
            Self::fabric_queue_validate_operation_match(
                &operation,
                FabricQueueOperationKind::Nack,
                sequence,
                consumer,
                queue_epoch,
                lease_token,
            )?;
            if operation.status != Some(super::fabric_queue::FabricQueueJobStatus::DeadLettered) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric queue dead-letter operation id refers to a non-DLQ NACK",
                ));
            }
            let info = self.fabric_stream_info(&queue_mutation_stream_name(queue))?;
            let replication = self.fabric_queue_resume_metadata_replication(
                &queue_mutation_stream_name(queue),
                partition,
                mutation_sequence,
                info.committed_sequence,
            )?;
            return Ok(FabricQueueReplicatedDeadLetterResult {
                source_policy,
                target_queue,
                target_sequence: None,
                target_replication: None,
                source_mutation_sequence: Some(mutation_sequence),
                source_replication: Some(replication),
                result: replication.committed.then_some(FabricQueueNackResult {
                    status: super::fabric_queue::FabricQueueJobStatus::DeadLettered,
                    deliveries: self
                        .fabric_queue_committed_job_snapshot(queue, sequence)?
                        .deliveries,
                    available_at_ms: None,
                }),
                forwarded: replication.committed,
                resumed: true,
            });
        }

        if !source_policy.ready {
            return Ok(FabricQueueReplicatedDeadLetterResult {
                source_policy,
                target_queue,
                target_sequence: None,
                target_replication: None,
                source_mutation_sequence: None,
                source_replication: None,
                result: None,
                forwarded: false,
                resumed: false,
            });
        }

        let plan = self
            .fabric_queue_plan_committed_dead_letter_nack(
                queue,
                sequence,
                consumer,
                queue_epoch,
                lease_token,
                operation_id,
                error,
                now_ms,
            )?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Fabric queue delivery has not exhausted attempts or has no DLQ target",
                )
            })?;

        let target = self.fabric_queue_forward_dead_letter_target(
            queue,
            &plan,
            partition,
            replication_factor,
            now_ms,
        )?;
        if !target.enqueued {
            return Ok(FabricQueueReplicatedDeadLetterResult {
                source_policy,
                target_queue: plan.target_queue,
                target_sequence: target.sequence,
                target_replication: target.replication,
                source_mutation_sequence: None,
                source_replication: None,
                result: None,
                forwarded: false,
                resumed: target.deduplicated,
            });
        }

        let mutation_stream = queue_mutation_stream_name(queue);
        let source_info = self.fabric_stream_info(&mutation_stream)?;
        if source_info.last_sequence.unwrap_or(0) > source_info.committed_sequence {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric queue {queue:?} has an uncommitted metadata mutation; retry it before finalizing DLQ handoff"
                ),
            ));
        }
        let appended = self.fabric_stream_replicated_append(
            &mutation_stream,
            partition,
            replication_factor,
            &plan.source_mutation_bytes,
        )?;
        Ok(FabricQueueReplicatedDeadLetterResult {
            source_policy,
            target_queue: plan.target_queue,
            target_sequence: target.sequence,
            target_replication: target.replication,
            source_mutation_sequence: Some(appended.sequence),
            source_replication: Some(appended.status),
            result: appended.status.committed.then_some(plan.result),
            forwarded: appended.status.committed,
            resumed: target.deduplicated,
        })
    }

    /// Configure an immutable durable worker concurrency domain.
    ///
    /// This is a work-queue concurrency cap, not a fan-out subscription group.
    /// Repeating the same configuration resumes or reuses the original durable
    /// metadata mutation; changing max_concurrency requires an explicit future
    /// reconfiguration primitive rather than silently rewriting history.
    pub fn fabric_queue_configure_consumer_group_replicated(
        &mut self,
        queue: &str,
        group: &str,
        max_concurrency: usize,
        partition: u16,
        replication_factor: usize,
    ) -> io::Result<FabricQueueReplicatedGroupConfigResult> {
        let config = FabricQueueConsumerGroupConfig::new(group, max_concurrency)?;
        let policy = self.fabric_queue_begin_replication(queue, partition, replication_factor)?;
        if !policy.ready {
            return Ok(FabricQueueReplicatedGroupConfigResult {
                policy,
                mutation_sequence: None,
                replication: None,
                configured: false,
                resumed: false,
            });
        }
        self.fabric_queue_require_committed_creation(queue)?;
        let mutation_stream = queue_mutation_stream_name(queue);
        let info = self.fabric_stream_info(&mutation_stream)?;

        let mut next = 2u64;
        let mut found = None;
        loop {
            let records = self.fabric_stream_read(&mutation_stream, next, 1024)?;
            if records.is_empty() {
                break;
            }
            for record in &records {
                if let Some(existing) = decode_queue_consumer_group_config(&record.payload)? {
                    if existing.name == group {
                        if found.is_some() {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "Fabric queue {queue:?} contains duplicate consumer-group configuration for {group:?}"
                                ),
                            ));
                        }
                        if existing != config {
                            return Err(io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                format!(
                                    "Fabric queue consumer group {group:?} is already configured with max_concurrency {}",
                                    existing.max_concurrency
                                ),
                            ));
                        }
                        found = Some(record.sequence);
                    }
                }
                next = record.sequence.saturating_add(1);
            }
            if records.len() < 1024 {
                break;
            }
        }

        if let Some(sequence) = found {
            if sequence > info.committed_sequence {
                self.fabric_stream_retry_pending(&mutation_stream, partition)?;
            }
            let replication = self.fabric_stream_replication_status(
                &mutation_stream,
                partition,
                sequence,
            )
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "uncommitted Fabric queue consumer-group configuration {sequence} has no recoverable replication intent"
                        ),
                    )
                } else {
                    error
                }
            })?;
            return Ok(FabricQueueReplicatedGroupConfigResult {
                policy,
                mutation_sequence: Some(sequence),
                replication: Some(replication),
                configured: replication.committed,
                resumed: true,
            });
        }

        if info.last_sequence.unwrap_or(0) > info.committed_sequence {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric queue {queue:?} has an uncommitted metadata mutation; retry it before configuring consumer group {group:?}"
                ),
            ));
        }

        let bytes = self.fabric_queue_encode_consumer_group_config(queue, &config)?;
        let appended = self.fabric_stream_replicated_append(
            &mutation_stream,
            partition,
            replication_factor,
            &bytes,
        )?;
        Ok(FabricQueueReplicatedGroupConfigResult {
            policy,
            mutation_sequence: Some(appended.sequence),
            replication: Some(appended.status),
            configured: appended.status.committed,
            resumed: false,
        })
    }

    pub fn fabric_queue_consumer_group_info_replicated(
        &mut self,
        queue: &str,
        group: &str,
    ) -> io::Result<FabricQueueConsumerGroupInfo> {
        self.fabric_queue_replication_placement(queue)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("Fabric queue {queue:?} does not have a replication policy"),
                )
            })?;
        self.fabric_queue_committed_consumer_group_info(queue, group)
    }

    fn fabric_queue_find_operation(
        &mut self,
        mutation_stream: &str,
        operation_id: &str,
    ) -> io::Result<Option<(u64, FabricQueueOperation)>> {
        let mut next = 2u64;
        let mut matching = None;
        loop {
            let records = self.fabric_stream_read(mutation_stream, next, 1024)?;
            if records.is_empty() {
                break;
            }
            for record in &records {
                if let Some(operation) = decode_queue_operation(&record.payload)? {
                    if operation.operation_id.as_deref() == Some(operation_id) {
                        if matching.is_some() {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "Fabric queue metadata contains duplicate operation id {operation_id:?}"
                                ),
                            ));
                        }
                        matching = Some((record.sequence, operation));
                    }
                }
                next = record.sequence.saturating_add(1);
            }
            if records.len() < 1024 {
                break;
            }
        }
        Ok(matching)
    }

    fn fabric_queue_resume_metadata_replication(
        &mut self,
        mutation_stream: &str,
        partition: u16,
        mutation_sequence: u64,
        committed: u64,
    ) -> io::Result<FabricStreamReplicationStatus> {
        if mutation_sequence > committed {
            self.fabric_stream_retry_pending(mutation_stream, partition)?;
        }
        self.fabric_stream_replication_status(
            mutation_stream,
            partition,
            mutation_sequence,
        )
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "uncommitted Fabric queue metadata mutation {mutation_sequence} has no recoverable replication intent"
                    ),
                )
            } else {
                error
            }
        })
    }

    fn fabric_queue_validate_operation_match(
        operation: &FabricQueueOperation,
        expected_kind: FabricQueueOperationKind,
        sequence: u64,
        consumer: &str,
        queue_epoch: u64,
        lease_token: u64,
    ) -> io::Result<()> {
        if operation.kind != expected_kind
            || operation.sequence != sequence
            || operation.consumer != consumer
            || operation.queue_epoch != queue_epoch
            || operation.lease_token != lease_token
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric queue operation id was reused with different mutation or fencing inputs",
            ));
        }
        Ok(())
    }

    /// Acquire one job through a quorum-committed, epoch-fenced lease.
    ///
    /// The leader serializes queue metadata mutations: while a lease mutation
    /// is uncommitted, only a retry carrying the same operation_id may resume
    /// it. A different acquire fails with WouldBlock rather than competing for
    /// the same committed queue state.
    pub fn fabric_queue_acquire_replicated(
        &mut self,
        queue: &str,
        consumer: &str,
        operation_id: &str,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedAcquireResult> {
        self.fabric_queue_acquire_replicated_inner(
            queue,
            consumer,
            None,
            operation_id,
            None,
            partition,
            replication_factor,
            now_ms,
        )
    }

    pub fn fabric_queue_acquire_replicated_with_lease_duration(
        &mut self,
        queue: &str,
        consumer: &str,
        operation_id: &str,
        lease_duration_ms: u64,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedAcquireResult> {
        self.fabric_queue_acquire_replicated_inner(
            queue,
            consumer,
            None,
            operation_id,
            Some(lease_duration_ms),
            partition,
            replication_factor,
            now_ms,
        )
    }

    pub fn fabric_queue_acquire_consumer_group_replicated(
        &mut self,
        queue: &str,
        group: &str,
        consumer: &str,
        operation_id: &str,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedAcquireResult> {
        validate_consumer_group_name(group)?;
        self.fabric_queue_acquire_replicated_inner(
            queue,
            consumer,
            Some(group),
            operation_id,
            None,
            partition,
            replication_factor,
            now_ms,
        )
    }

    pub fn fabric_queue_acquire_consumer_group_replicated_with_lease_duration(
        &mut self,
        queue: &str,
        group: &str,
        consumer: &str,
        operation_id: &str,
        lease_duration_ms: u64,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedAcquireResult> {
        validate_consumer_group_name(group)?;
        self.fabric_queue_acquire_replicated_inner(
            queue,
            consumer,
            Some(group),
            operation_id,
            Some(lease_duration_ms),
            partition,
            replication_factor,
            now_ms,
        )
    }

    fn fabric_queue_acquire_replicated_inner(
        &mut self,
        queue: &str,
        consumer: &str,
        consumer_group: Option<&str>,
        operation_id: &str,
        lease_duration_ms: Option<u64>,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedAcquireResult> {
        validate_consumer_name(consumer)?;
        if operation_id.is_empty() || operation_id.len() > 256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric queue acquire operation ids must be 1..=256 bytes",
            ));
        }

        let policy = self.fabric_queue_begin_replication(queue, partition, replication_factor)?;
        if !policy.ready {
            return Ok(FabricQueueReplicatedAcquireResult {
                policy,
                mutation_sequence: None,
                replication: None,
                delivery: None,
                resumed: false,
            });
        }
        self.fabric_queue_require_committed_creation(queue)?;
        let placement = self
            .fabric_queue_replication_placement(queue)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric queue replication policy disappeared during acquire",
                )
            })?;
        let mutation_stream = queue_mutation_stream_name(queue);
        let info = self.fabric_stream_info(&mutation_stream)?;
        let committed = info.committed_sequence;

        let mut next = 2u64;
        let mut matching = None;
        loop {
            let records = self.fabric_stream_read(&mutation_stream, next, 1024)?;
            if records.is_empty() {
                break;
            }
            for record in &records {
                if let Some(lease) = decode_queue_lease_mutation(&record.payload)? {
                    if lease.operation_id.as_deref() == Some(operation_id) {
                        if matching.is_some() {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "Fabric queue {queue:?} contains duplicate acquire operation id {operation_id:?}"
                                ),
                            ));
                        }
                        if lease.consumer != consumer
                            || lease.consumer_group.as_deref() != consumer_group
                            || lease.queue_epoch != placement.epoch
                            || lease_duration_ms
                                .is_some_and(|duration| duration != lease.lease_duration_ms)
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Fabric queue acquire operation id was reused with different fencing inputs",
                            ));
                        }
                        matching = Some((record.sequence, lease));
                    }
                }
                next = record.sequence.saturating_add(1);
            }
            if records.len() < 1024 {
                break;
            }
        }

        if let Some((mutation_sequence, lease)) = matching {
            if mutation_sequence > committed {
                self.fabric_stream_retry_pending(&mutation_stream, partition)?;
            }
            let replication = self
                .fabric_stream_replication_status(
                    &mutation_stream,
                    partition,
                    mutation_sequence,
                )
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::NotFound {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "uncommitted Fabric queue lease mutation {mutation_sequence} has no recoverable replication intent"
                            ),
                        )
                    } else {
                        error
                    }
                })?;
            let delivery = if replication.committed {
                Some(self.fabric_queue_delivery_for_committed_lease(queue, &lease)?)
            } else {
                None
            };
            return Ok(FabricQueueReplicatedAcquireResult {
                policy,
                mutation_sequence: Some(mutation_sequence),
                replication: Some(replication),
                delivery,
                resumed: true,
            });
        }

        if info.last_sequence.unwrap_or(0) > committed {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric queue {queue:?} has an uncommitted metadata mutation; retry that operation before acquiring another job"
                ),
            ));
        }

        let Some(plan) = self.fabric_queue_plan_committed_lease(
            queue,
            consumer,
            consumer_group,
            operation_id,
            placement.epoch,
            lease_duration_ms,
            now_ms,
        )?
        else {
            return Ok(FabricQueueReplicatedAcquireResult {
                policy,
                mutation_sequence: None,
                replication: None,
                delivery: None,
                resumed: false,
            });
        };

        let appended = self.fabric_stream_replicated_append(
            &mutation_stream,
            partition,
            replication_factor,
            &plan.mutation_bytes,
        )?;
        let delivery = if appended.status.committed {
            Some(plan.delivery)
        } else {
            None
        };
        Ok(FabricQueueReplicatedAcquireResult {
            policy,
            mutation_sequence: Some(appended.sequence),
            replication: Some(appended.status),
            delivery,
            resumed: false,
        })
    }

    pub fn fabric_queue_ack_replicated(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: &str,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedAckResult> {
        validate_consumer_name(consumer)?;
        validate_operation_id(operation_id)?;
        let policy = self.fabric_queue_begin_replication(queue, partition, replication_factor)?;
        if !policy.ready {
            return Ok(FabricQueueReplicatedAckResult {
                policy,
                mutation_sequence: None,
                replication: None,
                completed: false,
                resumed: false,
            });
        }
        let placement = self
            .fabric_queue_replication_placement(queue)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Fabric queue policy missing")
            })?;
        if queue_epoch != placement.epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric queue ACK uses a stale queue epoch",
            ));
        }
        self.fabric_queue_require_committed_creation(queue)?;
        let mutation_stream = queue_mutation_stream_name(queue);
        let info = self.fabric_stream_info(&mutation_stream)?;

        if let Some((mutation_sequence, operation)) =
            self.fabric_queue_find_operation(&mutation_stream, operation_id)?
        {
            Self::fabric_queue_validate_operation_match(
                &operation,
                FabricQueueOperationKind::Ack,
                sequence,
                consumer,
                queue_epoch,
                lease_token,
            )?;
            let replication = self.fabric_queue_resume_metadata_replication(
                &mutation_stream,
                partition,
                mutation_sequence,
                info.committed_sequence,
            )?;
            return Ok(FabricQueueReplicatedAckResult {
                policy,
                mutation_sequence: Some(mutation_sequence),
                replication: Some(replication),
                completed: replication.committed,
                resumed: true,
            });
        }

        if info.last_sequence.unwrap_or(0) > info.committed_sequence {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric queue {queue:?} has an uncommitted metadata mutation; retry it before ACK"
                ),
            ));
        }
        let bytes = self.fabric_queue_plan_committed_ack(
            queue,
            sequence,
            consumer,
            queue_epoch,
            lease_token,
            operation_id,
            None,
            now_ms,
        )?;
        let appended = self.fabric_stream_replicated_append(
            &mutation_stream,
            partition,
            replication_factor,
            &bytes,
        )?;
        Ok(FabricQueueReplicatedAckResult {
            policy,
            mutation_sequence: Some(appended.sequence),
            replication: Some(appended.status),
            completed: appended.status.committed,
            resumed: false,
        })
    }

    pub fn fabric_queue_nack_replicated(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: &str,
        delay_ms: u64,
        error: Option<&str>,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedNackResult> {
        validate_consumer_name(consumer)?;
        validate_operation_id(operation_id)?;
        let policy = self.fabric_queue_begin_replication(queue, partition, replication_factor)?;
        if !policy.ready {
            return Ok(FabricQueueReplicatedNackResult {
                policy,
                mutation_sequence: None,
                replication: None,
                result: None,
                resumed: false,
            });
        }
        let placement = self
            .fabric_queue_replication_placement(queue)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Fabric queue policy missing")
            })?;
        if queue_epoch != placement.epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric queue NACK uses a stale queue epoch",
            ));
        }
        self.fabric_queue_require_committed_creation(queue)?;
        let mutation_stream = queue_mutation_stream_name(queue);
        let info = self.fabric_stream_info(&mutation_stream)?;

        if let Some((mutation_sequence, operation)) =
            self.fabric_queue_find_operation(&mutation_stream, operation_id)?
        {
            Self::fabric_queue_validate_operation_match(
                &operation,
                FabricQueueOperationKind::Nack,
                sequence,
                consumer,
                queue_epoch,
                lease_token,
            )?;
            let replication = self.fabric_queue_resume_metadata_replication(
                &mutation_stream,
                partition,
                mutation_sequence,
                info.committed_sequence,
            )?;
            let result = if replication.committed {
                let snapshot = self.fabric_queue_committed_job_snapshot(queue, sequence)?;
                let expected_status = operation.status.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "committed Fabric queue NACK is missing target status",
                    )
                })?;
                if snapshot.status != expected_status {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "committed Fabric queue state does not match NACK mutation",
                    ));
                }
                Some(FabricQueueNackResult {
                    status: snapshot.status,
                    deliveries: snapshot.deliveries,
                    available_at_ms: operation.available_at_ms,
                })
            } else {
                None
            };
            return Ok(FabricQueueReplicatedNackResult {
                policy,
                mutation_sequence: Some(mutation_sequence),
                replication: Some(replication),
                result,
                resumed: true,
            });
        }

        if info.last_sequence.unwrap_or(0) > info.committed_sequence {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric queue {queue:?} has an uncommitted metadata mutation; retry it before NACK"
                ),
            ));
        }
        if self
            .fabric_queue_plan_committed_dead_letter_nack(
                queue,
                sequence,
                consumer,
                queue_epoch,
                lease_token,
                operation_id,
                error,
                now_ms,
            )?
            .is_some()
        {
            let dlq = self.fabric_queue_dead_letter_replicated(
                queue,
                sequence,
                consumer,
                queue_epoch,
                lease_token,
                operation_id,
                error,
                partition,
                replication_factor,
                now_ms,
            )?;
            return Ok(FabricQueueReplicatedNackResult {
                policy: dlq.source_policy,
                mutation_sequence: dlq.source_mutation_sequence,
                replication: dlq.source_replication,
                result: dlq.result,
                resumed: dlq.resumed,
            });
        }

        let (bytes, planned_result) = self.fabric_queue_plan_committed_nack(
            queue,
            sequence,
            consumer,
            queue_epoch,
            lease_token,
            operation_id,
            delay_ms,
            error,
            now_ms,
        )?;
        let appended = self.fabric_stream_replicated_append(
            &mutation_stream,
            partition,
            replication_factor,
            &bytes,
        )?;
        Ok(FabricQueueReplicatedNackResult {
            policy,
            mutation_sequence: Some(appended.sequence),
            replication: Some(appended.status),
            result: appended.status.committed.then_some(planned_result),
            resumed: false,
        })
    }

    pub fn fabric_queue_renew_replicated(
        &mut self,
        queue: &str,
        sequence: u64,
        consumer: &str,
        queue_epoch: u64,
        lease_token: u64,
        operation_id: &str,
        extension_ms: u64,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedRenewResult> {
        validate_consumer_name(consumer)?;
        validate_operation_id(operation_id)?;
        let policy = self.fabric_queue_begin_replication(queue, partition, replication_factor)?;
        if !policy.ready {
            return Ok(FabricQueueReplicatedRenewResult {
                policy,
                mutation_sequence: None,
                replication: None,
                lease_until_ms: None,
                resumed: false,
            });
        }
        let placement = self
            .fabric_queue_replication_placement(queue)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Fabric queue policy missing")
            })?;
        if queue_epoch != placement.epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric queue renew uses a stale queue epoch",
            ));
        }
        self.fabric_queue_require_committed_creation(queue)?;
        let mutation_stream = queue_mutation_stream_name(queue);
        let info = self.fabric_stream_info(&mutation_stream)?;

        if let Some((mutation_sequence, operation)) =
            self.fabric_queue_find_operation(&mutation_stream, operation_id)?
        {
            Self::fabric_queue_validate_operation_match(
                &operation,
                FabricQueueOperationKind::Renew,
                sequence,
                consumer,
                queue_epoch,
                lease_token,
            )?;
            let replication = self.fabric_queue_resume_metadata_replication(
                &mutation_stream,
                partition,
                mutation_sequence,
                info.committed_sequence,
            )?;
            return Ok(FabricQueueReplicatedRenewResult {
                policy,
                mutation_sequence: Some(mutation_sequence),
                replication: Some(replication),
                lease_until_ms: replication
                    .committed
                    .then_some(operation.lease_until_ms)
                    .flatten(),
                resumed: true,
            });
        }

        if info.last_sequence.unwrap_or(0) > info.committed_sequence {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric queue {queue:?} has an uncommitted metadata mutation; retry it before renew"
                ),
            ));
        }
        let (bytes, lease_until_ms) = self.fabric_queue_plan_committed_renew(
            queue,
            sequence,
            consumer,
            queue_epoch,
            lease_token,
            operation_id,
            extension_ms,
            now_ms,
        )?;
        let appended = self.fabric_stream_replicated_append(
            &mutation_stream,
            partition,
            replication_factor,
            &bytes,
        )?;
        Ok(FabricQueueReplicatedRenewResult {
            policy,
            mutation_sequence: Some(appended.sequence),
            replication: Some(appended.status),
            lease_until_ms: appended.status.committed.then_some(lease_until_ms),
            resumed: false,
        })
    }

    /// Replicate the next due lease-expiry transition.
    ///
    /// Expiry is a queue metadata mutation, not a local wall-clock side effect.
    /// The job remains Active until LeaseExpired reaches quorum, at which point
    /// it becomes Waiting or terminal according to max_attempts/DLQ policy.
    pub fn fabric_queue_reap_expired_replicated(
        &mut self,
        queue: &str,
        partition: u16,
        replication_factor: usize,
        now_ms: u64,
    ) -> io::Result<FabricQueueReplicatedReapResult> {
        let policy = self.fabric_queue_begin_replication(queue, partition, replication_factor)?;
        if !policy.ready {
            return Ok(FabricQueueReplicatedReapResult {
                policy,
                mutation_sequence: None,
                replication: None,
                expired_sequence: None,
                result: None,
                resumed: false,
            });
        }

        self.fabric_queue_require_committed_creation(queue)?;
        let placement = self
            .fabric_queue_replication_placement(queue)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Fabric queue policy missing")
            })?;
        let mutation_stream = queue_mutation_stream_name(queue);
        let info = self.fabric_stream_info(&mutation_stream)?;
        let tail = info.last_sequence.unwrap_or(0);
        let committed = info.committed_sequence;

        if tail > committed {
            if tail != committed.saturating_add(1) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Fabric queue {queue:?} has more than one uncommitted metadata mutation"
                    ),
                ));
            }
            let record = self
                .fabric_stream_read(&mutation_stream, committed.saturating_add(1), 1)?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Fabric queue metadata tail disappeared during expiry recovery",
                    )
                })?;
            let operation = decode_queue_operation(&record.payload)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Fabric queue has an uncommitted non-worker metadata mutation",
                )
            })?;
            if operation.kind != FabricQueueOperationKind::Expire {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "Fabric queue {queue:?} has an uncommitted {:?} operation; retry it before lease expiry",
                        operation.kind
                    ),
                ));
            }
            if operation.queue_epoch != placement.epoch {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "pending Fabric queue expiry uses a stale queue epoch",
                ));
            }

            let replication = self.fabric_queue_resume_metadata_replication(
                &mutation_stream,
                partition,
                record.sequence,
                committed,
            )?;
            let result = if replication.committed {
                let snapshot =
                    self.fabric_queue_committed_job_snapshot(queue, operation.sequence)?;
                let expected_status = operation.status.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "committed Fabric queue expiry is missing target status",
                    )
                })?;
                if snapshot.status != expected_status {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "committed Fabric queue state does not match lease expiry mutation",
                    ));
                }
                Some(FabricQueueNackResult {
                    status: snapshot.status,
                    deliveries: snapshot.deliveries,
                    available_at_ms: operation.available_at_ms,
                })
            } else {
                None
            };
            return Ok(FabricQueueReplicatedReapResult {
                policy,
                mutation_sequence: Some(record.sequence),
                replication: Some(replication),
                expired_sequence: Some(operation.sequence),
                result,
                resumed: true,
            });
        }

        if let Some(dead_letter) =
            self.fabric_queue_plan_committed_dead_letter_expiry(queue, placement.epoch, now_ms)?
        {
            let target = self.fabric_queue_forward_dead_letter_target(
                queue,
                &dead_letter,
                partition,
                replication_factor,
                now_ms,
            )?;
            if !target.enqueued {
                return Ok(FabricQueueReplicatedReapResult {
                    policy,
                    mutation_sequence: None,
                    replication: None,
                    expired_sequence: Some(dead_letter.source_sequence),
                    result: None,
                    resumed: target.deduplicated,
                });
            }

            let source_info = self.fabric_stream_info(&mutation_stream)?;
            if source_info.last_sequence.unwrap_or(0) > source_info.committed_sequence {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "Fabric queue {queue:?} has an uncommitted metadata mutation; retry it before finalizing expired DLQ handoff"
                    ),
                ));
            }
            let appended = self.fabric_stream_replicated_append(
                &mutation_stream,
                partition,
                replication_factor,
                &dead_letter.source_mutation_bytes,
            )?;
            return Ok(FabricQueueReplicatedReapResult {
                policy,
                mutation_sequence: Some(appended.sequence),
                replication: Some(appended.status),
                expired_sequence: Some(dead_letter.source_sequence),
                result: appended.status.committed.then_some(dead_letter.result),
                resumed: target.deduplicated,
            });
        }

        let Some(plan) = self.fabric_queue_plan_committed_expiry(queue, placement.epoch, now_ms)?
        else {
            return Ok(FabricQueueReplicatedReapResult {
                policy,
                mutation_sequence: None,
                replication: None,
                expired_sequence: None,
                result: None,
                resumed: false,
            });
        };

        let appended = self.fabric_stream_replicated_append(
            &mutation_stream,
            partition,
            replication_factor,
            &plan.mutation_bytes,
        )?;
        Ok(FabricQueueReplicatedReapResult {
            policy,
            mutation_sequence: Some(appended.sequence),
            replication: Some(appended.status),
            expired_sequence: Some(plan.sequence),
            result: appended.status.committed.then_some(plan.result),
            resumed: false,
        })
    }

    fn fabric_queue_require_committed_creation(
        &mut self,
        queue: &str,
    ) -> io::Result<FabricQueueConfig> {
        let mutation_stream = queue_mutation_stream_name(queue);
        let first = self.fabric_stream_read_committed(&mutation_stream, 1, 1)?;
        let first = first.first().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric queue {queue:?} cannot accept jobs before QueueCreated is quorum committed"
                ),
            )
        })?;
        if first.sequence != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "committed Fabric queue metadata does not start at sequence 1",
            ));
        }
        decode_queue_created_mutation(&first.payload)
    }

    pub fn fabric_queue_policy_sync_status(
        &mut self,
        queue: &str,
    ) -> io::Result<FabricQueuePolicySyncReport> {
        let Some(placement) = self.fabric_queue_replication_placement(queue)? else {
            return Ok(FabricQueuePolicySyncReport::default());
        };
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric queue replication requires distribution",
            )
        })?;
        let acknowledgements = if placement.replicas.len() == 1 && placement.leader == local {
            1
        } else {
            self.distributed
                .fabric_queue_policy_sync
                .pending
                .get(&(queue.to_string(), placement.epoch))
                .filter(|pending| pending.placement == placement)
                .map(|pending| pending.acknowledgements.len())
                .unwrap_or(0)
        };
        Ok(FabricQueuePolicySyncReport {
            acknowledgements,
            required: placement.replicas.len(),
            ready: acknowledgements == placement.replicas.len(),
            ..FabricQueuePolicySyncReport::default()
        })
    }

    pub fn fabric_queue_replication_ready(&mut self, queue: &str) -> io::Result<bool> {
        Ok(self.fabric_queue_policy_sync_status(queue)?.ready)
    }

    pub(crate) fn fabric_queue_has_replication_policy(&mut self, queue: &str) -> io::Result<bool> {
        validate_queue_name(queue)?;
        for stream in [queue_stream_name(queue), queue_mutation_stream_name(queue)] {
            match self.fabric_stream_replication_policy(&stream) {
                Ok(Some(_)) => return Ok(true),
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    pub(crate) fn fabric_queue_apply_policy_from_cluster(
        &mut self,
        install: &FabricQueuePolicyInstall,
        from_node: NodeId,
    ) -> io::Result<()> {
        let placement = install.placement()?;
        if from_node != placement.leader {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric queue policy sender is not the declared leader",
            ));
        }
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric queue policy install requires distribution",
            )
        })?;
        if local == placement.leader || !placement.replicas.contains(&local) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local node is not an authorized Fabric queue follower",
            ));
        }

        if let Some(cluster) = self.distributed.cluster.as_ref() {
            for replica in &placement.replicas {
                let known = cluster.get_node(*replica).is_some() || *replica == local;
                if !known {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Fabric queue policy references unknown replica {:?}",
                            replica
                        ),
                    ));
                }
            }
        }

        self.fabric_queue_install_exact_policy(&placement)
    }

    pub(crate) fn fabric_queue_record_policy_ack(
        &mut self,
        ack: FabricQueuePolicyAck,
        from_node: NodeId,
    ) -> io::Result<FabricQueuePolicySyncReport> {
        if ack.replica() != from_node {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric queue policy ACK replica does not match transport peer",
            ));
        }
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric queue policy ACK requires distribution",
            )
        })?;
        if ack.leader() != local {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric queue policy ACK targets a different leader",
            ));
        }

        let placement = self
            .fabric_queue_replication_placement(&ack.queue)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric queue policy ACK has no installed queue policy",
                )
            })?;
        if ack.partition != placement.partition
            || ack.epoch != placement.epoch
            || ack.membership_fingerprint != placement.membership_fingerprint
            || ack.replication_factor != placement.replicas.len()
            || !placement.replicas.contains(&ack.replica())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stale or unauthorized Fabric queue policy ACK",
            ));
        }

        let key = (ack.queue.clone(), ack.epoch);
        let pending = self
            .distributed
            .fabric_queue_policy_sync
            .pending
            .entry(key)
            .or_insert_with(|| {
                let mut acknowledgements = HashSet::new();
                acknowledgements.insert(local);
                PendingQueuePolicy {
                    placement: placement.clone(),
                    acknowledgements,
                }
            });
        if pending.placement != placement {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric queue policy ACK conflicts with pending placement",
            ));
        }
        if ack.accepted {
            pending.acknowledgements.insert(ack.replica());
        } else {
            pending.acknowledgements.remove(&ack.replica());
        }

        self.fabric_queue_policy_sync_status(&ack.queue)
    }

    fn fabric_queue_bootstrap_replication_policy(
        &mut self,
        queue: &str,
        partition: u16,
        replication_factor: usize,
    ) -> io::Result<FabricQueuePlacement> {
        validate_queue_name(queue)?;
        if partition != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical Fabric queue partitions are not implemented yet; use partition 0",
            ));
        }
        if replication_factor == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric queue replication_factor must be greater than zero",
            ));
        }

        let payload = queue_stream_name(queue);
        let mutations = queue_mutation_stream_name(queue);
        ensure_stream(self, &payload)?;
        ensure_stream(self, &mutations)?;

        let payload_policy = self.fabric_stream_replication_policy(&payload)?;
        let mutation_policy = self.fabric_stream_replication_policy(&mutations)?;

        let policy = match (&payload_policy, &mutation_policy) {
            (Some(left), Some(right)) => {
                if left != right {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Fabric queue {queue:?} has divergent payload and mutation replication policies"
                        ),
                    ));
                }
                validate_requested_policy(left, partition, replication_factor)?;
                left.clone()
            }
            (Some(installed), None) => {
                validate_requested_policy(installed, partition, replication_factor)?;
                require_empty_stream(self, &payload)?;
                require_unowned_empty_stream(self, &mutations)?;
                self.fabric_stream_establish_replication_policy(&mutations, installed.clone())?;
                installed.clone()
            }
            (None, Some(installed)) => {
                validate_requested_policy(installed, partition, replication_factor)?;
                require_empty_stream(self, &mutations)?;
                require_unowned_empty_stream(self, &payload)?;
                self.fabric_stream_establish_replication_policy(&payload, installed.clone())?;
                installed.clone()
            }
            (None, None) => {
                require_unowned_empty_stream(self, &payload)?;
                require_unowned_empty_stream(self, &mutations)?;

                let owner_key = queue_placement_key(queue);
                let placement =
                    self.fabric_stream_placement(&owner_key, partition, replication_factor)?;
                let policy = FabricStreamReplicationPolicy {
                    partition,
                    epoch: FABRIC_STREAM_INITIAL_EPOCH,
                    leader: placement.leader.0,
                    membership_fingerprint: placement.membership_fingerprint,
                    replication_factor,
                    replicas: placement.replicas.iter().map(|node| node.0).collect(),
                };

                self.fabric_stream_establish_replication_policy(&payload, policy.clone())?;
                self.fabric_stream_establish_replication_policy(&mutations, policy.clone())?;
                policy
            }
        };

        Ok(FabricQueuePlacement::from_policy(queue, &policy))
    }

    fn fabric_queue_install_exact_policy(
        &mut self,
        placement: &FabricQueuePlacement,
    ) -> io::Result<()> {
        let payload = queue_stream_name(&placement.queue);
        let mutations = queue_mutation_stream_name(&placement.queue);
        ensure_stream(self, &payload)?;
        ensure_stream(self, &mutations)?;
        let policy = placement.to_policy();

        for stream in [&payload, &mutations] {
            match self.fabric_stream_replication_policy(stream)? {
                Some(existing) if existing == policy => {}
                Some(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Fabric queue stream {stream:?} has a conflicting replication policy"
                        ),
                    ));
                }
                None => {
                    require_empty_stream(self, stream)?;
                    self.fabric_stream_establish_replication_policy(stream, policy.clone())?;
                }
            }
        }
        Ok(())
    }
}

fn queue_placement_key(queue: &str) -> String {
    format!("{QUEUE_PLACEMENT_PREFIX}{queue}")
}

fn ensure_stream(runtime: &mut Runtime, stream: &str) -> io::Result<()> {
    match runtime.fabric_stream_create(stream, FabricStreamConfig::default()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn require_unowned_empty_stream(runtime: &mut Runtime, stream: &str) -> io::Result<()> {
    if runtime.fabric_stream_replication_policy(stream)?.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("Fabric stream {stream:?} already has a replication policy"),
        ));
    }
    require_empty_stream(runtime, stream)
}

fn require_empty_stream(runtime: &mut Runtime, stream: &str) -> io::Result<()> {
    let info = runtime.fabric_stream_info(stream)?;
    if info.last_sequence.is_some() || info.committed_sequence > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Fabric stream {stream:?} has durable history during queue policy bootstrap; explicit queue migration/recovery is required"
            ),
        ));
    }
    Ok(())
}

fn validate_requested_policy(
    policy: &FabricStreamReplicationPolicy,
    partition: u16,
    replication_factor: usize,
) -> io::Result<()> {
    if policy.partition != partition || policy.replication_factor != replication_factor {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "installed Fabric queue policy uses partition {} and replication factor {}, requested partition {partition} and replication factor {replication_factor}",
                policy.partition, policy.replication_factor
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_policy() -> FabricStreamReplicationPolicy {
        FabricStreamReplicationPolicy {
            partition: 0,
            epoch: 7,
            leader: 11,
            membership_fingerprint: 99,
            replication_factor: 3,
            replicas: vec![11, 22, 33],
        }
    }

    #[test]
    fn queue_placement_preserves_one_policy_for_all_internal_streams() {
        let placement = FabricQueuePlacement::from_policy("orders", &sample_policy());
        assert_eq!(placement.queue, "orders");
        assert_eq!(placement.partition, 0);
        assert_eq!(placement.epoch, 7);
        assert_eq!(placement.leader, NodeId(11));
        assert_eq!(placement.replicas, vec![NodeId(11), NodeId(22), NodeId(33)]);
        assert_eq!(placement.replication_factor(), 3);
        assert_eq!(placement.membership_fingerprint, 99);
    }

    #[test]
    fn queue_policy_wire_round_trip_preserves_shared_placement() {
        let placement = FabricQueuePlacement::from_policy("orders", &sample_policy());
        let install = FabricQueuePolicyInstall::from_placement(&placement);
        let bytes = install.to_wire_bytes().unwrap();
        let decoded = FabricQueuePolicyInstall::from_wire_bytes(&bytes).unwrap();
        assert_eq!(decoded.placement().unwrap(), placement);
    }

    #[test]
    fn queue_policy_rejects_duplicate_replicas() {
        let mut install = FabricQueuePolicyInstall::from_placement(
            &FabricQueuePlacement::from_policy("orders", &sample_policy()),
        );
        install.replicas = vec![11, 22, 22];
        assert_eq!(
            install.placement().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn queue_policy_ack_round_trip_preserves_fencing_fields() {
        let placement = FabricQueuePlacement::from_policy("orders", &sample_policy());
        let install = FabricQueuePolicyInstall::from_placement(&placement);
        let ack = FabricQueuePolicyAck::accepted(&install, NodeId(22));
        let bytes = ack.to_wire_bytes().unwrap();
        let decoded = FabricQueuePolicyAck::from_wire_bytes(&bytes).unwrap();
        assert_eq!(decoded, ack);
        assert_eq!(decoded.leader(), NodeId(11));
        assert_eq!(decoded.replica(), NodeId(22));
    }

    #[test]
    fn queue_placement_key_is_independent_of_internal_stream_names() {
        assert_eq!(queue_placement_key("orders"), "__queue_owner.orders");
        assert_ne!(queue_placement_key("orders"), queue_stream_name("orders"));
        assert_ne!(
            queue_placement_key("orders"),
            queue_mutation_stream_name("orders")
        );
    }

    #[test]
    fn local_queue_api_fails_closed_once_replication_policy_exists() {
        let root = std::env::temp_dir().join(format!(
            "nulang-fabric-queue-replicated-guard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut runtime = Runtime::new();
        runtime.fabric_stream_open(&root).unwrap();

        let payload = queue_stream_name("orders");
        runtime
            .fabric_stream_create(&payload, FabricStreamConfig::default())
            .unwrap();
        runtime
            .fabric_stream_establish_replication_policy(&payload, sample_policy())
            .unwrap();

        let error = runtime
            .fabric_queue_create("orders", crate::runtime::FabricQueueConfig::default())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replicated_queue_creation_is_policy_gated_retry_safe_and_quorum_committed() {
        use crate::runtime::cluster_dst::DeterministicCluster;
        use crate::runtime::FabricQueueJobStatus;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let addrs = [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39101),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39102),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39103),
        ];
        let mut cluster = DeterministicCluster::new(&addrs, 0x5155455545);
        cluster.run_rounds(30);
        assert!(cluster.active_views_converged());

        let base = std::env::temp_dir().join(format!(
            "nulang-fabric-queue-rf3-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let roots: Vec<_> = (0..3)
            .map(|index| base.join(format!("node-{index}")))
            .collect();
        for (index, root) in roots.iter().enumerate() {
            cluster.node_mut(index).fabric_stream_open(root).unwrap();
        }

        let placement = cluster
            .node_mut(0)
            .fabric_stream_placement(&queue_placement_key("orders"), 0, 3)
            .unwrap();
        let leader_index = (0..3)
            .find(|&index| cluster.id(index) == placement.leader)
            .unwrap();
        let config = FabricQueueConfig {
            visibility_timeout_ms: 15_000,
            max_attempts: 5,
            dead_letter_queue: None,
        };

        // First call persists/broadcasts policy only. No queue mutation may
        // appear before every replica has acknowledged the shared policy.
        let first = cluster
            .node_mut(leader_index)
            .fabric_queue_create_replicated("orders", config.clone(), 0, 3)
            .unwrap();
        assert!(!first.policy.ready);
        assert_eq!(first.mutation_sequence, None);

        cluster.run_rounds(8);
        let policy = cluster
            .node_mut(leader_index)
            .fabric_queue_policy_sync_status("orders")
            .unwrap();
        assert!(policy.ready);
        assert_eq!(policy.acknowledgements, 3);

        // QueueCreated is now sequence 1. Calling create again before quorum
        // completes must redispatch that same sequence, never append sequence 2.
        let pending = cluster
            .node_mut(leader_index)
            .fabric_queue_create_replicated("orders", config.clone(), 0, 3)
            .unwrap();
        assert_eq!(pending.mutation_sequence, Some(1));
        let retry = cluster
            .node_mut(leader_index)
            .fabric_queue_create_replicated("orders", config.clone(), 0, 3)
            .unwrap();
        assert_eq!(retry.mutation_sequence, Some(1));
        assert_eq!(
            cluster
                .node_mut(leader_index)
                .fabric_stream_info(&queue_mutation_stream_name("orders"))
                .unwrap()
                .last_sequence,
            Some(1)
        );

        cluster.run_rounds(12);
        let created = cluster
            .node_mut(leader_index)
            .fabric_queue_create_replicated("orders", config.clone(), 0, 3)
            .unwrap();
        assert!(created.created);
        assert!(created.replication.unwrap().committed);
        let queue_epoch = cluster
            .node_mut(leader_index)
            .fabric_queue_replication_placement("orders")
            .unwrap()
            .expect("queue policy must be installed")
            .epoch;

        let add_options = FabricQueueAddOptions {
            job_id: Some("job-1".to_string()),
            priority: 7,
            delay_ms: 0,
            max_attempts: None,
        };
        let pending_job = cluster
            .node_mut(leader_index)
            .fabric_queue_add_replicated(
                "orders",
                "render",
                b"payload",
                add_options.clone(),
                0,
                3,
                100,
            )
            .unwrap();
        assert_eq!(pending_job.sequence, Some(1));
        assert!(!pending_job.enqueued);

        let retry_job = cluster
            .node_mut(leader_index)
            .fabric_queue_add_replicated(
                "orders",
                "render",
                b"payload",
                add_options.clone(),
                0,
                3,
                100,
            )
            .unwrap();
        assert_eq!(retry_job.sequence, Some(1));
        assert!(retry_job.deduplicated);
        assert_eq!(
            cluster
                .node_mut(leader_index)
                .fabric_stream_info(&queue_stream_name("orders"))
                .unwrap()
                .last_sequence,
            Some(1)
        );

        cluster.run_rounds(12);
        let committed_job = cluster
            .node_mut(leader_index)
            .fabric_queue_add_replicated("orders", "render", b"payload", add_options, 0, 3, 100)
            .unwrap();
        assert_eq!(committed_job.sequence, Some(1));
        assert!(committed_job.deduplicated);
        assert!(committed_job.enqueued);
        assert!(committed_job.replication.unwrap().committed);

        for index in 0..3 {
            assert_eq!(
                cluster
                    .node_mut(index)
                    .fabric_stream_committed_sequence(&queue_mutation_stream_name("orders"))
                    .unwrap(),
                1
            );
            let info = cluster
                .node_mut(index)
                .fabric_queue_info_replicated("orders")
                .unwrap();
            assert_eq!(
                cluster
                    .node_mut(index)
                    .fabric_stream_committed_sequence(&queue_stream_name("orders"))
                    .unwrap(),
                1
            );
            assert_eq!(info.total, 1);
            assert_eq!(info.waiting, 1);
        }

        // Worker delivery is withheld until the LeaseAcquired mutation is
        // quorum committed. Same-operation retries resume sequence 2; a
        // different acquire cannot race the in-flight metadata mutation.
        let pending_lease = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated_with_lease_duration(
                "orders",
                "worker-a",
                "acquire-1",
                5_000,
                0,
                3,
                200,
            )
            .unwrap();
        assert_eq!(pending_lease.mutation_sequence, Some(2));
        assert!(pending_lease.delivery.is_none());
        assert!(!pending_lease.replication.unwrap().committed);

        let retry_lease = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated_with_lease_duration(
                "orders",
                "worker-a",
                "acquire-1",
                5_000,
                0,
                3,
                200,
            )
            .unwrap();
        assert_eq!(retry_lease.mutation_sequence, Some(2));
        assert!(retry_lease.resumed);
        assert!(retry_lease.delivery.is_none());

        let changed_duration = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated_with_lease_duration(
                "orders",
                "worker-a",
                "acquire-1",
                6_000,
                0,
                3,
                200,
            )
            .unwrap_err();
        assert_eq!(changed_duration.kind(), io::ErrorKind::InvalidData);

        let competing = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated("orders", "worker-b", "acquire-2", 0, 3, 200)
            .unwrap_err();
        assert_eq!(competing.kind(), io::ErrorKind::WouldBlock);

        cluster.run_rounds(12);
        let committed_lease = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated_with_lease_duration(
                "orders",
                "worker-a",
                "acquire-1",
                5_000,
                0,
                3,
                200,
            )
            .unwrap();
        assert!(committed_lease.resumed);
        assert!(committed_lease.replication.unwrap().committed);
        let delivery = committed_lease
            .delivery
            .expect("lease must be visible after quorum");
        assert_eq!(delivery.sequence, 1);
        assert_eq!(delivery.queue_epoch, queue_epoch);
        assert_eq!(delivery.job_id, "job-1");
        assert_eq!(delivery.name, "render");
        assert_eq!(delivery.payload, b"payload");
        assert_eq!(delivery.deliveries, 1);
        assert_eq!(delivery.lease_token, 1);
        assert_eq!(delivery.lease_until_ms, 5_200);

        for index in 0..3 {
            assert_eq!(
                cluster
                    .node_mut(index)
                    .fabric_stream_committed_sequence(&queue_mutation_stream_name("orders"))
                    .unwrap(),
                2
            );
            let info = cluster
                .node_mut(index)
                .fabric_queue_info_replicated("orders")
                .unwrap();
            assert_eq!(info.total, 1);
            assert_eq!(info.waiting, 0);
            assert_eq!(info.active, 1);
        }

        let lease_epoch = delivery.queue_epoch;
        let lease_token = delivery.lease_token;

        // Renew is also quorum-gated. The new deadline is hidden until the
        // LeaseRenewed mutation commits.
        let pending_renew = cluster
            .node_mut(leader_index)
            .fabric_queue_renew_replicated(
                "orders",
                1,
                "worker-a",
                lease_epoch,
                lease_token,
                "renew-1",
                20_000,
                0,
                3,
                300,
            )
            .unwrap();
        assert_eq!(pending_renew.mutation_sequence, Some(3));
        assert!(pending_renew.lease_until_ms.is_none());

        cluster.run_rounds(12);
        let committed_renew = cluster
            .node_mut(leader_index)
            .fabric_queue_renew_replicated(
                "orders",
                1,
                "worker-a",
                lease_epoch,
                lease_token,
                "renew-1",
                20_000,
                0,
                3,
                300,
            )
            .unwrap();
        assert!(committed_renew.resumed);
        assert!(committed_renew.replication.unwrap().committed);
        assert_eq!(committed_renew.lease_until_ms, Some(20_300));

        // NACK returns the job to Waiting only after the mutation commits.
        let pending_nack = cluster
            .node_mut(leader_index)
            .fabric_queue_nack_replicated(
                "orders",
                1,
                "worker-a",
                lease_epoch,
                lease_token,
                "nack-1",
                100,
                Some("retry"),
                0,
                3,
                400,
            )
            .unwrap();
        assert_eq!(pending_nack.mutation_sequence, Some(4));
        assert!(pending_nack.result.is_none());

        cluster.run_rounds(12);
        let committed_nack = cluster
            .node_mut(leader_index)
            .fabric_queue_nack_replicated(
                "orders",
                1,
                "worker-a",
                lease_epoch,
                lease_token,
                "nack-1",
                100,
                Some("retry"),
                0,
                3,
                400,
            )
            .unwrap();
        assert!(committed_nack.resumed);
        let nack_result = committed_nack
            .result
            .expect("NACK must be visible after quorum");
        assert_eq!(nack_result.status, FabricQueueJobStatus::Waiting);
        assert_eq!(nack_result.deliveries, 1);
        assert_eq!(nack_result.available_at_ms, Some(500));

        // The next acquisition increments the fencing token.
        let second_pending = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated("orders", "worker-b", "acquire-2", 0, 3, 600)
            .unwrap();
        assert_eq!(second_pending.mutation_sequence, Some(5));
        assert!(second_pending.delivery.is_none());

        cluster.run_rounds(12);
        let second_committed = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated("orders", "worker-b", "acquire-2", 0, 3, 600)
            .unwrap();
        let second_delivery = second_committed
            .delivery
            .expect("second lease must be visible after quorum");
        assert_eq!(second_delivery.queue_epoch, lease_epoch);
        assert_eq!(second_delivery.lease_token, 2);
        assert_eq!(second_delivery.deliveries, 2);

        // The previous worker/token is now fenced out, as is a stale queue
        // epoch.
        let stale_token = cluster
            .node_mut(leader_index)
            .fabric_queue_ack_replicated(
                "orders",
                1,
                "worker-a",
                lease_epoch,
                1,
                "ack-stale-token",
                0,
                3,
                700,
            )
            .unwrap_err();
        assert_eq!(stale_token.kind(), io::ErrorKind::PermissionDenied);

        let stale_epoch = cluster
            .node_mut(leader_index)
            .fabric_queue_ack_replicated(
                "orders",
                1,
                "worker-b",
                lease_epoch.saturating_add(1),
                2,
                "ack-stale-epoch",
                0,
                3,
                700,
            )
            .unwrap_err();
        assert_eq!(stale_epoch.kind(), io::ErrorKind::PermissionDenied);

        // ACK is not considered complete until its metadata mutation commits.
        let pending_ack = cluster
            .node_mut(leader_index)
            .fabric_queue_ack_replicated(
                "orders",
                1,
                "worker-b",
                lease_epoch,
                2,
                "ack-2",
                0,
                3,
                700,
            )
            .unwrap();
        assert_eq!(pending_ack.mutation_sequence, Some(6));
        assert!(!pending_ack.completed);

        cluster.run_rounds(12);
        let committed_ack = cluster
            .node_mut(leader_index)
            .fabric_queue_ack_replicated(
                "orders",
                1,
                "worker-b",
                lease_epoch,
                2,
                "ack-2",
                0,
                3,
                700,
            )
            .unwrap();
        assert!(committed_ack.resumed);
        assert!(committed_ack.completed);
        assert!(committed_ack.replication.unwrap().committed);

        for index in 0..3 {
            assert_eq!(
                cluster
                    .node_mut(index)
                    .fabric_stream_committed_sequence(&queue_mutation_stream_name("orders"))
                    .unwrap(),
                6
            );
            let info = cluster
                .node_mut(index)
                .fabric_queue_info_replicated("orders")
                .unwrap();
            assert_eq!(info.total, 1);
            assert_eq!(info.waiting, 0);
            assert_eq!(info.active, 0);
            assert_eq!(info.completed, 1);
        }

        // A second job exercises replicated lease expiry. Expiry itself is a
        // quorum mutation: until sequence 8 commits, the job remains Active
        // and cannot be reassigned.
        let job2_options = FabricQueueAddOptions {
            job_id: Some("job-2".to_string()),
            priority: 1,
            delay_ms: 0,
            max_attempts: None,
        };
        let pending_job2 = cluster
            .node_mut(leader_index)
            .fabric_queue_add_replicated(
                "orders",
                "render",
                b"payload-2",
                job2_options.clone(),
                0,
                3,
                1_000,
            )
            .unwrap();
        assert_eq!(pending_job2.sequence, Some(2));
        assert!(!pending_job2.enqueued);
        cluster.run_rounds(12);
        let committed_job2 = cluster
            .node_mut(leader_index)
            .fabric_queue_add_replicated(
                "orders",
                "render",
                b"payload-2",
                job2_options,
                0,
                3,
                1_000,
            )
            .unwrap();
        assert!(committed_job2.enqueued);

        let pending_job2_lease = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated("orders", "worker-c", "acquire-job-2", 0, 3, 1_000)
            .unwrap();
        assert_eq!(pending_job2_lease.mutation_sequence, Some(7));
        assert!(pending_job2_lease.delivery.is_none());
        cluster.run_rounds(12);
        let committed_job2_lease = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated("orders", "worker-c", "acquire-job-2", 0, 3, 1_000)
            .unwrap();
        let job2_delivery = committed_job2_lease
            .delivery
            .expect("second job lease must commit");
        assert_eq!(job2_delivery.sequence, 2);
        assert_eq!(job2_delivery.lease_token, 1);
        assert_eq!(job2_delivery.lease_until_ms, 16_000);

        let early_reap = cluster
            .node_mut(leader_index)
            .fabric_queue_reap_expired_replicated("orders", 0, 3, 15_999)
            .unwrap();
        assert_eq!(early_reap.mutation_sequence, None);
        assert_eq!(early_reap.expired_sequence, None);

        let pending_expiry = cluster
            .node_mut(leader_index)
            .fabric_queue_reap_expired_replicated("orders", 0, 3, 16_000)
            .unwrap();
        assert_eq!(pending_expiry.mutation_sequence, Some(8));
        assert_eq!(pending_expiry.expired_sequence, Some(2));
        assert!(pending_expiry.result.is_none());
        assert!(!pending_expiry.replication.unwrap().committed);

        let retry_expiry = cluster
            .node_mut(leader_index)
            .fabric_queue_reap_expired_replicated("orders", 0, 3, 16_500)
            .unwrap();
        assert_eq!(retry_expiry.mutation_sequence, Some(8));
        assert_eq!(retry_expiry.expired_sequence, Some(2));
        assert!(retry_expiry.resumed);
        assert!(retry_expiry.result.is_none());

        let blocked_acquire = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated(
                "orders",
                "worker-d",
                "acquire-job-2-redelivery",
                0,
                3,
                16_500,
            )
            .unwrap_err();
        assert_eq!(blocked_acquire.kind(), io::ErrorKind::WouldBlock);

        for index in 0..3 {
            let info = cluster
                .node_mut(index)
                .fabric_queue_info_replicated("orders")
                .unwrap();
            assert_eq!(info.total, 2);
            assert_eq!(info.active, 1);
            assert_eq!(info.waiting, 0);
            assert_eq!(info.completed, 1);
        }

        cluster.run_rounds(12);
        for index in 0..3 {
            assert_eq!(
                cluster
                    .node_mut(index)
                    .fabric_stream_committed_sequence(&queue_mutation_stream_name("orders"))
                    .unwrap(),
                8
            );
            let info = cluster
                .node_mut(index)
                .fabric_queue_info_replicated("orders")
                .unwrap();
            assert_eq!(info.total, 2);
            assert_eq!(info.active, 0);
            assert_eq!(info.waiting, 1);
            assert_eq!(info.completed, 1);
        }

        let settled_reap = cluster
            .node_mut(leader_index)
            .fabric_queue_reap_expired_replicated("orders", 0, 3, 17_000)
            .unwrap();
        assert_eq!(settled_reap.mutation_sequence, None);
        assert_eq!(settled_reap.expired_sequence, None);

        let pending_redelivery = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated(
                "orders",
                "worker-d",
                "acquire-job-2-redelivery",
                0,
                3,
                17_000,
            )
            .unwrap();
        assert_eq!(pending_redelivery.mutation_sequence, Some(9));
        assert!(pending_redelivery.delivery.is_none());
        cluster.run_rounds(12);
        let committed_redelivery = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated(
                "orders",
                "worker-d",
                "acquire-job-2-redelivery",
                0,
                3,
                17_000,
            )
            .unwrap();
        let redelivery = committed_redelivery
            .delivery
            .expect("expired job must become redeliverable only after quorum");
        assert_eq!(redelivery.sequence, 2);
        assert_eq!(redelivery.deliveries, 2);
        assert_eq!(redelivery.lease_token, 2);

        let pending_job2_ack = cluster
            .node_mut(leader_index)
            .fabric_queue_ack_replicated(
                "orders",
                2,
                "worker-d",
                queue_epoch,
                2,
                "ack-job-2",
                0,
                3,
                17_100,
            )
            .unwrap();
        assert_eq!(pending_job2_ack.mutation_sequence, Some(10));
        assert!(!pending_job2_ack.completed);
        cluster.run_rounds(12);
        let committed_job2_ack = cluster
            .node_mut(leader_index)
            .fabric_queue_ack_replicated(
                "orders",
                2,
                "worker-d",
                queue_epoch,
                2,
                "ack-job-2",
                0,
                3,
                17_100,
            )
            .unwrap();
        assert!(committed_job2_ack.completed);

        // Simulate leader-local torn/uncommitted tails. The replicated read
        // path must stop at each durable commit boundary and never attempt to
        // decode these malformed records.
        assert_eq!(
            cluster
                .node_mut(leader_index)
                .fabric_stream_append(&queue_stream_name("orders"), b"not-a-queue-envelope")
                .unwrap(),
            3
        );
        assert_eq!(
            cluster
                .node_mut(leader_index)
                .fabric_stream_append(
                    &queue_mutation_stream_name("orders"),
                    b"not-a-queue-mutation"
                )
                .unwrap(),
            11
        );
        let committed_view = cluster
            .node_mut(leader_index)
            .fabric_queue_info_replicated("orders")
            .unwrap();
        assert_eq!(committed_view.total, 2);
        assert_eq!(committed_view.waiting, 0);
        assert_eq!(committed_view.active, 0);
        assert_eq!(committed_view.completed, 2);

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn replicated_consumer_group_enforces_committed_concurrency() {
        use crate::runtime::cluster_dst::DeterministicCluster;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let addrs = [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39201),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39202),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39203),
        ];
        let mut cluster = DeterministicCluster::new(&addrs, 0x47524f5550);
        cluster.run_rounds(30);
        assert!(cluster.active_views_converged());

        let base = std::env::temp_dir().join(format!(
            "nulang-fabric-queue-group-rf3-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for index in 0..3 {
            cluster
                .node_mut(index)
                .fabric_stream_open(base.join(format!("node-{index}")))
                .unwrap();
        }

        let placement = cluster
            .node_mut(0)
            .fabric_stream_placement(&queue_placement_key("grouped"), 0, 3)
            .unwrap();
        let leader_index = (0..3)
            .find(|&index| cluster.id(index) == placement.leader)
            .unwrap();
        let config = FabricQueueConfig {
            visibility_timeout_ms: 30_000,
            max_attempts: 3,
            dead_letter_queue: None,
        };

        let first = cluster
            .node_mut(leader_index)
            .fabric_queue_create_replicated("grouped", config.clone(), 0, 3)
            .unwrap();
        assert!(!first.policy.ready);
        cluster.run_rounds(8);

        let pending_create = cluster
            .node_mut(leader_index)
            .fabric_queue_create_replicated("grouped", config.clone(), 0, 3)
            .unwrap();
        assert_eq!(pending_create.mutation_sequence, Some(1));
        cluster.run_rounds(12);
        assert!(
            cluster
                .node_mut(leader_index)
                .fabric_queue_create_replicated("grouped", config, 0, 3)
                .unwrap()
                .created
        );

        let pending_group = cluster
            .node_mut(leader_index)
            .fabric_queue_configure_consumer_group_replicated("grouped", "renderers", 1, 0, 3)
            .unwrap();
        assert_eq!(pending_group.mutation_sequence, Some(2));
        assert!(!pending_group.configured);

        let retry_group = cluster
            .node_mut(leader_index)
            .fabric_queue_configure_consumer_group_replicated("grouped", "renderers", 1, 0, 3)
            .unwrap();
        assert_eq!(retry_group.mutation_sequence, Some(2));
        assert!(retry_group.resumed);
        assert!(!retry_group.configured);

        cluster.run_rounds(12);
        let committed_group = cluster
            .node_mut(leader_index)
            .fabric_queue_configure_consumer_group_replicated("grouped", "renderers", 1, 0, 3)
            .unwrap();
        assert!(committed_group.configured);
        assert!(committed_group.resumed);

        for (job_id, payload) in [
            ("g-job-1", b"one".as_slice()),
            ("g-job-2", b"two".as_slice()),
        ] {
            let options = FabricQueueAddOptions {
                job_id: Some(job_id.to_string()),
                priority: 0,
                delay_ms: 0,
                max_attempts: None,
            };
            let pending = cluster
                .node_mut(leader_index)
                .fabric_queue_add_replicated(
                    "grouped",
                    "render",
                    payload,
                    options.clone(),
                    0,
                    3,
                    100,
                )
                .unwrap();
            assert!(!pending.enqueued);
            cluster.run_rounds(12);
            let committed = cluster
                .node_mut(leader_index)
                .fabric_queue_add_replicated("grouped", "render", payload, options, 0, 3, 100)
                .unwrap();
            assert!(committed.enqueued);
        }

        let first_pending = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_consumer_group_replicated(
                "grouped",
                "renderers",
                "worker-a",
                "group-acquire-1",
                0,
                3,
                200,
            )
            .unwrap();
        assert_eq!(first_pending.mutation_sequence, Some(3));
        assert!(first_pending.delivery.is_none());

        cluster.run_rounds(12);
        let first_committed = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_consumer_group_replicated(
                "grouped",
                "renderers",
                "worker-a",
                "group-acquire-1",
                0,
                3,
                200,
            )
            .unwrap();
        let first_delivery = first_committed.delivery.expect("group lease must commit");
        assert_eq!(first_delivery.sequence, 1);
        assert_eq!(first_delivery.consumer_group.as_deref(), Some("renderers"));

        for index in 0..3 {
            let group = cluster
                .node_mut(index)
                .fabric_queue_consumer_group_info_replicated("grouped", "renderers")
                .unwrap();
            assert_eq!(group.max_concurrency, 1);
            assert_eq!(group.active, 1);
            assert_eq!(group.available(), 0);
            assert!(group.saturated());
        }

        let saturated = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_consumer_group_replicated(
                "grouped",
                "renderers",
                "worker-b",
                "group-acquire-2",
                0,
                3,
                300,
            )
            .unwrap();
        assert_eq!(saturated.mutation_sequence, None);
        assert!(saturated.delivery.is_none());
        assert_eq!(
            cluster
                .node_mut(leader_index)
                .fabric_stream_info(&queue_mutation_stream_name("grouped"))
                .unwrap()
                .last_sequence,
            Some(3)
        );

        let queue_epoch = first_delivery.queue_epoch;
        let pending_ack = cluster
            .node_mut(leader_index)
            .fabric_queue_ack_replicated(
                "grouped",
                1,
                "worker-a",
                queue_epoch,
                first_delivery.lease_token,
                "group-ack-1",
                0,
                3,
                400,
            )
            .unwrap();
        assert_eq!(pending_ack.mutation_sequence, Some(4));
        assert!(!pending_ack.completed);
        cluster.run_rounds(12);
        assert!(
            cluster
                .node_mut(leader_index)
                .fabric_queue_ack_replicated(
                    "grouped",
                    1,
                    "worker-a",
                    queue_epoch,
                    first_delivery.lease_token,
                    "group-ack-1",
                    0,
                    3,
                    400,
                )
                .unwrap()
                .completed
        );

        let available = cluster
            .node_mut(leader_index)
            .fabric_queue_consumer_group_info_replicated("grouped", "renderers")
            .unwrap();
        assert_eq!(available.active, 0);
        assert_eq!(available.available(), 1);
        assert!(!available.saturated());

        let second_pending = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_consumer_group_replicated(
                "grouped",
                "renderers",
                "worker-b",
                "group-acquire-2",
                0,
                3,
                500,
            )
            .unwrap();
        assert_eq!(second_pending.mutation_sequence, Some(5));
        cluster.run_rounds(12);
        let second = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_consumer_group_replicated(
                "grouped",
                "renderers",
                "worker-b",
                "group-acquire-2",
                0,
                3,
                500,
            )
            .unwrap()
            .delivery
            .expect("second group lease must commit");
        assert_eq!(second.sequence, 2);
        assert_eq!(second.consumer_group.as_deref(), Some("renderers"));

        let pending_ack2 = cluster
            .node_mut(leader_index)
            .fabric_queue_ack_replicated(
                "grouped",
                2,
                "worker-b",
                second.queue_epoch,
                second.lease_token,
                "group-ack-2",
                0,
                3,
                600,
            )
            .unwrap();
        assert_eq!(pending_ack2.mutation_sequence, Some(6));
        cluster.run_rounds(12);
        assert!(
            cluster
                .node_mut(leader_index)
                .fabric_queue_ack_replicated(
                    "grouped",
                    2,
                    "worker-b",
                    second.queue_epoch,
                    second.lease_token,
                    "group-ack-2",
                    0,
                    3,
                    600,
                )
                .unwrap()
                .completed
        );

        for index in 0..3 {
            let group = cluster
                .node_mut(index)
                .fabric_queue_consumer_group_info_replicated("grouped", "renderers")
                .unwrap();
            assert_eq!(group.active, 0);
            assert_eq!(group.available(), 1);
            let info = cluster
                .node_mut(index)
                .fabric_queue_info_replicated("grouped")
                .unwrap();
            assert_eq!(info.total, 2);
            assert_eq!(info.completed, 2);
            assert_eq!(info.active, 0);
        }

        let conflict = cluster
            .node_mut(leader_index)
            .fabric_queue_configure_consumer_group_replicated("grouped", "renderers", 2, 0, 3)
            .unwrap_err();
        assert_eq!(conflict.kind(), io::ErrorKind::AlreadyExists);

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn replicated_dlq_handoff_is_target_first_retry_safe_and_lossless() {
        use crate::runtime::cluster_dst::DeterministicCluster;
        use crate::runtime::FabricQueueJobStatus;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};

        let addrs = [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39301),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39302),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 39303),
        ];
        let mut cluster = DeterministicCluster::new(&addrs, 0x444c5152);
        cluster.run_rounds(30);
        assert!(cluster.active_views_converged());

        let base = std::env::temp_dir().join(format!(
            "nulang-fabric-queue-dlq-rf3-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for index in 0..3 {
            cluster
                .node_mut(index)
                .fabric_stream_open(base.join(format!("node-{index}")))
                .unwrap();
        }

        let source = "dlq-source";
        let target = "dlq-target";
        let placement = cluster
            .node_mut(0)
            .fabric_stream_placement(&queue_placement_key(source), 0, 3)
            .unwrap();
        let leader_index = (0..3)
            .find(|&index| cluster.id(index) == placement.leader)
            .unwrap();

        let source_config = FabricQueueConfig {
            visibility_timeout_ms: 100,
            max_attempts: 1,
            dead_letter_queue: Some(target.to_string()),
        };
        let first_create = cluster
            .node_mut(leader_index)
            .fabric_queue_create_replicated(source, source_config.clone(), 0, 3)
            .unwrap();
        assert!(!first_create.policy.ready);
        cluster.run_rounds(8);
        let pending_create = cluster
            .node_mut(leader_index)
            .fabric_queue_create_replicated(source, source_config.clone(), 0, 3)
            .unwrap();
        assert_eq!(pending_create.mutation_sequence, Some(1));
        cluster.run_rounds(12);
        assert!(
            cluster
                .node_mut(leader_index)
                .fabric_queue_create_replicated(source, source_config, 0, 3)
                .unwrap()
                .created
        );

        // The DLQ target is explicitly co-owned with the source. First retry
        // synchronizes that exact policy; the next appends QueueCreated.
        let target_config = FabricQueueConfig {
            visibility_timeout_ms: 30_000,
            max_attempts: 3,
            dead_letter_queue: None,
        };
        let target_sync = cluster
            .node_mut(leader_index)
            .fabric_queue_prepare_dead_letter_target_replicated(
                source,
                target,
                target_config.clone(),
                0,
                3,
            )
            .unwrap();
        assert!(!target_sync.created);
        cluster.run_rounds(8);
        let target_pending = cluster
            .node_mut(leader_index)
            .fabric_queue_prepare_dead_letter_target_replicated(
                source,
                target,
                target_config.clone(),
                0,
                3,
            )
            .unwrap();
        assert_eq!(target_pending.mutation_sequence, Some(1));
        cluster.run_rounds(12);
        assert!(
            cluster
                .node_mut(leader_index)
                .fabric_queue_prepare_dead_letter_target_replicated(
                    source,
                    target,
                    target_config,
                    0,
                    3,
                )
                .unwrap()
                .created
        );

        let target_placement = cluster
            .node_mut(leader_index)
            .fabric_queue_replication_placement(target)
            .unwrap()
            .expect("target placement must exist");
        let source_placement = cluster
            .node_mut(leader_index)
            .fabric_queue_replication_placement(source)
            .unwrap()
            .expect("source placement must exist");
        assert_eq!(target_placement.epoch, source_placement.epoch);
        assert_eq!(target_placement.leader, source_placement.leader);
        assert_eq!(target_placement.replicas, source_placement.replicas);
        assert_eq!(
            target_placement.membership_fingerprint,
            source_placement.membership_fingerprint
        );

        let add = FabricQueueAddOptions {
            job_id: Some("poison-1".to_string()),
            priority: 7,
            delay_ms: 0,
            max_attempts: None,
        };
        let pending_add = cluster
            .node_mut(leader_index)
            .fabric_queue_add_replicated(source, "render", b"poison", add.clone(), 0, 3, 100)
            .unwrap();
        assert!(!pending_add.enqueued);
        cluster.run_rounds(12);
        assert!(
            cluster
                .node_mut(leader_index)
                .fabric_queue_add_replicated(source, "render", b"poison", add, 0, 3, 100)
                .unwrap()
                .enqueued
        );

        let pending_lease = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated(source, "worker-a", "dlq-acquire-1", 0, 3, 200)
            .unwrap();
        assert_eq!(pending_lease.mutation_sequence, Some(2));
        cluster.run_rounds(12);
        let delivery = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated(source, "worker-a", "dlq-acquire-1", 0, 3, 200)
            .unwrap()
            .delivery
            .expect("source delivery must commit");
        assert_eq!(delivery.lease_token, 1);

        // Terminal NACK first replicates the target payload. Source metadata is
        // untouched until that payload has reached quorum.
        let first_dlq = cluster
            .node_mut(leader_index)
            .fabric_queue_nack_replicated(
                source,
                1,
                "worker-a",
                delivery.queue_epoch,
                delivery.lease_token,
                "dlq-nack-1",
                0,
                Some("poison"),
                0,
                3,
                250,
            )
            .unwrap();
        assert_eq!(first_dlq.mutation_sequence, None);
        assert!(first_dlq.result.is_none());

        let source_before_target_commit = cluster
            .node_mut(leader_index)
            .fabric_queue_info_replicated(source)
            .unwrap();
        assert_eq!(source_before_target_commit.active, 1);
        assert_eq!(source_before_target_commit.dead_lettered, 0);

        cluster.run_rounds(12);

        // This is the crash-safe handoff window: target is durable, source is
        // still Active. Restart/retry can only move forward from here.
        let source_after_target_commit = cluster
            .node_mut(leader_index)
            .fabric_queue_info_replicated(source)
            .unwrap();
        let target_after_target_commit = cluster
            .node_mut(leader_index)
            .fabric_queue_info_replicated(target)
            .unwrap();
        assert_eq!(source_after_target_commit.active, 1);
        assert_eq!(source_after_target_commit.dead_lettered, 0);
        assert_eq!(target_after_target_commit.total, 1);
        assert_eq!(target_after_target_commit.waiting, 1);

        // Deterministic DLQ ids are content-fenced: a conflicting payload
        // cannot hijack the retry identity.
        let dlq_collision = cluster
            .node_mut(leader_index)
            .fabric_queue_add_replicated(
                target,
                "render",
                b"different",
                FabricQueueAddOptions {
                    job_id: Some("__dlq:dlq-source:1".to_string()),
                    priority: 7,
                    delay_ms: 0,
                    max_attempts: None,
                },
                0,
                3,
                260,
            )
            .unwrap_err();
        assert_eq!(dlq_collision.kind(), io::ErrorKind::InvalidData);

        let source_terminal_pending = cluster
            .node_mut(leader_index)
            .fabric_queue_nack_replicated(
                source,
                1,
                "worker-a",
                delivery.queue_epoch,
                delivery.lease_token,
                "dlq-nack-1",
                0,
                Some("poison"),
                0,
                3,
                250,
            )
            .unwrap();
        assert_eq!(source_terminal_pending.mutation_sequence, Some(3));
        assert!(source_terminal_pending.result.is_none());

        let retry_source_terminal = cluster
            .node_mut(leader_index)
            .fabric_queue_nack_replicated(
                source,
                1,
                "worker-a",
                delivery.queue_epoch,
                delivery.lease_token,
                "dlq-nack-1",
                0,
                Some("poison"),
                0,
                3,
                250,
            )
            .unwrap();
        assert_eq!(retry_source_terminal.mutation_sequence, Some(3));
        assert!(retry_source_terminal.resumed);
        assert!(retry_source_terminal.result.is_none());

        cluster.run_rounds(12);
        let source_terminal = cluster
            .node_mut(leader_index)
            .fabric_queue_nack_replicated(
                source,
                1,
                "worker-a",
                delivery.queue_epoch,
                delivery.lease_token,
                "dlq-nack-1",
                0,
                Some("poison"),
                0,
                3,
                250,
            )
            .unwrap();
        assert_eq!(
            source_terminal
                .result
                .expect("source terminalization must commit")
                .status,
            FabricQueueJobStatus::DeadLettered
        );

        // Add a second poison job and let its lease expire. The reaper must use
        // the same target-first handoff rather than terminalizing locally.
        let add2 = FabricQueueAddOptions {
            job_id: Some("poison-2".to_string()),
            priority: 3,
            delay_ms: 0,
            max_attempts: None,
        };
        let pending_add2 = cluster
            .node_mut(leader_index)
            .fabric_queue_add_replicated(source, "render", b"poison-2", add2.clone(), 0, 3, 1_000)
            .unwrap();
        assert!(!pending_add2.enqueued);
        cluster.run_rounds(12);
        assert!(
            cluster
                .node_mut(leader_index)
                .fabric_queue_add_replicated(source, "render", b"poison-2", add2, 0, 3, 1_000,)
                .unwrap()
                .enqueued
        );

        let pending_lease2 = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated(source, "worker-b", "dlq-acquire-2", 0, 3, 1_000)
            .unwrap();
        assert_eq!(pending_lease2.mutation_sequence, Some(4));
        cluster.run_rounds(12);
        let delivery2 = cluster
            .node_mut(leader_index)
            .fabric_queue_acquire_replicated(source, "worker-b", "dlq-acquire-2", 0, 3, 1_000)
            .unwrap()
            .delivery
            .expect("second source delivery must commit");
        assert_eq!(delivery2.lease_until_ms, 1_100);

        let expiry_target_pending = cluster
            .node_mut(leader_index)
            .fabric_queue_reap_expired_replicated(source, 0, 3, 1_100)
            .unwrap();
        assert_eq!(expiry_target_pending.mutation_sequence, None);
        assert_eq!(expiry_target_pending.expired_sequence, Some(2));
        assert!(expiry_target_pending.result.is_none());

        cluster.run_rounds(12);
        let source_during_expiry_handoff = cluster
            .node_mut(leader_index)
            .fabric_queue_info_replicated(source)
            .unwrap();
        let target_two = cluster
            .node_mut(leader_index)
            .fabric_queue_info_replicated(target)
            .unwrap();
        assert_eq!(source_during_expiry_handoff.active, 1);
        assert_eq!(source_during_expiry_handoff.dead_lettered, 1);
        assert_eq!(target_two.total, 2);

        let expiry_source_pending = cluster
            .node_mut(leader_index)
            .fabric_queue_reap_expired_replicated(source, 0, 3, 1_100)
            .unwrap();
        assert_eq!(expiry_source_pending.mutation_sequence, Some(5));
        assert!(expiry_source_pending.result.is_none());
        cluster.run_rounds(12);
        let settled_reap = cluster
            .node_mut(leader_index)
            .fabric_queue_reap_expired_replicated(source, 0, 3, 1_100)
            .unwrap();
        assert_eq!(settled_reap.mutation_sequence, None);
        assert_eq!(settled_reap.expired_sequence, None);

        for index in 0..3 {
            let source_info = cluster
                .node_mut(index)
                .fabric_queue_info_replicated(source)
                .unwrap();
            let target_info = cluster
                .node_mut(index)
                .fabric_queue_info_replicated(target)
                .unwrap();
            assert_eq!(source_info.total, 2);
            assert_eq!(source_info.active, 0);
            assert_eq!(source_info.dead_lettered, 2);
            assert_eq!(target_info.total, 2);
            assert_eq!(target_info.waiting, 2);

            let target_records = cluster
                .node_mut(index)
                .fabric_stream_read_committed(&queue_stream_name(target), 1, 2)
                .unwrap();
            assert_eq!(target_records.len(), 2);
            let first = decode_queue_envelope_bytes(&target_records[0].payload).unwrap();
            let second = decode_queue_envelope_bytes(&target_records[1].payload).unwrap();
            assert_eq!(first.job_id.as_deref(), Some("__dlq:dlq-source:1"));
            assert_eq!(first.name, "render");
            assert_eq!(first.payload, b"poison");
            assert_eq!(first.priority, 7);
            assert_eq!(second.job_id.as_deref(), Some("__dlq:dlq-source:2"));
            assert_eq!(second.payload, b"poison-2");
        }

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn installed_policy_request_must_match_partition_and_replication_factor() {
        let policy = sample_policy();
        assert!(validate_requested_policy(&policy, 0, 3).is_ok());
        assert_eq!(
            validate_requested_policy(&policy, 0, 2).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            validate_requested_policy(&policy, 1, 3).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
