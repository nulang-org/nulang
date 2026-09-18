//! Deterministic ownership and replica placement for durable Fabric streams.
//!
//! The current durable store is one physical partition per stream (partition
//! zero). Placement is generic over a partition id so the ownership contract
//! does not change when physical multi-partition logs land later.
//!
//! Important safety property: placement uses the stable *known* membership set,
//! including Suspicious/Failed nodes, and changes only after a node is confirmed
//! removed or gracefully leaving. A transient partition therefore does not
//! elect a second writer. If the designated leader is unavailable, writes fail
//! closed until an explicit failover/epoch mechanism is introduced.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::time::{Duration, Instant};

use crate::runtime::fabric_stream::FabricStreamPendingIntent;
use crate::runtime::{
    ClusterState, FabricStreamConfig, MessagePriority, NodeId, NodeStatus, Packet, Runtime,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricStreamPlacement {
    pub stream: String,
    pub partition: u16,
    pub leader: NodeId,
    pub replicas: Vec<NodeId>,
    /// Fingerprint of the stable membership set used for placement.
    ///
    /// This is not a consensus term or monotonically increasing epoch. It is a
    /// deterministic stale-plan detector until explicit stream epochs land.
    pub membership_fingerprint: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricStreamReplicaAppend {
    pub stream: String,
    pub partition: u16,
    pub leader: NodeId,
    pub membership_fingerprint: u64,
    pub replication_factor: usize,
    pub stream_config: FabricStreamConfig,
    pub sequence: u64,
    pub payload: Vec<u8>,
}

pub(crate) const FABRIC_STREAM_REPLICA_BEHAVIOR: &str = "__nulang_fabric_stream_replica_v1";
const MAX_REPLICA_ENVELOPE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const FABRIC_STREAM_REPLICA_ACK_BEHAVIOR: &str = "__nulang_fabric_stream_replica_ack_v1";
pub(crate) const FABRIC_STREAM_COMMIT_BEHAVIOR: &str = "__nulang_fabric_stream_commit_v1";

const FABRIC_STREAM_RETRY_INITIAL: Duration = Duration::from_millis(500);
const FABRIC_STREAM_RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FabricStreamReplicaDispatchReport {
    pub intended_remote: usize,
    pub dispatched: usize,
    pub unavailable: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FabricStreamReplicationStatus {
    pub sequence: u64,
    pub quorum: usize,
    pub acknowledgements: usize,
    pub rejections: usize,
    pub committed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FabricStreamReplicatedAppendResult {
    pub sequence: u64,
    pub status: FabricStreamReplicationStatus,
    pub dispatch: FabricStreamReplicaDispatchReport,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FabricStreamRecoveryReport {
    pub recovered: usize,
    pub removed_committed: usize,
    pub removed_orphan_reservations: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FabricStreamRetryReport {
    pub pending_sequences: usize,
    pub intended_remote: usize,
    pub dispatched: usize,
    pub unavailable: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FabricStreamCatchUpReport {
    pub replicas_examined: usize,
    pub records_dispatched: usize,
    pub unavailable_replicas: usize,
    pub commit_updates_dispatched: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct FabricStreamReplicaAckOutcome {
    pub placement: FabricStreamPlacement,
    pub committed_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FabricStreamCommitUpdate {
    pub stream: String,
    pub partition: u16,
    pub leader: NodeId,
    pub membership_fingerprint: u64,
    pub replication_factor: usize,
    pub committed_sequence: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FabricStreamCommitUpdateWire {
    stream: String,
    partition: u16,
    leader: u64,
    membership_fingerprint: u64,
    replication_factor: usize,
    committed_sequence: u64,
}

impl FabricStreamCommitUpdate {
    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(&FabricStreamCommitUpdateWire {
            stream: self.stream.clone(),
            partition: self.partition,
            leader: self.leader.0,
            membership_fingerprint: self.membership_fingerprint,
            replication_factor: self.replication_factor,
            committed_sequence: self.committed_sequence,
        })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        let wire: FabricStreamCommitUpdateWire = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if wire.stream.is_empty() || wire.replication_factor == 0 || wire.committed_sequence == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Fabric stream commit update",
            ));
        }
        Ok(Self {
            stream: wire.stream,
            partition: wire.partition,
            leader: NodeId(wire.leader),
            membership_fingerprint: wire.membership_fingerprint,
            replication_factor: wire.replication_factor,
            committed_sequence: wire.committed_sequence,
        })
    }
}

#[derive(Debug, Clone)]
struct PendingReplicaCommit {
    leader: NodeId,
    membership_fingerprint: u64,
    replicas: HashSet<NodeId>,
    acknowledgements: HashSet<NodeId>,
    rejections: HashSet<NodeId>,
    quorum: usize,
}

#[derive(Debug, Clone)]
struct FabricStreamRetrySchedule {
    next_attempt: Instant,
    backoff: Duration,
}

#[derive(Debug, Default)]
pub(crate) struct FabricStreamReplicationState {
    pending: HashMap<(String, u16), BTreeMap<u64, PendingReplicaCommit>>,
    retry_schedules: HashMap<(String, u16), FabricStreamRetrySchedule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FabricStreamReplicaAck {
    pub stream: String,
    pub partition: u16,
    pub leader: NodeId,
    pub membership_fingerprint: u64,
    pub replication_factor: usize,
    pub sequence: u64,
    pub replica: NodeId,
    pub accepted: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FabricStreamReplicaAckWire {
    stream: String,
    partition: u16,
    leader: u64,
    membership_fingerprint: u64,
    replication_factor: usize,
    sequence: u64,
    replica: u64,
    accepted: bool,
}

impl FabricStreamReplicaAck {
    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(&FabricStreamReplicaAckWire {
            stream: self.stream.clone(),
            partition: self.partition,
            leader: self.leader.0,
            membership_fingerprint: self.membership_fingerprint,
            replication_factor: self.replication_factor,
            sequence: self.sequence,
            replica: self.replica.0,
            accepted: self.accepted,
        })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        let wire: FabricStreamReplicaAckWire = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if wire.stream.is_empty() || wire.replication_factor == 0 || wire.sequence == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Fabric stream replica ACK",
            ));
        }
        Ok(Self {
            stream: wire.stream,
            partition: wire.partition,
            leader: NodeId(wire.leader),
            membership_fingerprint: wire.membership_fingerprint,
            replication_factor: wire.replication_factor,
            sequence: wire.sequence,
            replica: NodeId(wire.replica),
            accepted: wire.accepted,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FabricStreamReplicaAppendWire {
    stream: String,
    partition: u16,
    leader: u64,
    membership_fingerprint: u64,
    replication_factor: usize,
    stream_config: FabricStreamConfig,
    sequence: u64,
    payload: Vec<u8>,
}

impl FabricStreamReplicaAppend {
    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        let wire = FabricStreamReplicaAppendWire {
            stream: self.stream.clone(),
            partition: self.partition,
            leader: self.leader.0,
            membership_fingerprint: self.membership_fingerprint,
            replication_factor: self.replication_factor,
            stream_config: self.stream_config,
            sequence: self.sequence,
            payload: self.payload.clone(),
        };
        let bytes = serde_json::to_vec(&wire)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if bytes.len() > MAX_REPLICA_ENVELOPE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Fabric stream replica envelope exceeds {} bytes",
                    MAX_REPLICA_ENVELOPE_BYTES
                ),
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_REPLICA_ENVELOPE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric stream replica envelope exceeds receiver limit",
            ));
        }
        let wire: FabricStreamReplicaAppendWire = serde_json::from_slice(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if wire.stream.is_empty() || wire.replication_factor == 0 || wire.sequence == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Fabric stream replica envelope",
            ));
        }
        Ok(Self {
            stream: wire.stream,
            partition: wire.partition,
            leader: NodeId(wire.leader),
            membership_fingerprint: wire.membership_fingerprint,
            replication_factor: wire.replication_factor,
            stream_config: wire.stream_config,
            sequence: wire.sequence,
            payload: wire.payload,
        })
    }
}

impl Runtime {
    /// Append locally as leader, create a pending quorum ticket, and dispatch
    /// the replica envelope to reachable followers.
    pub fn fabric_stream_replicated_append(
        &mut self,
        stream: &str,
        partition: u16,
        replication_factor: usize,
        payload: &[u8],
    ) -> io::Result<FabricStreamReplicatedAppendResult> {
        if partition != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical Fabric stream partition logs are not implemented yet; use partition 0",
            ));
        }

        let placement = self.fabric_stream_placement(stream, partition, replication_factor)?;
        let local = self
            .distributed
            .node_id
            .expect("placement validated node id");
        if placement.leader != local {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric stream partition leader is {:?}, local node is {:?}",
                    placement.leader, local
                ),
            ));
        }
        if let Some(cluster) = self.distributed.cluster.as_ref() {
            let local_status = cluster.get_node(local).map(|member| member.status);
            if !matches!(
                local_status,
                Some(NodeStatus::Healthy | NodeStatus::Joining)
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "local Fabric stream leader is not healthy",
                ));
            }
        }

        let stream_config = self.fabric_stream_config(stream)?;
        let sequence = self.fabric_stream_info(stream)?.next_sequence;
        let intent = FabricStreamPendingIntent {
            partition,
            leader: placement.leader.0,
            membership_fingerprint: placement.membership_fingerprint,
            replication_factor,
            replicas: placement.replicas.iter().map(|node| node.0).collect(),
            sequence,
        };

        // Persist intent before the record so a crash can never leave an
        // uncommitted durable tail without reconstructible replication state.
        self.fabric_stream_reserve_replication_intent(stream, intent)?;
        self.fabric_stream_append_reserved_replica(stream, sequence, payload)?;

        let append = FabricStreamReplicaAppend {
            stream: stream.to_string(),
            partition,
            leader: placement.leader,
            membership_fingerprint: placement.membership_fingerprint,
            replication_factor,
            stream_config,
            sequence,
            payload: payload.to_vec(),
        };

        let quorum = replication_factor / 2 + 1;
        let key = (stream.to_string(), partition);
        let mut acknowledgements = HashSet::new();
        acknowledgements.insert(local);
        self.distributed
            .fabric_stream_replication
            .pending
            .entry(key.clone())
            .or_default()
            .insert(
                sequence,
                PendingReplicaCommit {
                    leader: placement.leader,
                    membership_fingerprint: placement.membership_fingerprint,
                    replicas: placement.replicas.iter().copied().collect(),
                    acknowledgements,
                    rejections: HashSet::new(),
                    quorum,
                },
            );
        let retry_at = self.now() + FABRIC_STREAM_RETRY_INITIAL;
        self.distributed
            .fabric_stream_replication
            .retry_schedules
            .entry(key)
            .or_insert(FabricStreamRetrySchedule {
                next_attempt: retry_at,
                backoff: FABRIC_STREAM_RETRY_INITIAL,
            });

        // Replication factor one is committed by the leader's own fsync.
        let status = if quorum == 1 {
            self.fabric_stream_advance_commits(stream, partition)?;
            FabricStreamReplicationStatus {
                sequence,
                quorum: 1,
                acknowledgements: 1,
                rejections: 0,
                committed: true,
            }
        } else {
            self.fabric_stream_replication_status(stream, partition, sequence)?
        };

        let dispatch = self.fabric_stream_dispatch_replica_append(&placement, &append)?;
        Ok(FabricStreamReplicatedAppendResult {
            sequence,
            status,
            dispatch,
        })
    }

    /// Reconstruct in-memory quorum tickets from durable replication intent.
    ///
    /// ACK sets are intentionally rebuilt with only the leader's own fsync.
    /// Retrying followers is safe because replica application is idempotent.
    pub fn fabric_stream_recover_pending(
        &mut self,
        stream: &str,
    ) -> io::Result<FabricStreamRecoveryReport> {
        let cluster = self.distributed.cluster.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream recovery requires cluster membership",
            )
        })?;
        let result = self.fabric_stream_recover_pending_from_cluster(stream, &cluster);
        self.distributed.cluster = Some(cluster);
        result
    }

    pub(crate) fn fabric_stream_recover_pending_from_cluster(
        &mut self,
        stream: &str,
        cluster: &ClusterState,
    ) -> io::Result<FabricStreamRecoveryReport> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream recovery requires distribution",
            )
        })?;
        let committed = self.fabric_stream_committed_sequence(stream)?;
        let info = self.fabric_stream_info(stream)?;
        let intents = self.fabric_stream_pending_replication_intents(stream)?;
        let mut report = FabricStreamRecoveryReport::default();

        for intent in intents {
            if intent.sequence <= committed {
                self.fabric_stream_remove_replication_intent(stream, intent.sequence)?;
                report.removed_committed += 1;
                continue;
            }

            let record = self
                .fabric_stream_read(stream, intent.sequence, 1)?
                .into_iter()
                .next()
                .filter(|record| record.sequence == intent.sequence);
            if record.is_none() {
                if intent.sequence == info.next_sequence {
                    self.fabric_stream_remove_replication_intent(stream, intent.sequence)?;
                    report.removed_orphan_reservations += 1;
                    continue;
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Fabric replication intent for sequence {} has no matching durable record",
                        intent.sequence
                    ),
                ));
            }

            let placement = compute_stream_placement(
                local,
                Some(cluster),
                stream,
                intent.partition,
                intent.replication_factor,
            )?;
            let intent_replicas: Vec<NodeId> =
                intent.replicas.iter().copied().map(NodeId).collect();
            if placement.leader.0 != intent.leader
                || placement.membership_fingerprint != intent.membership_fingerprint
                || placement.replicas != intent_replicas
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Fabric replication intent for sequence {} no longer matches current placement",
                        intent.sequence
                    ),
                ));
            }
            if placement.leader != local {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "local node is no longer the persisted Fabric stream leader",
                ));
            }

            let key = (stream.to_string(), intent.partition);
            let entries = self
                .distributed
                .fabric_stream_replication
                .pending
                .entry(key)
                .or_default();
            if entries.contains_key(&intent.sequence) {
                continue;
            }

            let mut acknowledgements = HashSet::new();
            acknowledgements.insert(local);
            entries.insert(
                intent.sequence,
                PendingReplicaCommit {
                    leader: placement.leader,
                    membership_fingerprint: placement.membership_fingerprint,
                    replicas: placement.replicas.iter().copied().collect(),
                    acknowledgements,
                    rejections: HashSet::new(),
                    quorum: intent.replication_factor / 2 + 1,
                },
            );
            report.recovered += 1;
        }

        // Handles RF=1 recovery and any contiguous tickets that already meet
        // quorum after reconstruction.
        self.fabric_stream_advance_commits(stream, 0)?;
        if self
            .distributed
            .fabric_stream_replication
            .pending
            .contains_key(&(stream.to_string(), 0))
        {
            let retry_now = self.now();
            self.distributed
                .fabric_stream_replication
                .retry_schedules
                .entry((stream.to_string(), 0))
                .or_insert(FabricStreamRetrySchedule {
                    next_attempt: retry_now,
                    backoff: FABRIC_STREAM_RETRY_INITIAL,
                });
        }
        Ok(report)
    }

    /// Re-dispatch every pending exact sequence to the configured replica set.
    ///
    /// The retry is intentionally duplicate-tolerant. After restart the leader
    /// has forgotten follower ACK sets, so sending to all remote replicas is
    /// safer than guessing which followers already persisted a sequence.
    pub fn fabric_stream_retry_pending(
        &mut self,
        stream: &str,
        partition: u16,
    ) -> io::Result<FabricStreamRetryReport> {
        self.fabric_stream_recover_pending(stream)?;

        let key = (stream.to_string(), partition);
        let tickets: Vec<(u64, PendingReplicaCommit)> = self
            .distributed
            .fabric_stream_replication
            .pending
            .get(&key)
            .map(|entries| {
                entries
                    .iter()
                    .map(|(&sequence, ticket)| (sequence, ticket.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let mut report = FabricStreamRetryReport {
            pending_sequences: tickets.len(),
            ..FabricStreamRetryReport::default()
        };
        if tickets.is_empty() {
            return Ok(report);
        }

        let stream_config = self.fabric_stream_config(stream)?;
        for (sequence, ticket) in tickets {
            let record = self
                .fabric_stream_read(stream, sequence, 1)?
                .into_iter()
                .next()
                .filter(|record| record.sequence == sequence)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "pending Fabric replication sequence {sequence} is missing locally"
                        ),
                    )
                })?;

            let placement =
                self.fabric_stream_placement(stream, partition, ticket.replicas.len())?;
            if placement.leader != ticket.leader
                || placement.membership_fingerprint != ticket.membership_fingerprint
                || placement.replicas.iter().copied().collect::<HashSet<_>>() != ticket.replicas
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pending Fabric replication ticket no longer matches current placement",
                ));
            }

            let append = FabricStreamReplicaAppend {
                stream: stream.to_string(),
                partition,
                leader: ticket.leader,
                membership_fingerprint: ticket.membership_fingerprint,
                replication_factor: ticket.replicas.len(),
                stream_config,
                sequence,
                payload: record.payload,
            };
            let dispatch = self.fabric_stream_dispatch_replica_append(&placement, &append)?;
            report.intended_remote += dispatch.intended_remote;
            report.dispatched += dispatch.dispatched;
            report.unavailable += dispatch.unavailable;
        }

        Ok(report)
    }

    /// Run due pending-quorum retries using the runtime's logical clock.
    ///
    /// Retry state is intentionally in-memory; durable replication intent is
    /// the restart source of truth and recreates an immediate schedule.
    pub(crate) fn fabric_stream_tick_retries(&mut self) {
        let now = self.now();
        let pending_keys: Vec<(String, u16)> = self
            .distributed
            .fabric_stream_replication
            .pending
            .keys()
            .cloned()
            .collect();

        for key in &pending_keys {
            self.distributed
                .fabric_stream_replication
                .retry_schedules
                .entry(key.clone())
                .or_insert_with(|| FabricStreamRetrySchedule {
                    next_attempt: now,
                    backoff: FABRIC_STREAM_RETRY_INITIAL,
                });
        }

        let pending_key_set: HashSet<(String, u16)> = pending_keys.iter().cloned().collect();
        self.distributed
            .fabric_stream_replication
            .retry_schedules
            .retain(|key, _| pending_key_set.contains(key));

        let due: Vec<(String, u16)> = self
            .distributed
            .fabric_stream_replication
            .retry_schedules
            .iter()
            .filter(|(key, schedule)| {
                schedule.next_attempt <= now
                    && self
                        .distributed
                        .fabric_stream_replication
                        .pending
                        .contains_key(*key)
            })
            .map(|(key, _)| key.clone())
            .collect();

        for (stream, partition) in due {
            let result = self.fabric_stream_retry_pending(&stream, partition);
            let still_pending = self
                .distributed
                .fabric_stream_replication
                .pending
                .contains_key(&(stream.clone(), partition));
            if !still_pending {
                self.distributed
                    .fabric_stream_replication
                    .retry_schedules
                    .remove(&(stream, partition));
                continue;
            }

            let now = self.now();
            if let Some(schedule) = self
                .distributed
                .fabric_stream_replication
                .retry_schedules
                .get_mut(&(stream.clone(), partition))
            {
                schedule.backoff = schedule
                    .backoff
                    .saturating_mul(2)
                    .min(FABRIC_STREAM_RETRY_MAX);
                schedule.next_attempt = now + schedule.backoff;
            }
            if let Err(error) = result {
                tracing::warn!(
                    "nulang-fabric-stream: retry for {} partition {} failed: {}",
                    stream,
                    partition,
                    error
                );
            }
        }
    }

    /// Repair lagging replicas using only records already committed by quorum.
    ///
    /// Progress is leader-side durable ACK state. Dispatch does not advance
    /// progress; followers must durably apply and application-ACK each record.
    pub fn fabric_stream_catch_up_committed(
        &mut self,
        stream: &str,
        partition: u16,
        replication_factor: usize,
        max_records_per_replica: usize,
    ) -> io::Result<FabricStreamCatchUpReport> {
        if max_records_per_replica == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric catch-up max_records_per_replica must be greater than zero",
            ));
        }
        let placement = self.fabric_stream_placement(stream, partition, replication_factor)?;
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream catch-up requires distribution",
            )
        })?;
        if placement.leader != local {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only the Fabric stream leader may catch up replicas",
            ));
        }

        let committed = self.fabric_stream_committed_sequence(stream)?;
        let mut report = FabricStreamCatchUpReport::default();
        if committed == 0 {
            return Ok(report);
        }
        let stream_config = self.fabric_stream_config(stream)?;

        for replica in placement
            .replicas
            .iter()
            .copied()
            .filter(|node| *node != local)
        {
            report.replicas_examined += 1;
            let progress = self.fabric_stream_replica_progress(stream, replica.0)?;
            if progress >= committed {
                if self.fabric_stream_dispatch_commit_update_to(replica, &placement, committed)? {
                    report.commit_updates_dispatched += 1;
                } else {
                    report.unavailable_replicas += 1;
                }
                continue;
            }

            let records = self.fabric_stream_read_committed(
                stream,
                progress.saturating_add(1),
                max_records_per_replica,
            )?;
            if records.is_empty() {
                continue;
            }

            let mut unavailable = false;
            for record in records {
                let append = FabricStreamReplicaAppend {
                    stream: stream.to_string(),
                    partition,
                    leader: placement.leader,
                    membership_fingerprint: placement.membership_fingerprint,
                    replication_factor,
                    stream_config,
                    sequence: record.sequence,
                    payload: record.payload,
                };
                if self.fabric_stream_dispatch_replica_to(replica, &append)? {
                    report.records_dispatched += 1;
                } else {
                    unavailable = true;
                    break;
                }
            }
            if unavailable {
                report.unavailable_replicas += 1;
            }
        }

        Ok(report)
    }

    pub fn fabric_stream_replication_status(
        &mut self,
        stream: &str,
        partition: u16,
        sequence: u64,
    ) -> io::Result<FabricStreamReplicationStatus> {
        let committed = self.fabric_stream_committed_sequence(stream)?;
        if sequence <= committed {
            return Ok(FabricStreamReplicationStatus {
                sequence,
                quorum: 0,
                acknowledgements: 0,
                rejections: 0,
                committed: true,
            });
        }

        let key = (stream.to_string(), partition);
        let pending = self
            .distributed
            .fabric_stream_replication
            .pending
            .get(&key)
            .and_then(|entries| entries.get(&sequence))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no pending Fabric replication ticket for {stream} partition {partition} sequence {sequence}"
                    ),
                )
            })?;

        Ok(FabricStreamReplicationStatus {
            sequence,
            quorum: pending.quorum,
            acknowledgements: pending.acknowledgements.len(),
            rejections: pending.rejections.len(),
            committed: false,
        })
    }

    pub(crate) fn fabric_stream_record_replica_ack_from_cluster(
        &mut self,
        ack: FabricStreamReplicaAck,
        cluster: &ClusterState,
    ) -> io::Result<FabricStreamReplicaAckOutcome> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream replica ACK requires distribution",
            )
        })?;
        let current = compute_stream_placement(
            local,
            Some(cluster),
            &ack.stream,
            ack.partition,
            ack.replication_factor,
        )?;
        if current.membership_fingerprint != ack.membership_fingerprint
            || current.leader != ack.leader
            || !current.replicas.contains(&ack.replica)
            || ack.replica == local
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stale or unauthorized Fabric stream replica ACK",
            ));
        }

        if ack.accepted {
            self.fabric_stream_record_replica_progress(&ack.stream, ack.replica.0, ack.sequence)?;
        }

        let committed = self.fabric_stream_committed_sequence(&ack.stream)?;
        if ack.sequence <= committed {
            return Ok(FabricStreamReplicaAckOutcome {
                placement: current,
                committed_sequence: committed,
            });
        }

        let key = (ack.stream.clone(), ack.partition);
        if self
            .distributed
            .fabric_stream_replication
            .pending
            .get(&key)
            .and_then(|entries| entries.get(&ack.sequence))
            .is_none()
        {
            self.fabric_stream_recover_pending_from_cluster(&ack.stream, cluster)?;
        }

        self.fabric_stream_record_replica_ack(ack)?;
        let committed_sequence = self.fabric_stream_committed_sequence(&current.stream)?;
        Ok(FabricStreamReplicaAckOutcome {
            placement: current,
            committed_sequence,
        })
    }

    pub(crate) fn fabric_stream_record_replica_ack(
        &mut self,
        ack: FabricStreamReplicaAck,
    ) -> io::Result<FabricStreamReplicationStatus> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream replica ACK requires distribution",
            )
        })?;
        if ack.leader != local {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric stream replica ACK targets a different leader",
            ));
        }

        let committed_before = self.fabric_stream_committed_sequence(&ack.stream)?;
        if ack.sequence <= committed_before {
            return Ok(FabricStreamReplicationStatus {
                sequence: ack.sequence,
                quorum: 0,
                acknowledgements: 0,
                rejections: 0,
                committed: true,
            });
        }

        let key = (ack.stream.clone(), ack.partition);
        let status_before_commit;
        {
            let ticket = self
                .distributed
                .fabric_stream_replication
                .pending
                .get_mut(&key)
                .and_then(|entries| entries.get_mut(&ack.sequence))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "Fabric stream replica ACK has no pending ticket",
                    )
                })?;

            if ticket.leader != ack.leader
                || ticket.membership_fingerprint != ack.membership_fingerprint
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stale Fabric stream replica ACK",
                ));
            }
            if !ticket.replicas.contains(&ack.replica) || ack.replica == local {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Fabric stream replica ACK came from a non-follower",
                ));
            }

            if ack.accepted {
                ticket.rejections.remove(&ack.replica);
                ticket.acknowledgements.insert(ack.replica);
            } else if !ticket.acknowledgements.contains(&ack.replica) {
                ticket.rejections.insert(ack.replica);
            }
            status_before_commit = FabricStreamReplicationStatus {
                sequence: ack.sequence,
                quorum: ticket.quorum,
                acknowledgements: ticket.acknowledgements.len(),
                rejections: ticket.rejections.len(),
                committed: false,
            };
        }

        let committed_after = self.fabric_stream_advance_commits(&ack.stream, ack.partition)?;
        Ok(FabricStreamReplicationStatus {
            committed: ack.sequence <= committed_after,
            ..status_before_commit
        })
    }

    fn fabric_stream_advance_commits(&mut self, stream: &str, partition: u16) -> io::Result<u64> {
        let mut committed = self.fabric_stream_committed_sequence(stream)?;
        let key = (stream.to_string(), partition);

        loop {
            let next = committed.saturating_add(1);
            let ready = self
                .distributed
                .fabric_stream_replication
                .pending
                .get(&key)
                .and_then(|entries| entries.get(&next))
                .map(|ticket| ticket.acknowledgements.len() >= ticket.quorum)
                .unwrap_or(false);
            if !ready {
                break;
            }

            self.fabric_stream_commit_through(stream, next)?;
            self.fabric_stream_remove_replication_intent(stream, next)?;
            let remove_partition = if let Some(entries) = self
                .distributed
                .fabric_stream_replication
                .pending
                .get_mut(&key)
            {
                entries.remove(&next);
                entries.is_empty()
            } else {
                false
            };
            if remove_partition {
                self.distributed
                    .fabric_stream_replication
                    .pending
                    .remove(&key);
                self.distributed
                    .fabric_stream_replication
                    .retry_schedules
                    .remove(&key);
            }
            committed = next;
        }
        Ok(committed)
    }

    /// Compute deterministic rendezvous placement for a stream partition.
    ///
    /// Failed/Suspicious members remain candidates until confirmed removed, so
    /// transient liveness disagreement cannot independently move leadership.
    pub fn fabric_stream_placement(
        &self,
        stream: &str,
        partition: u16,
        replication_factor: usize,
    ) -> io::Result<FabricStreamPlacement> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream placement requires distribution to be enabled",
            )
        })?;
        compute_stream_placement(
            local,
            self.distributed.cluster.as_ref(),
            stream,
            partition,
            replication_factor,
        )
    }

    /// Commit one leader-local record and produce the exact replica envelope.
    ///
    /// This does not claim quorum durability: remote ACK tracking is a later
    /// layer. The returned envelope is the data-plane unit replicas apply
    /// idempotently at the exact leader-assigned sequence.
    pub fn fabric_stream_prepare_replica_append(
        &mut self,
        stream: &str,
        partition: u16,
        replication_factor: usize,
        payload: &[u8],
    ) -> io::Result<(FabricStreamPlacement, FabricStreamReplicaAppend)> {
        if partition != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical Fabric stream partition logs are not implemented yet; use partition 0",
            ));
        }

        let placement = self.fabric_stream_placement(stream, partition, replication_factor)?;
        let local = self
            .distributed
            .node_id
            .expect("placement validated node id");
        if placement.leader != local {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric stream partition leader is {:?}, local node is {:?}",
                    placement.leader, local
                ),
            ));
        }

        if let Some(cluster) = self.distributed.cluster.as_ref() {
            let local_status = cluster.get_node(local).map(|member| member.status);
            if !matches!(
                local_status,
                Some(NodeStatus::Healthy | NodeStatus::Joining)
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "local Fabric stream leader is not healthy",
                ));
            }
        }

        let stream_config = self.fabric_stream_config(stream)?;
        let sequence = self.fabric_stream_append(stream, payload)?;
        let envelope = FabricStreamReplicaAppend {
            stream: stream.to_string(),
            partition,
            leader: local,
            membership_fingerprint: placement.membership_fingerprint,
            replication_factor,
            stream_config,
            sequence,
            payload: payload.to_vec(),
        };
        Ok((placement, envelope))
    }

    /// Dispatch a prepared replica append to currently reachable remote
    /// replicas through the existing NUL0 ActorMessage envelope.
    ///
    /// This reports enqueue attempts only. It does not imply remote fsync or
    /// quorum commit; application-level replica ACKs are a later layer.
    pub fn fabric_stream_dispatch_replica_append(
        &mut self,
        placement: &FabricStreamPlacement,
        append: &FabricStreamReplicaAppend,
    ) -> io::Result<FabricStreamReplicaDispatchReport> {
        if placement.stream != append.stream
            || placement.partition != append.partition
            || placement.leader != append.leader
            || placement.membership_fingerprint != append.membership_fingerprint
            || placement.replicas.len() != append.replication_factor
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric stream placement does not match replica append envelope",
            ));
        }

        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream replication requires distribution to be enabled",
            )
        })?;
        if local != placement.leader {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only the Fabric stream leader may dispatch replica appends",
            ));
        }

        let bytes = append.to_wire_bytes()?;
        let cluster = self.distributed.cluster.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream replication requires cluster membership",
            )
        })?;

        let mut report = FabricStreamReplicaDispatchReport::default();
        let mut targets = Vec::new();
        for node in &placement.replicas {
            if *node == local {
                continue;
            }
            report.intended_remote += 1;
            match cluster.get_node(*node) {
                Some(info) if matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining) => {
                    targets.push((*node, info.address));
                }
                _ => report.unavailable += 1,
            }
        }

        if targets.is_empty() {
            return Ok(report);
        }
        let transport = self.distributed.transport.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream replication requires a network transport",
            )
        })?;

        for (node, address) in targets {
            let packet = Packet::ActorMessage {
                target_actor: 0,
                behavior_name: FABRIC_STREAM_REPLICA_BEHAVIOR.to_string(),
                content_hash: None,
                payload: Vec::new(),
                string_table: Vec::new(),
                object_table: vec![(0, bytes.clone())],
                sender_actor: 0,
                sender_node: local,
                priority: MessagePriority::System,
                trace_id: None,
            };
            transport.send(node, address, packet);
            report.dispatched += 1;
        }
        Ok(report)
    }

    pub(crate) fn fabric_stream_dispatch_replica_to(
        &mut self,
        target: NodeId,
        append: &FabricStreamReplicaAppend,
    ) -> io::Result<bool> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream replication requires distribution",
            )
        })?;
        if append.leader != local {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only the Fabric stream leader may dispatch replica appends",
            ));
        }

        let address = self
            .distributed
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.get_node(target))
            .filter(|info| matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining))
            .map(|info| info.address);
        let Some(address) = address else {
            return Ok(false);
        };
        let bytes = append.to_wire_bytes()?;
        let transport = self.distributed.transport.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream replication requires a network transport",
            )
        })?;
        transport.send(
            target,
            address,
            Packet::ActorMessage {
                target_actor: 0,
                behavior_name: FABRIC_STREAM_REPLICA_BEHAVIOR.to_string(),
                content_hash: None,
                payload: Vec::new(),
                string_table: Vec::new(),
                object_table: vec![(0, bytes)],
                sender_actor: 0,
                sender_node: local,
                priority: MessagePriority::System,
                trace_id: None,
            },
        );
        Ok(true)
    }

    pub(crate) fn fabric_stream_dispatch_commit_update_to(
        &mut self,
        target: NodeId,
        placement: &FabricStreamPlacement,
        committed_sequence: u64,
    ) -> io::Result<bool> {
        if committed_sequence == 0 {
            return Ok(false);
        }
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream commit propagation requires distribution",
            )
        })?;
        if placement.leader != local || !placement.replicas.contains(&target) || target == local {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid Fabric stream commit-update target",
            ));
        }
        let address = self
            .distributed
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.get_node(target))
            .filter(|info| matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining))
            .map(|info| info.address);
        let Some(address) = address else {
            return Ok(false);
        };

        let update = FabricStreamCommitUpdate {
            stream: placement.stream.clone(),
            partition: placement.partition,
            leader: placement.leader,
            membership_fingerprint: placement.membership_fingerprint,
            replication_factor: placement.replicas.len(),
            committed_sequence,
        };
        let bytes = update.to_wire_bytes()?;
        let transport = self.distributed.transport.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream commit propagation requires a network transport",
            )
        })?;
        transport.send(
            target,
            address,
            Packet::ActorMessage {
                target_actor: 0,
                behavior_name: FABRIC_STREAM_COMMIT_BEHAVIOR.to_string(),
                content_hash: None,
                payload: Vec::new(),
                string_table: Vec::new(),
                object_table: vec![(0, bytes)],
                sender_actor: 0,
                sender_node: local,
                priority: MessagePriority::System,
                trace_id: None,
            },
        );
        Ok(true)
    }

    /// Apply a leader-produced record on a replica.
    ///
    /// The receiver recomputes placement and rejects stale membership
    /// fingerprints, wrong leaders, non-replica destinations, sequence gaps,
    /// and conflicting duplicate data.
    pub fn fabric_stream_apply_replica(
        &mut self,
        append: &FabricStreamReplicaAppend,
    ) -> io::Result<bool> {
        let placement = self.fabric_stream_placement(
            &append.stream,
            append.partition,
            append.replication_factor,
        )?;
        self.fabric_stream_apply_replica_with_placement(append, placement)
    }

    pub(crate) fn fabric_stream_apply_commit_update_from_cluster(
        &mut self,
        update: &FabricStreamCommitUpdate,
        cluster: &ClusterState,
    ) -> io::Result<()> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream commit update requires distribution",
            )
        })?;
        let placement = compute_stream_placement(
            local,
            Some(cluster),
            &update.stream,
            update.partition,
            update.replication_factor,
        )?;
        if placement.leader != update.leader
            || placement.membership_fingerprint != update.membership_fingerprint
            || !placement.replicas.contains(&local)
            || local == placement.leader
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "stale or unauthorized Fabric stream commit update",
            ));
        }

        let tail = self
            .fabric_stream_info(&update.stream)?
            .last_sequence
            .unwrap_or(0);
        if update.committed_sequence > tail {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Fabric commit update {} is ahead of local replica tail {tail}",
                    update.committed_sequence
                ),
            ));
        }
        self.fabric_stream_commit_through(&update.stream, update.committed_sequence)
    }

    pub(crate) fn fabric_stream_apply_replica_from_cluster(
        &mut self,
        append: &FabricStreamReplicaAppend,
        cluster: &ClusterState,
    ) -> io::Result<bool> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream placement requires distribution to be enabled",
            )
        })?;
        let placement = compute_stream_placement(
            local,
            Some(cluster),
            &append.stream,
            append.partition,
            append.replication_factor,
        )?;
        self.fabric_stream_apply_replica_with_placement(append, placement)
    }

    fn fabric_stream_apply_replica_with_placement(
        &mut self,
        append: &FabricStreamReplicaAppend,
        placement: FabricStreamPlacement,
    ) -> io::Result<bool> {
        if append.partition != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical Fabric stream partition logs are not implemented yet; use partition 0",
            ));
        }

        if placement.membership_fingerprint != append.membership_fingerprint {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stale Fabric stream replica append: membership fingerprint changed",
            ));
        }
        if placement.leader != append.leader {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stale Fabric stream replica append: leader changed",
            ));
        }

        let local = self
            .distributed
            .node_id
            .expect("placement validated node id");
        if !placement.replicas.contains(&local) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "local node {:?} is not a replica for Fabric stream partition",
                    local
                ),
            ));
        }

        // A follower can bootstrap the local physical stream from the leader's
        // persisted configuration. Existing streams must already match.
        let store = self.distributed.fabric_streams.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream storage is not open; call fabric_stream_open first",
            )
        })?;
        match store.stream_config(&append.stream) {
            Ok(existing) if existing != append.stream_config => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric replica stream configuration differs from leader",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                store.create_stream(&append.stream, append.stream_config)?;
            }
            Err(error) => return Err(error),
        }

        store.append_replica(&append.stream, append.sequence, &append.payload)
    }
}

fn compute_stream_placement(
    local: NodeId,
    cluster: Option<&ClusterState>,
    stream: &str,
    partition: u16,
    replication_factor: usize,
) -> io::Result<FabricStreamPlacement> {
    if stream.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric stream name cannot be empty",
        ));
    }
    if replication_factor == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Fabric stream replication_factor must be at least 1",
        ));
    }

    let mut candidates: Vec<NodeId> = match cluster {
        Some(cluster) => cluster
            .all_members()
            .into_iter()
            .filter(|member| {
                member.status != NodeStatus::Leaving && !cluster.is_removed(member.node_id)
            })
            .map(|member| member.node_id)
            .collect(),
        None => vec![local],
    };
    candidates.sort_unstable();
    candidates.dedup();

    if replication_factor > candidates.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Fabric stream replication_factor {replication_factor} exceeds known membership {}",
                candidates.len()
            ),
        ));
    }

    let membership_fingerprint = membership_fingerprint(&candidates);
    candidates
        .sort_by_key(|node| Reverse((rendezvous_score(stream, partition, *node), Reverse(node.0))));
    let replicas: Vec<NodeId> = candidates.into_iter().take(replication_factor).collect();
    let leader = *replicas
        .first()
        .expect("replication factor validation guarantees one replica");

    Ok(FabricStreamPlacement {
        stream: stream.to_string(),
        partition,
        leader,
        replicas,
        membership_fingerprint,
    })
}

fn membership_fingerprint(nodes: &[NodeId]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"nulang-fabric-stream-membership-v1");
    for node in nodes {
        hasher.update(&node.0.to_be_bytes());
    }
    let digest = hasher.finalize();
    u64::from_be_bytes(
        digest.as_bytes()[..8]
            .try_into()
            .expect("BLAKE3 digest always has at least eight bytes"),
    )
}

fn rendezvous_score(stream: &str, partition: u16, node: NodeId) -> u128 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"nulang-fabric-stream-placement-v1");
    hasher.update(&(stream.len() as u64).to_be_bytes());
    hasher.update(stream.as_bytes());
    hasher.update(&partition.to_be_bytes());
    hasher.update(&node.0.to_be_bytes());
    let digest = hasher.finalize();
    u128::from_be_bytes(
        digest.as_bytes()[..16]
            .try_into()
            .expect("BLAKE3 digest always has at least sixteen bytes"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ClusterState, FabricStreamConfig};
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    fn runtime_with_members(local_addr: SocketAddr, peer_addrs: &[SocketAddr]) -> Runtime {
        let local = NodeId::new(&local_addr);
        let mut runtime = Runtime::new();
        runtime.distributed.enabled = true;
        runtime.distributed.node_id = Some(local);
        let mut cluster = ClusterState::new(local, local_addr);
        for peer_addr in peer_addrs {
            cluster.handle_heartbeat(NodeId::new(peer_addr), *peer_addr);
        }
        runtime.distributed.cluster = Some(cluster);
        runtime
    }

    fn test_dir(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nulang-fabric-replication-{label}-{}-{id}",
            std::process::id()
        ))
    }

    #[test]
    fn placement_is_identical_across_membership_insertion_order() {
        let a = addr(33101);
        let b = addr(33102);
        let c = addr(33103);

        let first = runtime_with_members(a, &[b, c]);
        let second = runtime_with_members(c, &[a, b]);

        let p1 = first.fabric_stream_placement("orders", 0, 3).unwrap();
        let p2 = second.fabric_stream_placement("orders", 0, 3).unwrap();
        assert_eq!(p1.leader, p2.leader);
        assert_eq!(p1.replicas, p2.replicas);
        assert_eq!(p1.membership_fingerprint, p2.membership_fingerprint);
    }

    #[test]
    fn placement_does_not_move_when_member_is_only_failed() {
        let a = addr(33201);
        let b = addr(33202);
        let node_b = NodeId::new(&b);
        let mut runtime = runtime_with_members(a, &[b]);

        let before = runtime.fabric_stream_placement("orders", 0, 2).unwrap();
        runtime
            .distributed
            .cluster
            .as_mut()
            .unwrap()
            .merge_membership(vec![crate::runtime::NodeGossip {
                node_id: node_b,
                address: b,
                status: NodeStatus::Failed,
                incarnation: 2,
            }]);
        let after = runtime.fabric_stream_placement("orders", 0, 2).unwrap();

        assert_eq!(before, after);
    }

    #[test]
    fn leader_envelope_applies_idempotently_to_follower() {
        let a_addr = addr(33301);
        let b_addr = addr(33302);
        let a_id = NodeId::new(&a_addr);
        let b_id = NodeId::new(&b_addr);

        let mut a = runtime_with_members(a_addr, &[b_addr]);
        let mut b = runtime_with_members(b_addr, &[a_addr]);
        let placement = a.fabric_stream_placement("events", 0, 2).unwrap();
        assert_eq!(
            placement,
            b.fabric_stream_placement("events", 0, 2).unwrap()
        );

        let root_a = test_dir("leader");
        let root_b = test_dir("follower");
        a.fabric_stream_open(&root_a).unwrap();
        b.fabric_stream_open(&root_b).unwrap();
        a.fabric_stream_create("events", FabricStreamConfig::default())
            .unwrap();
        b.fabric_stream_create("events", FabricStreamConfig::default())
            .unwrap();

        let (leader, follower) = if placement.leader == a_id {
            (&mut a, &mut b)
        } else {
            assert_eq!(placement.leader, b_id);
            (&mut b, &mut a)
        };

        let (_, append) = leader
            .fabric_stream_prepare_replica_append("events", 0, 2, b"hello")
            .unwrap();
        assert!(follower.fabric_stream_apply_replica(&append).unwrap());
        assert!(!follower.fabric_stream_apply_replica(&append).unwrap());

        let records = follower.fabric_stream_read("events", 1, 10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sequence, 1);
        assert_eq!(records[0].payload, b"hello");

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[test]
    fn recovery_removes_intent_reserved_before_missing_append() {
        let local_addr = addr(33501);
        let local = NodeId::new(&local_addr);
        let mut runtime = runtime_with_members(local_addr, &[]);
        let root = test_dir("orphan-intent");
        runtime.fabric_stream_open(&root).unwrap();
        runtime
            .fabric_stream_create("events", FabricStreamConfig::default())
            .unwrap();

        let placement = runtime.fabric_stream_placement("events", 0, 1).unwrap();
        runtime
            .fabric_stream_reserve_replication_intent(
                "events",
                FabricStreamPendingIntent {
                    partition: 0,
                    leader: local.0,
                    membership_fingerprint: placement.membership_fingerprint,
                    replication_factor: 1,
                    replicas: vec![local.0],
                    sequence: 1,
                },
            )
            .unwrap();

        let report = runtime.fabric_stream_recover_pending("events").unwrap();
        assert_eq!(report.recovered, 0);
        assert_eq!(report.removed_orphan_reservations, 1);
        assert!(runtime
            .fabric_stream_pending_replication_intents("events")
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_cleans_intent_left_behind_after_durable_commit() {
        let local_addr = addr(33502);
        let local = NodeId::new(&local_addr);
        let mut runtime = runtime_with_members(local_addr, &[]);
        let root = test_dir("committed-intent");
        runtime.fabric_stream_open(&root).unwrap();
        runtime
            .fabric_stream_create("events", FabricStreamConfig::default())
            .unwrap();

        let placement = runtime.fabric_stream_placement("events", 0, 1).unwrap();
        runtime
            .fabric_stream_reserve_replication_intent(
                "events",
                FabricStreamPendingIntent {
                    partition: 0,
                    leader: local.0,
                    membership_fingerprint: placement.membership_fingerprint,
                    replication_factor: 1,
                    replicas: vec![local.0],
                    sequence: 1,
                },
            )
            .unwrap();
        runtime
            .fabric_stream_append_reserved_replica("events", 1, b"committed")
            .unwrap();
        runtime.fabric_stream_commit_through("events", 1).unwrap();

        let report = runtime.fabric_stream_recover_pending("events").unwrap();
        assert_eq!(report.recovered, 0);
        assert_eq!(report.removed_committed, 1);
        assert!(runtime
            .fabric_stream_pending_replication_intents("events")
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn commit_update_wire_roundtrip_preserves_contract() {
        let update = FabricStreamCommitUpdate {
            stream: "orders".into(),
            partition: 0,
            leader: NodeId(10),
            membership_fingerprint: 44,
            replication_factor: 3,
            committed_sequence: 7,
        };
        let bytes = update.to_wire_bytes().unwrap();
        assert_eq!(
            FabricStreamCommitUpdate::from_wire_bytes(&bytes).unwrap(),
            update
        );
    }

    #[test]
    fn replica_ack_wire_roundtrip_preserves_contract() {
        let ack = FabricStreamReplicaAck {
            stream: "orders".into(),
            partition: 0,
            leader: NodeId(10),
            membership_fingerprint: 44,
            replication_factor: 3,
            sequence: 3,
            replica: NodeId(11),
            accepted: true,
        };
        let bytes = ack.to_wire_bytes().unwrap();
        assert_eq!(
            FabricStreamReplicaAck::from_wire_bytes(&bytes).unwrap(),
            ack
        );
    }

    #[test]
    fn replica_envelope_wire_roundtrip_preserves_contract() {
        let append = FabricStreamReplicaAppend {
            stream: "events".into(),
            partition: 0,
            leader: NodeId(42),
            membership_fingerprint: 99,
            replication_factor: 3,
            stream_config: FabricStreamConfig::default(),
            sequence: 7,
            payload: b"hello".to_vec(),
        };
        let bytes = append.to_wire_bytes().unwrap();
        let decoded = FabricStreamReplicaAppend::from_wire_bytes(&bytes).unwrap();
        assert_eq!(decoded, append);
    }

    #[test]
    fn stale_envelope_is_rejected_after_confirmed_membership_change() {
        let a_addr = addr(33401);
        let b_addr = addr(33402);
        let a_id = NodeId::new(&a_addr);
        let b_id = NodeId::new(&b_addr);

        let mut a = runtime_with_members(a_addr, &[b_addr]);
        let mut b = runtime_with_members(b_addr, &[a_addr]);
        let placement = a.fabric_stream_placement("events", 0, 2).unwrap();

        let root_a = test_dir("stale-a");
        let root_b = test_dir("stale-b");
        a.fabric_stream_open(&root_a).unwrap();
        b.fabric_stream_open(&root_b).unwrap();
        a.fabric_stream_create("events", FabricStreamConfig::default())
            .unwrap();
        b.fabric_stream_create("events", FabricStreamConfig::default())
            .unwrap();

        let (leader, follower, removed) = if placement.leader == a_id {
            (&mut a, &mut b, a_id)
        } else {
            (&mut b, &mut a, b_id)
        };
        let (_, append) = leader
            .fabric_stream_prepare_replica_append("events", 0, 2, b"before-removal")
            .unwrap();

        // Confirming removal changes the stable placement membership. A stale
        // envelope from the old membership must not be accepted afterward.
        follower
            .distributed
            .cluster
            .as_mut()
            .unwrap()
            .mark_removed(removed);
        let error = follower
            .fabric_stream_apply_replica(&append)
            .expect_err("stale membership envelope must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }
}
