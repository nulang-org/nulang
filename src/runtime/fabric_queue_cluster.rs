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
    queue_mutation_stream_name, queue_stream_name, validate_queue_name,
};
use super::fabric_stream::{
    FabricStreamReplicationPolicy, FABRIC_STREAM_INITIAL_EPOCH,
};
use super::{FabricStreamConfig, MessagePriority, NodeId, NodeStatus, Packet, Runtime};

const QUEUE_PLACEMENT_PREFIX: &str = "__queue_owner.";
pub(crate) const FABRIC_QUEUE_POLICY_BEHAVIOR: &str = "__nulang_fabric_queue_policy_v1";
pub(crate) const FABRIC_QUEUE_POLICY_ACK_BEHAVIOR: &str =
    "__nulang_fabric_queue_policy_ack_v1";

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
        serde_json::to_vec(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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
    pub(crate) fn accepted(
        install: &FabricQueuePolicyInstall,
        replica: NodeId,
    ) -> Self {
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

    pub(crate) fn rejected(
        install: &FabricQueuePolicyInstall,
        replica: NodeId,
    ) -> Self {
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
        serde_json::to_vec(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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
                format!(
                    "Fabric queue {queue:?} has a partially installed replication policy"
                ),
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
                self.fabric_stream_establish_replication_policy(
                    &mutations,
                    installed.clone(),
                )?;
                installed.clone()
            }
            (None, Some(installed)) => {
                validate_requested_policy(installed, partition, replication_factor)?;
                require_empty_stream(self, &mutations)?;
                require_unowned_empty_stream(self, &payload)?;
                self.fabric_stream_establish_replication_policy(
                    &payload,
                    installed.clone(),
                )?;
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

                self.fabric_stream_establish_replication_policy(
                    &payload,
                    policy.clone(),
                )?;
                self.fabric_stream_establish_replication_policy(
                    &mutations,
                    policy.clone(),
                )?;
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
        assert_eq!(
            placement.replicas,
            vec![NodeId(11), NodeId(22), NodeId(33)]
        );
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
    fn installed_policy_request_must_match_partition_and_replication_factor() {
        let policy = sample_policy();
        assert!(validate_requested_policy(&policy, 0, 3).is_ok());
        assert_eq!(
            validate_requested_policy(&policy, 0, 2)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            validate_requested_policy(&policy, 1, 3)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
