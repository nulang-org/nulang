//! Queue-level ownership for replicated Fabric queues.
//!
//! A queue owns multiple internal Fabric streams (currently payload and
//! mutation metadata). They must share one epoch, leader, replica set, and
//! membership fingerprint. Deriving placement independently from each stream
//! name is unsafe because rendezvous hashing could choose different leaders.
//!
//! This module establishes one logical queue placement and installs it
//! identically on every internal stream before replicated queue mutations are
//! allowed. Network propagation of this queue policy is the next layer.

use std::io;

use super::fabric_queue::{
    queue_mutation_stream_name, queue_stream_name, validate_queue_name,
};
use super::fabric_stream::{
    FabricStreamReplicationPolicy, FABRIC_STREAM_INITIAL_EPOCH,
};
use super::{FabricStreamConfig, NodeId, Runtime};

const QUEUE_PLACEMENT_PREFIX: &str = "__queue_owner.";

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
}

impl Runtime {
    /// Return the installed queue-level placement if both internal streams are
    /// governed by the same replication policy.
    ///
    /// A partial or divergent installation is reported as an error rather
    /// than choosing one internal stream as authoritative.
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

    /// Bootstrap or resume installation of one replication policy across every
    /// internal stream belonging to a queue.
    ///
    /// This is crate-private until the queue-policy control message is wired.
    /// Calling it independently on cluster members is not yet a supported
    /// distributed creation protocol.
    pub(crate) fn fabric_queue_bootstrap_replication_policy(
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

                // Establishing either side is retry-safe. A crash after the
                // first write is repaired by the partial-policy branches above
                // as long as the unowned peer still has no durable history.
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
