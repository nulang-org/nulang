use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use nulang::runtime::{
    DeterministicNetworkTransport, FabricStreamConfig, IncomingPacket, NodeId, OutgoingPacket,
    Runtime,
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
    let transport =
        DeterministicNetworkTransport::bind_with_bus(addr, bus).expect("transport should bind");
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
        "nulang-fabric-network-replication-{label}-{}-{id}",
        std::process::id()
    ))
}

#[test]
fn stream_replica_dispatch_uses_existing_actor_message_transport() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:34101".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34102".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut a = runtime(addr_a, bus.clone());
    let mut b = runtime(addr_b, bus);

    a.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_b, addr_b);
    b.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_a, addr_a);

    let placement_a = a.fabric_stream_placement("orders", 0, 2).unwrap();
    let placement_b = b.fabric_stream_placement("orders", 0, 2).unwrap();
    assert_eq!(placement_a, placement_b);

    let root_a = temp_dir("a");
    let root_b = temp_dir("b");
    a.fabric_stream_open(&root_a).unwrap();
    b.fabric_stream_open(&root_b).unwrap();

    let (leader, follower) = if placement_a.leader == node_a {
        (&mut a, &mut b)
    } else {
        assert_eq!(placement_a.leader, node_b);
        (&mut b, &mut a)
    };

    // Only the leader is pre-provisioned. The follower must bootstrap the
    // durable stream metadata from the validated replica envelope.
    leader
        .fabric_stream_create("orders", FabricStreamConfig::default())
        .unwrap();

    let (placement, append) = leader
        .fabric_stream_prepare_replica_append("orders", 0, 2, b"order-1")
        .unwrap();
    let report = leader
        .fabric_stream_dispatch_replica_append(&placement, &append)
        .unwrap();
    assert_eq!(report.intended_remote, 1);
    assert_eq!(report.dispatched, 1);
    assert_eq!(report.unavailable, 0);

    follower.process_network();
    let records = follower.fabric_stream_read("orders", 1, 10).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].sequence, 1);
    assert_eq!(records[0].payload, b"order-1");

    // Re-dispatching the same leader envelope is a valid network retry, not a
    // duplicate durable record.
    let retry = leader
        .fabric_stream_dispatch_replica_append(&placement, &append)
        .unwrap();
    assert_eq!(retry.dispatched, 1);
    follower.process_network();
    let records = follower.fabric_stream_read("orders", 1, 10).unwrap();
    assert_eq!(records.len(), 1);

    let _ = std::fs::remove_dir_all(root_a);
    let _ = std::fs::remove_dir_all(root_b);
}


#[test]
fn stream_quorum_commit_waits_for_application_ack() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:34201".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34202".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut a = runtime(addr_a, bus.clone());
    let mut b = runtime(addr_b, bus);

    a.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_b, addr_b);
    b.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_a, addr_a);

    let placement = a.fabric_stream_placement("quorum", 0, 2).unwrap();
    assert_eq!(placement, b.fabric_stream_placement("quorum", 0, 2).unwrap());

    let root_a = temp_dir("quorum-a");
    let root_b = temp_dir("quorum-b");
    a.fabric_stream_open(&root_a).unwrap();
    b.fabric_stream_open(&root_b).unwrap();

    let (leader, follower) = if placement.leader == node_a {
        (&mut a, &mut b)
    } else {
        (&mut b, &mut a)
    };
    leader
        .fabric_stream_create("quorum", FabricStreamConfig::default())
        .unwrap();

    let result = leader
        .fabric_stream_replicated_append("quorum", 0, 2, b"pending")
        .unwrap();
    assert_eq!(result.status.quorum, 2);
    assert_eq!(result.status.acknowledgements, 1);
    assert!(!result.status.committed);
    assert_eq!(
        leader.fabric_stream_read("quorum", 1, 10).unwrap().len(),
        1
    );
    assert!(leader
        .fabric_stream_read_committed("quorum", 1, 10)
        .unwrap()
        .is_empty());

    // Follower fsyncs the exact record and emits an application ACK.
    follower.process_network();
    // Leader consumes the application ACK and advances the durable commit index.
    leader.process_network();

    assert_eq!(
        leader.fabric_stream_committed_sequence("quorum").unwrap(),
        1
    );
    let committed = leader
        .fabric_stream_read_committed("quorum", 1, 10)
        .unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].payload, b"pending");

    // Followers currently retain durable replica data but do not independently
    // claim leader commit visibility; commit propagation is a later layer.
    assert_eq!(
        follower.fabric_stream_committed_sequence("quorum").unwrap(),
        0
    );

    let _ = std::fs::remove_dir_all(root_a);
    let _ = std::fs::remove_dir_all(root_b);
}

#[test]
fn three_replica_stream_commits_on_majority() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addrs: Vec<SocketAddr> = ["127.0.0.1:34301", "127.0.0.1:34302", "127.0.0.1:34303"]
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
            if i == j {
                continue;
            }
            nodes[i]
                .distributed
                .cluster
                .as_mut()
                .unwrap()
                .handle_heartbeat(ids[j], addrs[j]);
        }
    }

    let placement = nodes[0]
        .fabric_stream_placement("majority", 0, 3)
        .unwrap();
    for node in &nodes[1..] {
        assert_eq!(
            node.fabric_stream_placement("majority", 0, 3).unwrap(),
            placement
        );
    }

    let roots: Vec<PathBuf> = (0..3)
        .map(|index| temp_dir(&format!("majority-{index}")))
        .collect();
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }

    let leader_index = ids
        .iter()
        .position(|id| *id == placement.leader)
        .unwrap();
    nodes[leader_index]
        .fabric_stream_create("majority", FabricStreamConfig::default())
        .unwrap();

    let result = nodes[leader_index]
        .fabric_stream_replicated_append("majority", 0, 3, b"quorum")
        .unwrap();
    assert_eq!(result.status.quorum, 2);
    assert_eq!(result.status.acknowledgements, 1);
    assert!(!result.status.committed);

    // Process exactly one follower. Leader + this follower is a majority of 3.
    let follower_index = (0..3).find(|index| *index != leader_index).unwrap();
    nodes[follower_index].process_network();
    nodes[leader_index].process_network();

    assert_eq!(
        nodes[leader_index]
            .fabric_stream_committed_sequence("majority")
            .unwrap(),
        1
    );
    assert_eq!(
        nodes[leader_index]
            .fabric_stream_read_committed("majority", 1, 10)
            .unwrap()
            .len(),
        1
    );

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}
