use std::collections::{HashMap, HashSet};
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
    assert_eq!(
        placement,
        b.fabric_stream_placement("quorum", 0, 2).unwrap()
    );

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
    assert_eq!(leader.fabric_stream_read("quorum", 1, 10).unwrap().len(), 1);
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

    let placement = nodes[0].fabric_stream_placement("majority", 0, 3).unwrap();
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

    let leader_index = ids.iter().position(|id| *id == placement.leader).unwrap();
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

#[test]
fn replica_nack_does_not_advance_quorum_commit() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:34401".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34402".parse().unwrap();
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

    let placement = a.fabric_stream_placement("nack", 0, 2).unwrap();
    let root_a = temp_dir("nack-a");
    let root_b = temp_dir("nack-b");
    a.fabric_stream_open(&root_a).unwrap();
    b.fabric_stream_open(&root_b).unwrap();

    let (leader, follower) = if placement.leader == node_a {
        (&mut a, &mut b)
    } else {
        (&mut b, &mut a)
    };

    leader
        .fabric_stream_create("nack", FabricStreamConfig::default())
        .unwrap();
    follower
        .fabric_stream_create(
            "nack",
            FabricStreamConfig {
                segment_max_bytes: 128 * 1024,
            },
        )
        .unwrap();

    let result = leader
        .fabric_stream_replicated_append("nack", 0, 2, b"will-reject")
        .unwrap();
    assert_eq!(result.status.acknowledgements, 1);
    assert_eq!(result.status.rejections, 0);

    follower.process_network();
    leader.process_network();

    let status = leader
        .fabric_stream_replication_status("nack", 0, result.sequence)
        .unwrap();
    assert_eq!(status.acknowledgements, 1);
    assert_eq!(status.rejections, 1);
    assert!(!status.committed);
    assert_eq!(leader.fabric_stream_committed_sequence("nack").unwrap(), 0);

    let _ = std::fs::remove_dir_all(root_a);
    let _ = std::fs::remove_dir_all(root_b);
}

#[test]
fn leader_restart_recovers_pending_ticket_and_retries_replica() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:34501".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34502".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut nodes = vec![runtime(addr_a, bus.clone()), runtime(addr_b, bus.clone())];
    nodes[0]
        .distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_b, addr_b);
    nodes[1]
        .distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_a, addr_a);

    let placement = nodes[0].fabric_stream_placement("recovery", 0, 2).unwrap();
    assert_eq!(
        placement,
        nodes[1].fabric_stream_placement("recovery", 0, 2).unwrap()
    );

    let roots = vec![temp_dir("recovery-a"), temp_dir("recovery-b")];
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }

    let leader_index = if placement.leader == node_a { 0 } else { 1 };
    let follower_index = 1 - leader_index;
    nodes[leader_index]
        .fabric_stream_create("recovery", FabricStreamConfig::default())
        .unwrap();

    let initial = nodes[leader_index]
        .fabric_stream_replicated_append("recovery", 0, 2, b"survive-restart")
        .unwrap();
    assert_eq!(initial.status.acknowledgements, 1);
    assert!(!initial.status.committed);

    // Crash the leader before the follower processes the first dispatch.
    let leader_addr = if leader_index == 0 { addr_a } else { addr_b };
    let follower_addr = if follower_index == 0 { addr_a } else { addr_b };
    let follower_id = if follower_index == 0 { node_a } else { node_b };
    let dead = std::mem::replace(&mut nodes[leader_index], Runtime::new());
    drop(dead);

    // Registering the restarted transport at the same address replaces the
    // dead endpoint in the deterministic bus, preserving stable NodeId.
    let mut restarted = runtime(leader_addr, bus.clone());
    restarted
        .distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(follower_id, follower_addr);
    restarted.fabric_stream_open(&roots[leader_index]).unwrap();

    let recovered = restarted.fabric_stream_recover_pending("recovery").unwrap();
    assert_eq!(recovered.recovered, 1);
    assert_eq!(recovered.removed_committed, 0);
    assert_eq!(recovered.removed_orphan_reservations, 0);

    let status = restarted
        .fabric_stream_replication_status("recovery", 0, initial.sequence)
        .unwrap();
    assert_eq!(status.acknowledgements, 1);
    assert!(!status.committed);

    let retry = restarted
        .fabric_stream_retry_pending("recovery", 0)
        .unwrap();
    assert_eq!(retry.pending_sequences, 1);
    assert_eq!(retry.dispatched, 1);

    // Duplicate delivery is possible: the original pre-crash dispatch may
    // still be queued. Exact-sequence follower application is idempotent.
    nodes[follower_index].process_network();
    nodes[follower_index].process_network();
    restarted.process_network();
    restarted.process_network();

    assert_eq!(
        restarted
            .fabric_stream_committed_sequence("recovery")
            .unwrap(),
        1
    );
    assert_eq!(
        restarted
            .fabric_stream_read_committed("recovery", 1, 10)
            .unwrap()[0]
            .payload,
        b"survive-restart"
    );

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn lagging_committed_replica_catches_up_and_receives_commit_boundary() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addrs: Vec<SocketAddr> = ["127.0.0.1:34601", "127.0.0.1:34602", "127.0.0.1:34603"]
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

    let placement = nodes[0].fabric_stream_placement("catchup", 0, 3).unwrap();
    let leader_index = ids.iter().position(|id| *id == placement.leader).unwrap();
    let followers: Vec<usize> = (0..3).filter(|index| *index != leader_index).collect();
    let quorum_follower = followers[0];
    let lagging_follower = followers[1];

    let roots: Vec<PathBuf> = (0..3)
        .map(|index| temp_dir(&format!("catchup-{index}")))
        .collect();
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[leader_index]
        .fabric_stream_create("catchup", FabricStreamConfig::default())
        .unwrap();

    // Drop leader -> lagging follower traffic while keeping membership stable.
    nodes[leader_index]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([ids[lagging_follower]]));

    let append = nodes[leader_index]
        .fabric_stream_replicated_append("catchup", 0, 3, b"committed-with-majority")
        .unwrap();
    assert_eq!(append.status.quorum, 2);
    assert_eq!(append.status.acknowledgements, 1);

    nodes[quorum_follower].process_network();
    nodes[leader_index].process_network();
    nodes[quorum_follower].process_network();

    assert_eq!(
        nodes[leader_index]
            .fabric_stream_committed_sequence("catchup")
            .unwrap(),
        1
    );
    assert_eq!(
        nodes[quorum_follower]
            .fabric_stream_committed_sequence("catchup")
            .unwrap(),
        1
    );

    // Heal the link. Durable progress says the lagging replica has ACKed
    // nothing, so catch-up sends sequence 1 only to that replica.
    nodes[leader_index]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());

    let catchup = nodes[leader_index]
        .fabric_stream_catch_up_committed("catchup", 0, 3, 10)
        .unwrap();
    assert_eq!(catchup.records_dispatched, 1);
    assert_eq!(catchup.unavailable_replicas, 0);

    nodes[lagging_follower].process_network();
    nodes[leader_index].process_network();
    nodes[lagging_follower].process_network();

    let records = nodes[lagging_follower]
        .fabric_stream_read("catchup", 1, 10)
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].payload, b"committed-with-majority");
    assert_eq!(
        nodes[lagging_follower]
            .fabric_stream_committed_sequence("catchup")
            .unwrap(),
        1
    );

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}
