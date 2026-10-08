//! Verify the deliberately local scope of Fabric durable consumer state.
//!
//! Stream records and their commit index survive a replicated leader transition,
//! but acknowledged consumer cursors and leases are not yet quorum replicated.
//! A promoted replica must therefore replay an ACKed record (at-least-once),
//! rather than claim JetStream-style durable consumer continuity.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use nulang::runtime::{
    ClusterState, DeterministicNetworkTransport, FabricStreamConfig, IncomingPacket, NodeId,
    OutgoingPacket, Packet, Runtime,
};

type Bus = Arc<
    parking_lot::Mutex<
        HashMap<
            NodeId,
            (
                std::sync::mpsc::SyncSender<IncomingPacket>,
                std::sync::mpsc::SyncSender<OutgoingPacket>,
            ),
        >,
    >,
>;

fn runtime(addr: SocketAddr, bus: Bus) -> Runtime {
    let mut runtime = Runtime::new();
    runtime.install_virtual_clock();
    let transport = DeterministicNetworkTransport::bind_with_bus(addr, bus)
        .expect("deterministic network transport should bind");
    transport.register_on_bus();
    runtime
        .enable_distribution_with_transport(Box::new(transport))
        .expect("distribution should enable");
    runtime
}

fn temp_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nulang-fabric-consumer-failover-{label}-{}-{id}",
        std::process::id()
    ))
}

#[test]
fn promoted_replica_replays_acknowledged_record_without_consumer_state_replication() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addrs: Vec<SocketAddr> = ["127.0.0.1:39121", "127.0.0.1:39122", "127.0.0.1:39123"]
        .into_iter()
        .map(|addr| addr.parse().unwrap())
        .collect();
    let ids: Vec<NodeId> = addrs.iter().map(NodeId::new).collect();
    let mut nodes: Vec<Runtime> = addrs
        .iter()
        .copied()
        .map(|addr| runtime(addr, bus.clone()))
        .collect();

    for i in 0..nodes.len() {
        for j in 0..nodes.len() {
            if i != j {
                nodes[i]
                    .distributed
                    .cluster
                    .as_mut()
                    .unwrap()
                    .handle_heartbeat(ids[j], addrs[j]);
            }
        }
    }

    let initial = nodes[0]
        .fabric_stream_placement("consumer-failover", 0, 3)
        .unwrap();
    let old_leader = ids.iter().position(|id| *id == initial.leader).unwrap();
    let survivors: Vec<usize> = (0..3).filter(|index| *index != old_leader).collect();

    let roots: Vec<PathBuf> = (0..3)
        .map(|index| temp_dir(&format!("node-{index}")))
        .collect();
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[old_leader]
        .fabric_stream_create("consumer-failover", FabricStreamConfig::default())
        .unwrap();
    let append = nodes[old_leader]
        .fabric_stream_replicated_append("consumer-failover", 0, 3, b"charge-once")
        .unwrap();
    assert!(!append.status.committed);

    // Replicate the record and propagate the committed index to both followers.
    for index in &survivors {
        nodes[*index].process_network();
    }
    nodes[old_leader].process_network();
    nodes[old_leader].process_network();
    for index in &survivors {
        nodes[*index].process_network();
        nodes[*index].process_network();
        assert_eq!(
            nodes[*index]
                .fabric_stream_committed_sequence("consumer-failover")
                .unwrap(),
            1
        );
    }

    // Successful ACK is durable *on the old leader*, not on its followers.
    let original_delivery = nodes[old_leader]
        .fabric_stream_deliver_consumer(
            "consumer-failover",
            "billing",
            10,
            Duration::from_secs(30),
        )
        .unwrap();
    assert_eq!(original_delivery.len(), 1);
    assert_eq!(original_delivery[0].leader_epoch, Some(1));
    nodes[old_leader]
        .fabric_stream_ack_consumer_fenced(
            "consumer-failover",
            "billing",
            1,
            original_delivery[0].leader_epoch.unwrap(),
        )
        .unwrap();
    assert_eq!(
        nodes[old_leader]
            .fabric_stream_cursor("consumer-failover", "billing")
            .unwrap(),
        1
    );
    for index in &survivors {
        assert_eq!(
            nodes[*index]
                .fabric_stream_cursor("consumer-failover", "billing")
                .unwrap(),
            0
        );
    }

    // Determine the deterministic leader of the surviving two-replica policy.
    let scratch_local = survivors[0];
    let scratch_peer = survivors[1];
    let mut scratch = Runtime::new();
    scratch.distributed.enabled = true;
    scratch.distributed.node_id = Some(ids[scratch_local]);
    let mut scratch_cluster = ClusterState::new(ids[scratch_local], addrs[scratch_local]);
    scratch_cluster.handle_heartbeat(ids[scratch_peer], addrs[scratch_peer]);
    scratch.distributed.cluster = Some(scratch_cluster);
    let reduced = scratch
        .fabric_stream_placement("consumer-failover", 0, 2)
        .unwrap();
    let candidate = survivors
        .iter()
        .copied()
        .find(|index| ids[*index] == reduced.leader)
        .unwrap();
    let voter = survivors
        .iter()
        .copied()
        .find(|index| *index != candidate)
        .unwrap();

    // Confirm graceful node departure to both replicas, then terminate the
    // original runtime before electing the replacement. This is an actual
    // leadership transition, not merely reopening the same leader directory.
    for index in &survivors {
        nodes[old_leader]
            .distributed
            .transport
            .as_mut()
            .unwrap()
            .send(
                ids[*index],
                addrs[*index],
                Packet::NodeGoodbye {
                    node_id: ids[old_leader],
                    durable: Vec::new(),
                },
            );
    }
    let terminated = std::mem::replace(&mut nodes[old_leader], Runtime::new());
    drop(terminated);

    nodes[candidate].process_network();
    nodes[voter].process_network();
    nodes[candidate].process_network();
    nodes[voter].process_network();
    for index in &survivors {
        assert_eq!(
            nodes[*index]
                .fabric_stream_epoch("consumer-failover")
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            nodes[*index]
                .fabric_stream_committed_sequence("consumer-failover")
                .unwrap(),
            1
        );
    }

    // A newly elected leader has the committed stream record but cannot
    // reconstruct the previous leader's ACK/lease state from a quorum.
    // It must not deliver or mutate consumer progress until explicit
    // metadata recovery is implemented. Replay is NOT automatically safe.
    assert_eq!(
        nodes[candidate]
            .fabric_stream_cursor("consumer-failover", "billing")
            .unwrap(),
        0
    );
    assert!(nodes[candidate]
        .fabric_stream_read_consumer("consumer-failover", "billing", 10)
        .is_err());
    assert!(nodes[candidate]
        .fabric_stream_deliver_consumer(
            "consumer-failover",
            "billing",
            10,
            Duration::from_secs(30),
        )
        .is_err());
    assert!(nodes[candidate]
        .fabric_stream_ack_consumer_fenced("consumer-failover", "billing", 1, 1)
        .is_err());
    assert!(nodes[candidate]
        .fabric_stream_ack_consumer_fenced("consumer-failover", "billing", 1, 2)
        .is_err());
    assert!(nodes[candidate]
        .fabric_stream_nack_consumer_fenced("consumer-failover", "billing", 1, 2)
        .is_err());
    assert!(nodes[candidate]
        .fabric_stream_commit_cursor_fenced("consumer-failover", "billing", 1, 2)
        .is_err());
    assert_eq!(
        nodes[candidate]
            .fabric_stream_cursor("consumer-failover", "billing")
            .unwrap(),
        0
    );

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}
