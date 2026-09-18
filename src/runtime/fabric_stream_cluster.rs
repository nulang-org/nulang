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
use std::io;

use crate::runtime::{
    FabricStreamConfig, NodeId, NodeStatus, Runtime,
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

impl Runtime {
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

        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric stream placement requires distribution to be enabled",
            )
        })?;

        let mut candidates: Vec<NodeId> = match self.distributed.cluster.as_ref() {
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
        candidates.sort_by_key(|node| {
            Reverse((rendezvous_score(stream, partition, *node), Reverse(node.0)))
        });
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
        let local = self.distributed.node_id.expect("placement validated node id");
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
            if !matches!(local_status, Some(NodeStatus::Healthy | NodeStatus::Joining)) {
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

    /// Apply a leader-produced record on a replica.
    ///
    /// The receiver recomputes placement and rejects stale membership
    /// fingerprints, wrong leaders, non-replica destinations, sequence gaps,
    /// and conflicting duplicate data.
    pub fn fabric_stream_apply_replica(
        &mut self,
        append: &FabricStreamReplicaAppend,
    ) -> io::Result<bool> {
        if append.partition != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical Fabric stream partition logs are not implemented yet; use partition 0",
            ));
        }

        let placement = self.fabric_stream_placement(
            &append.stream,
            append.partition,
            append.replication_factor,
        )?;
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

        let local = self.distributed.node_id.expect("placement validated node id");
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

    fn runtime_with_members(
        local_addr: SocketAddr,
        peer_addrs: &[SocketAddr],
    ) -> Runtime {
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
