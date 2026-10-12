use std::collections::{HashMap, HashSet};
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

#[test]
fn pending_stream_replication_retries_automatically_on_logical_clock() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:34701".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34702".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut nodes = vec![runtime(addr_a, bus.clone()), runtime(addr_b, bus)];
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

    let placement = nodes[0]
        .fabric_stream_placement("auto-retry", 0, 2)
        .unwrap();
    let leader_index = if placement.leader == node_a { 0 } else { 1 };
    let follower_index = 1 - leader_index;
    let follower_id = if follower_index == 0 { node_a } else { node_b };

    let roots = vec![temp_dir("auto-retry-a"), temp_dir("auto-retry-b")];
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[leader_index]
        .fabric_stream_create("auto-retry", FabricStreamConfig::default())
        .unwrap();

    nodes[leader_index]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([follower_id]));

    let append = nodes[leader_index]
        .fabric_stream_replicated_append("auto-retry", 0, 2, b"retry-me")
        .unwrap();
    assert!(!append.status.committed);

    // Heal immediately, but the scheduler must respect its 500ms initial delay.
    nodes[leader_index]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());
    nodes[leader_index].advance_time(Duration::from_millis(499));
    nodes[leader_index].process_network();
    nodes[follower_index].process_network();
    assert!(nodes[follower_index]
        .fabric_stream_read("auto-retry", 1, 10)
        .is_err());

    nodes[leader_index].advance_time(Duration::from_millis(1));
    nodes[leader_index].process_network();
    nodes[follower_index].process_network();
    nodes[leader_index].process_network();
    nodes[follower_index].process_network();

    assert_eq!(
        nodes[leader_index]
            .fabric_stream_committed_sequence("auto-retry")
            .unwrap(),
        1
    );
    assert_eq!(
        nodes[follower_index]
            .fabric_stream_committed_sequence("auto-retry")
            .unwrap(),
        1
    );

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn quorum_epoch_transition_fences_removed_old_leader() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addrs: Vec<SocketAddr> = ["127.0.0.1:34801", "127.0.0.1:34802", "127.0.0.1:34803"]
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

    let initial = nodes[0]
        .fabric_stream_placement("epoch-transition", 0, 3)
        .unwrap();
    let old_leader = ids.iter().position(|node| *node == initial.leader).unwrap();
    let roots: Vec<PathBuf> = (0..3)
        .map(|index| temp_dir(&format!("epoch-transition-{index}")))
        .collect();
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[old_leader]
        .fabric_stream_create("epoch-transition", FabricStreamConfig::default())
        .unwrap();

    let first = nodes[old_leader]
        .fabric_stream_replicated_append("epoch-transition", 0, 3, b"epoch-one")
        .unwrap();
    assert_eq!(first.sequence, 1);

    for index in 0..3 {
        if index != old_leader {
            nodes[index].process_network();
        }
    }
    nodes[old_leader].process_network();
    nodes[old_leader].process_network();
    for index in 0..3 {
        if index != old_leader {
            nodes[index].process_network();
            nodes[index].process_network();
        }
    }

    for node in &mut nodes {
        assert_eq!(
            node.fabric_stream_epoch("epoch-transition").unwrap(),
            Some(1)
        );
        assert_eq!(
            node.fabric_stream_committed_sequence("epoch-transition")
                .unwrap(),
            1
        );
    }

    // Confirm the old leader removed on both surviving replicas. Their current
    // deterministic RF=2 placement is the only eligible epoch-2 policy.
    let survivors: Vec<usize> = (0..3).filter(|index| *index != old_leader).collect();
    for index in &survivors {
        nodes[*index]
            .distributed
            .cluster
            .as_mut()
            .unwrap()
            .mark_removed(ids[old_leader]);
    }

    let next = nodes[survivors[0]]
        .fabric_stream_placement("epoch-transition", 0, 2)
        .unwrap();
    let candidate = survivors
        .iter()
        .copied()
        .find(|index| ids[*index] == next.leader)
        .unwrap();
    let voter = survivors
        .iter()
        .copied()
        .find(|index| *index != candidate)
        .unwrap();

    let starting = nodes[candidate]
        .fabric_stream_begin_epoch_transition("epoch-transition", 0, 2)
        .unwrap();
    assert_eq!(starting.from_epoch, 1);
    assert_eq!(starting.to_epoch, 2);
    assert_eq!(starting.affirmative_votes, 1);
    assert!(!starting.finalized);

    // The other survivor durably promises epoch 2 and votes. The candidate
    // records the old-policy majority, installs epoch 2, and emits commit.
    nodes[voter].process_network();
    nodes[candidate].process_network();
    nodes[voter].process_network();

    for index in &survivors {
        assert_eq!(
            nodes[*index]
                .fabric_stream_epoch("epoch-transition")
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            nodes[*index]
                .fabric_stream_committed_sequence("epoch-transition")
                .unwrap(),
            1
        );
    }

    // The removed old leader still has epoch 1 locally and can durably append
    // to its own disk, but its old epoch cannot obtain a quorum from survivors.
    let stale = nodes[old_leader]
        .fabric_stream_replicated_append("epoch-transition", 0, 3, b"stale-old-epoch")
        .unwrap();
    assert_eq!(stale.status.acknowledgements, 1);
    assert!(!stale.status.committed);

    for index in &survivors {
        nodes[*index].process_network();
    }
    nodes[old_leader].process_network();

    assert_eq!(
        nodes[old_leader]
            .fabric_stream_committed_sequence("epoch-transition")
            .unwrap(),
        1
    );
    assert_eq!(
        nodes[old_leader]
            .fabric_stream_read_committed("epoch-transition", 1, 10)
            .unwrap()
            .len(),
        1
    );

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn epoch_repair_brings_lagging_survivor_to_proposal_tail() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addrs: Vec<SocketAddr> = ["127.0.0.1:34901", "127.0.0.1:34902", "127.0.0.1:34903"]
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

    let initial = nodes[0]
        .fabric_stream_placement("epoch-repair", 0, 3)
        .unwrap();
    let old_leader = ids.iter().position(|node| *node == initial.leader).unwrap();
    let survivors: Vec<usize> = (0..3).filter(|index| *index != old_leader).collect();

    // Determine which survivor will be the RF=2 leader after the old leader is
    // removed, without mutating the real cluster state yet.
    let scratch_local = survivors[0];
    let mut scratch = Runtime::new();
    scratch.distributed.enabled = true;
    scratch.distributed.node_id = Some(ids[scratch_local]);
    let mut scratch_cluster = ClusterState::new(ids[scratch_local], addrs[scratch_local]);
    let scratch_peer = survivors[1];
    scratch_cluster.handle_heartbeat(ids[scratch_peer], addrs[scratch_peer]);
    scratch.distributed.cluster = Some(scratch_cluster);
    let future = scratch
        .fabric_stream_placement("epoch-repair", 0, 2)
        .unwrap();
    let candidate = survivors
        .iter()
        .copied()
        .find(|index| ids[*index] == future.leader)
        .unwrap();
    let lagging = survivors
        .iter()
        .copied()
        .find(|index| *index != candidate)
        .unwrap();

    let roots: Vec<PathBuf> = (0..3)
        .map(|index| temp_dir(&format!("epoch-repair-{index}")))
        .collect();
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[old_leader]
        .fabric_stream_create("epoch-repair", FabricStreamConfig::default())
        .unwrap();

    // Establish epoch 1 and a common committed sequence 1.
    nodes[old_leader]
        .fabric_stream_replicated_append("epoch-repair", 0, 3, b"common")
        .unwrap();
    for index in &survivors {
        nodes[*index].process_network();
    }
    nodes[old_leader].process_network();
    nodes[old_leader].process_network();
    for index in &survivors {
        nodes[*index].process_network();
        nodes[*index].process_network();
    }

    // Sequence 2 reaches only the old leader + future candidate. That pair is
    // an RF=3 majority, so sequence 2 commits while the other survivor lags.
    nodes[old_leader]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([ids[lagging]]));
    nodes[old_leader]
        .fabric_stream_replicated_append("epoch-repair", 0, 3, b"majority-only")
        .unwrap();
    nodes[candidate].process_network();
    nodes[old_leader].process_network();
    nodes[candidate].process_network();
    nodes[old_leader]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());

    assert_eq!(
        nodes[candidate]
            .fabric_stream_committed_sequence("epoch-repair")
            .unwrap(),
        2
    );
    assert_eq!(
        nodes[lagging]
            .fabric_stream_info("epoch-repair")
            .unwrap()
            .last_sequence,
        Some(1)
    );

    for index in &survivors {
        nodes[*index]
            .distributed
            .cluster
            .as_mut()
            .unwrap()
            .mark_removed(ids[old_leader]);
    }

    let starting = nodes[candidate]
        .fabric_stream_begin_epoch_transition("epoch-repair", 0, 2)
        .unwrap();
    assert_eq!(starting.affirmative_votes, 1);

    // Lagging survivor rejects because its tail is 1 instead of candidate tail 2.
    nodes[lagging].process_network();
    nodes[candidate].process_network();

    let repair = nodes[candidate]
        .fabric_stream_repair_epoch_transition("epoch-repair", 10)
        .unwrap();
    assert_eq!(repair.records_dispatched, 1);
    assert_eq!(repair.ahead_replicas, 0);

    // Repair applies exact sequence 2, then the receiver immediately re-votes.
    nodes[lagging].process_network();
    nodes[candidate].process_network();
    nodes[lagging].process_network();

    for index in &survivors {
        assert_eq!(
            nodes[*index].fabric_stream_epoch("epoch-repair").unwrap(),
            Some(2)
        );
        assert_eq!(
            nodes[*index]
                .fabric_stream_committed_sequence("epoch-repair")
                .unwrap(),
            2
        );
        assert_eq!(
            nodes[*index]
                .fabric_stream_read_committed("epoch-repair", 1, 10)
                .unwrap()
                .len(),
            2
        );
    }

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn epoch_pull_reconciles_candidate_behind_a_survivor() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addrs: Vec<SocketAddr> = ["127.0.0.1:35001", "127.0.0.1:35002", "127.0.0.1:35003"]
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

    let initial = nodes[0]
        .fabric_stream_placement("epoch-pull", 0, 3)
        .unwrap();
    let old_leader = ids.iter().position(|node| *node == initial.leader).unwrap();
    let survivors: Vec<usize> = (0..3).filter(|index| *index != old_leader).collect();

    // Determine the future RF=2 leader after the old leader is removed.
    let scratch_local = survivors[0];
    let mut scratch = Runtime::new();
    scratch.distributed.enabled = true;
    scratch.distributed.node_id = Some(ids[scratch_local]);
    let mut scratch_cluster = ClusterState::new(ids[scratch_local], addrs[scratch_local]);
    let scratch_peer = survivors[1];
    scratch_cluster.handle_heartbeat(ids[scratch_peer], addrs[scratch_peer]);
    scratch.distributed.cluster = Some(scratch_cluster);
    let future = scratch.fabric_stream_placement("epoch-pull", 0, 2).unwrap();
    let candidate = survivors
        .iter()
        .copied()
        .find(|index| ids[*index] == future.leader)
        .unwrap();
    let ahead = survivors
        .iter()
        .copied()
        .find(|index| *index != candidate)
        .unwrap();

    let roots: Vec<PathBuf> = (0..3)
        .map(|index| temp_dir(&format!("epoch-pull-{index}")))
        .collect();
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[old_leader]
        .fabric_stream_create("epoch-pull", FabricStreamConfig::default())
        .unwrap();

    // Common committed sequence 1.
    nodes[old_leader]
        .fabric_stream_replicated_append("epoch-pull", 0, 3, b"common")
        .unwrap();
    for index in &survivors {
        nodes[*index].process_network();
    }
    nodes[old_leader].process_network();
    nodes[old_leader].process_network();
    for index in &survivors {
        nodes[*index].process_network();
        nodes[*index].process_network();
    }

    // Sequence 2 reaches the old leader + non-candidate survivor. It commits
    // under epoch 1 while the future deterministic leader remains at tail 1.
    nodes[old_leader]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([ids[candidate]]));
    nodes[old_leader]
        .fabric_stream_replicated_append("epoch-pull", 0, 3, b"ahead-survivor")
        .unwrap();
    nodes[ahead].process_network();
    nodes[old_leader].process_network();
    nodes[ahead].process_network();
    nodes[old_leader]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());

    assert_eq!(
        nodes[candidate]
            .fabric_stream_info("epoch-pull")
            .unwrap()
            .last_sequence,
        Some(1)
    );
    assert_eq!(
        nodes[ahead]
            .fabric_stream_info("epoch-pull")
            .unwrap()
            .last_sequence,
        Some(2)
    );

    for index in &survivors {
        nodes[*index]
            .distributed
            .cluster
            .as_mut()
            .unwrap()
            .mark_removed(ids[old_leader]);
    }

    // Term 2 binds candidate tail 1. The ahead survivor rejects with tail 2.
    let first = nodes[candidate]
        .fabric_stream_begin_epoch_transition("epoch-pull", 0, 2)
        .unwrap();
    assert_eq!(first.to_epoch, 2);
    assert_eq!(first.affirmative_votes, 1);
    nodes[ahead].process_network();
    nodes[candidate].process_network();

    // Candidate pulls the missing exact suffix under the stale term-2
    // proposal. Applying it changes candidate tail but does not install term 2.
    let pull = nodes[candidate]
        .fabric_stream_pull_epoch_transition("epoch-pull", 10)
        .unwrap();
    assert_eq!(pull.ahead_replicas, 1);
    assert_eq!(pull.requests_dispatched, 1);
    assert_eq!(pull.source_tail, 2);

    nodes[ahead].process_network();
    nodes[candidate].process_network();

    assert_eq!(
        nodes[candidate]
            .fabric_stream_info("epoch-pull")
            .unwrap()
            .last_sequence,
        Some(2)
    );
    assert_eq!(
        nodes[candidate].fabric_stream_epoch("epoch-pull").unwrap(),
        Some(1)
    );

    // Because the candidate tail changed, a higher term supersedes the stale
    // term-2 proposal. The survivor now matches the term-3 candidate tail and
    // can durably vote yes.
    let second = nodes[candidate]
        .fabric_stream_begin_epoch_transition("epoch-pull", 0, 2)
        .unwrap();
    assert_eq!(second.to_epoch, 3);
    assert_eq!(second.affirmative_votes, 1);

    nodes[ahead].process_network();
    nodes[candidate].process_network();
    nodes[ahead].process_network();

    for index in &survivors {
        assert_eq!(
            nodes[*index].fabric_stream_epoch("epoch-pull").unwrap(),
            Some(3)
        );
        assert_eq!(
            nodes[*index]
                .fabric_stream_committed_sequence("epoch-pull")
                .unwrap(),
            2
        );
        assert_eq!(
            nodes[*index]
                .fabric_stream_read_committed("epoch-pull", 1, 10)
                .unwrap()
                .len(),
            2
        );
    }

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn confirmed_goodbye_automatically_transitions_stream_leadership() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addrs: Vec<SocketAddr> = ["127.0.0.1:35101", "127.0.0.1:35102", "127.0.0.1:35103"]
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

    let initial = nodes[0]
        .fabric_stream_placement("auto-failover", 0, 3)
        .unwrap();
    let old_leader = ids.iter().position(|node| *node == initial.leader).unwrap();
    let survivors: Vec<usize> = (0..3).filter(|index| *index != old_leader).collect();

    let roots: Vec<PathBuf> = (0..3)
        .map(|index| temp_dir(&format!("auto-failover-{index}")))
        .collect();
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[old_leader]
        .fabric_stream_create("auto-failover", FabricStreamConfig::default())
        .unwrap();

    nodes[old_leader]
        .fabric_stream_replicated_append("auto-failover", 0, 3, b"stable-prefix")
        .unwrap();
    for index in &survivors {
        nodes[*index].process_network();
    }
    nodes[old_leader].process_network();
    nodes[old_leader].process_network();
    for index in &survivors {
        nodes[*index].process_network();
        nodes[*index].process_network();
    }

    for index in &survivors {
        assert_eq!(
            nodes[*index]
                .fabric_stream_committed_sequence("auto-failover")
                .unwrap(),
            1
        );
    }

    // Determine which survivor should lead the reduced RF=2 placement.
    let scratch_local = survivors[0];
    let mut scratch = Runtime::new();
    scratch.distributed.enabled = true;
    scratch.distributed.node_id = Some(ids[scratch_local]);
    let mut scratch_cluster = ClusterState::new(ids[scratch_local], addrs[scratch_local]);
    let scratch_peer = survivors[1];
    scratch_cluster.handle_heartbeat(ids[scratch_peer], addrs[scratch_peer]);
    scratch.distributed.cluster = Some(scratch_cluster);
    let reduced = scratch
        .fabric_stream_placement("auto-failover", 0, 2)
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

    // A positive goodbye is a confirmed-removal path. Queue the goodbye on
    // both survivors before either processes it so both compute the same
    // reduced membership before prepare/vote traffic runs.
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

    // Candidate processes goodbye and automatically starts the transition.
    nodes[candidate].process_network();
    assert_eq!(
        nodes[candidate]
            .fabric_stream_epoch("auto-failover")
            .unwrap(),
        Some(1)
    );

    // The other survivor processes its goodbye before the candidate's prepare,
    // then votes for the exact shared tail. Candidate finalizes and sends the
    // epoch commit back.
    nodes[voter].process_network();
    nodes[candidate].process_network();
    nodes[voter].process_network();

    for index in &survivors {
        assert_eq!(
            nodes[*index].fabric_stream_epoch("auto-failover").unwrap(),
            Some(2)
        );
        assert_eq!(
            nodes[*index]
                .fabric_stream_committed_sequence("auto-failover")
                .unwrap(),
            1
        );
    }

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn automatic_failover_retries_until_survivor_confirms_removal() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addrs: Vec<SocketAddr> = ["127.0.0.1:35201", "127.0.0.1:35202", "127.0.0.1:35203"]
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

    let initial = nodes[0]
        .fabric_stream_placement("auto-failover-retry", 0, 3)
        .unwrap();
    let old_leader = ids.iter().position(|node| *node == initial.leader).unwrap();
    let survivors: Vec<usize> = (0..3).filter(|index| *index != old_leader).collect();

    let roots: Vec<PathBuf> = (0..3)
        .map(|index| temp_dir(&format!("auto-failover-retry-{index}")))
        .collect();
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[old_leader]
        .fabric_stream_create("auto-failover-retry", FabricStreamConfig::default())
        .unwrap();
    nodes[old_leader]
        .fabric_stream_replicated_append("auto-failover-retry", 0, 3, b"stable")
        .unwrap();

    for index in &survivors {
        nodes[*index].process_network();
    }
    nodes[old_leader].process_network();
    nodes[old_leader].process_network();
    for index in &survivors {
        nodes[*index].process_network();
        nodes[*index].process_network();
    }

    let scratch_local = survivors[0];
    let mut scratch = Runtime::new();
    scratch.distributed.enabled = true;
    scratch.distributed.node_id = Some(ids[scratch_local]);
    let mut scratch_cluster = ClusterState::new(ids[scratch_local], addrs[scratch_local]);
    let scratch_peer = survivors[1];
    scratch_cluster.handle_heartbeat(ids[scratch_peer], addrs[scratch_peer]);
    scratch.distributed.cluster = Some(scratch_cluster);
    let reduced = scratch
        .fabric_stream_placement("auto-failover-retry", 0, 2)
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

    // Only the candidate confirms removal initially.
    nodes[old_leader]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .send(
            ids[candidate],
            addrs[candidate],
            Packet::NodeGoodbye {
                node_id: ids[old_leader],
                durable: Vec::new(),
            },
        );
    nodes[candidate].process_network();

    // The voter still sees the old 3-node membership, so the first prepare
    // cannot validate the proposed RF=2 placement.
    nodes[voter].process_network();
    assert_eq!(
        nodes[voter]
            .fabric_stream_epoch("auto-failover-retry")
            .unwrap(),
        Some(1)
    );

    // Now the voter independently confirms the removal.
    nodes[old_leader]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .send(
            ids[voter],
            addrs[voter],
            Packet::NodeGoodbye {
                node_id: ids[old_leader],
                durable: Vec::new(),
            },
        );
    nodes[voter].process_network();

    // No retry before the 500 ms logical-clock deadline.
    nodes[candidate].advance_time(Duration::from_millis(499));
    nodes[candidate].process_network();
    nodes[voter].process_network();
    assert_eq!(
        nodes[candidate]
            .fabric_stream_epoch("auto-failover-retry")
            .unwrap(),
        Some(1)
    );

    // At 500 ms the candidate re-sends the same durable proposal. The voter
    // now has matching reduced membership and can promise/vote.
    nodes[candidate].advance_time(Duration::from_millis(1));
    nodes[candidate].process_network();
    nodes[voter].process_network();
    nodes[candidate].process_network();
    nodes[voter].process_network();

    for index in &survivors {
        assert_eq!(
            nodes[*index]
                .fabric_stream_epoch("auto-failover-retry")
                .unwrap(),
            Some(2)
        );
    }

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn automatic_failover_pushes_lagging_survivor_before_transition() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addrs: Vec<SocketAddr> = ["127.0.0.1:35301", "127.0.0.1:35302", "127.0.0.1:35303"]
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

    let initial = nodes[0]
        .fabric_stream_placement("auto-failover-push", 0, 3)
        .unwrap();
    let old_leader = ids.iter().position(|node| *node == initial.leader).unwrap();
    let survivors: Vec<usize> = (0..3).filter(|index| *index != old_leader).collect();

    let scratch_local = survivors[0];
    let mut scratch = Runtime::new();
    scratch.distributed.enabled = true;
    scratch.distributed.node_id = Some(ids[scratch_local]);
    let mut scratch_cluster = ClusterState::new(ids[scratch_local], addrs[scratch_local]);
    let scratch_peer = survivors[1];
    scratch_cluster.handle_heartbeat(ids[scratch_peer], addrs[scratch_peer]);
    scratch.distributed.cluster = Some(scratch_cluster);
    let reduced = scratch
        .fabric_stream_placement("auto-failover-push", 0, 2)
        .unwrap();
    let candidate = survivors
        .iter()
        .copied()
        .find(|index| ids[*index] == reduced.leader)
        .unwrap();
    let lagging = survivors
        .iter()
        .copied()
        .find(|index| *index != candidate)
        .unwrap();

    let roots: Vec<PathBuf> = (0..3)
        .map(|index| temp_dir(&format!("auto-failover-push-{index}")))
        .collect();
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[old_leader]
        .fabric_stream_create("auto-failover-push", FabricStreamConfig::default())
        .unwrap();

    nodes[old_leader]
        .fabric_stream_replicated_append("auto-failover-push", 0, 3, b"common")
        .unwrap();
    for index in &survivors {
        nodes[*index].process_network();
    }
    nodes[old_leader].process_network();
    nodes[old_leader].process_network();
    for index in &survivors {
        nodes[*index].process_network();
        nodes[*index].process_network();
    }

    // Sequence 2 commits on old leader + future candidate while the other
    // survivor remains one record behind.
    nodes[old_leader]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([ids[lagging]]));
    nodes[old_leader]
        .fabric_stream_replicated_append("auto-failover-push", 0, 3, b"candidate-ahead")
        .unwrap();
    nodes[candidate].process_network();
    nodes[old_leader].process_network();
    nodes[candidate].process_network();
    nodes[old_leader]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());

    assert_eq!(
        nodes[candidate]
            .fabric_stream_info("auto-failover-push")
            .unwrap()
            .last_sequence,
        Some(2)
    );
    assert_eq!(
        nodes[lagging]
            .fabric_stream_info("auto-failover-push")
            .unwrap()
            .last_sequence,
        Some(1)
    );

    // Both survivors receive confirmed goodbye. Candidate starts failover;
    // lagging survivor rejects the first prepare with tail 1.
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
    nodes[candidate].process_network();
    nodes[lagging].process_network();
    nodes[candidate].process_network();

    assert_eq!(
        nodes[candidate]
            .fabric_stream_epoch("auto-failover-push")
            .unwrap(),
        Some(1)
    );

    // First failover retry automatically chooses push repair.
    nodes[candidate].advance_time(Duration::from_millis(500));
    nodes[candidate].process_network();
    nodes[lagging].process_network();
    nodes[candidate].process_network();
    nodes[lagging].process_network();

    for index in &survivors {
        assert_eq!(
            nodes[*index]
                .fabric_stream_epoch("auto-failover-push")
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            nodes[*index]
                .fabric_stream_committed_sequence("auto-failover-push")
                .unwrap(),
            2
        );
    }

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn automatic_failover_pulls_ahead_survivor_and_supersedes_term() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addrs: Vec<SocketAddr> = ["127.0.0.1:35401", "127.0.0.1:35402", "127.0.0.1:35403"]
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

    let initial = nodes[0]
        .fabric_stream_placement("auto-failover-pull", 0, 3)
        .unwrap();
    let old_leader = ids.iter().position(|node| *node == initial.leader).unwrap();
    let survivors: Vec<usize> = (0..3).filter(|index| *index != old_leader).collect();

    let scratch_local = survivors[0];
    let mut scratch = Runtime::new();
    scratch.distributed.enabled = true;
    scratch.distributed.node_id = Some(ids[scratch_local]);
    let mut scratch_cluster = ClusterState::new(ids[scratch_local], addrs[scratch_local]);
    let scratch_peer = survivors[1];
    scratch_cluster.handle_heartbeat(ids[scratch_peer], addrs[scratch_peer]);
    scratch.distributed.cluster = Some(scratch_cluster);
    let reduced = scratch
        .fabric_stream_placement("auto-failover-pull", 0, 2)
        .unwrap();
    let candidate = survivors
        .iter()
        .copied()
        .find(|index| ids[*index] == reduced.leader)
        .unwrap();
    let ahead = survivors
        .iter()
        .copied()
        .find(|index| *index != candidate)
        .unwrap();

    let roots: Vec<PathBuf> = (0..3)
        .map(|index| temp_dir(&format!("auto-failover-pull-{index}")))
        .collect();
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[old_leader]
        .fabric_stream_create("auto-failover-pull", FabricStreamConfig::default())
        .unwrap();

    nodes[old_leader]
        .fabric_stream_replicated_append("auto-failover-pull", 0, 3, b"common")
        .unwrap();
    for index in &survivors {
        nodes[*index].process_network();
    }
    nodes[old_leader].process_network();
    nodes[old_leader].process_network();
    for index in &survivors {
        nodes[*index].process_network();
        nodes[*index].process_network();
    }

    // Sequence 2 commits on old leader + non-candidate survivor. The future
    // deterministic RF=2 leader is behind by one record.
    nodes[old_leader]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([ids[candidate]]));
    nodes[old_leader]
        .fabric_stream_replicated_append("auto-failover-pull", 0, 3, b"survivor-ahead")
        .unwrap();
    nodes[ahead].process_network();
    nodes[old_leader].process_network();
    nodes[ahead].process_network();
    nodes[old_leader]
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());

    assert_eq!(
        nodes[candidate]
            .fabric_stream_info("auto-failover-pull")
            .unwrap()
            .last_sequence,
        Some(1)
    );
    assert_eq!(
        nodes[ahead]
            .fabric_stream_info("auto-failover-pull")
            .unwrap()
            .last_sequence,
        Some(2)
    );

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
    nodes[candidate].process_network();
    nodes[ahead].process_network();
    nodes[candidate].process_network();

    // First retry sees the rejected ahead vote and automatically pulls.
    nodes[candidate].advance_time(Duration::from_millis(500));
    nodes[candidate].process_network();
    nodes[ahead].process_network();
    nodes[candidate].process_network();

    assert_eq!(
        nodes[candidate]
            .fabric_stream_info("auto-failover-pull")
            .unwrap()
            .last_sequence,
        Some(2)
    );
    assert_eq!(
        nodes[candidate]
            .fabric_stream_epoch("auto-failover-pull")
            .unwrap(),
        Some(1)
    );

    // Pulling changed the proposal-bound candidate tail. The next scheduled
    // retry automatically starts the higher term and drives it to quorum.
    nodes[candidate].advance_time(Duration::from_secs(1));
    nodes[candidate].process_network();
    nodes[ahead].process_network();
    nodes[candidate].process_network();
    nodes[ahead].process_network();

    for index in &survivors {
        assert_eq!(
            nodes[*index]
                .fabric_stream_epoch("auto-failover-pull")
                .unwrap(),
            Some(3)
        );
        assert_eq!(
            nodes[*index]
                .fabric_stream_committed_sequence("auto-failover-pull")
                .unwrap(),
            2
        );
    }

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn installed_policy_ignores_unrelated_cluster_growth_during_quorum_commit() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:35501".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:35502".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut nodes = vec![runtime(addr_a, bus.clone()), runtime(addr_b, bus)];
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

    let initial = nodes[0]
        .fabric_stream_placement("stable-policy-growth", 0, 2)
        .unwrap();
    assert_eq!(
        initial,
        nodes[1]
            .fabric_stream_placement("stable-policy-growth", 0, 2)
            .unwrap()
    );
    let leader = if initial.leader == node_a { 0 } else { 1 };
    let follower = 1 - leader;

    let roots = vec![temp_dir("stable-policy-a"), temp_dir("stable-policy-b")];
    for (node, root) in nodes.iter_mut().zip(&roots) {
        node.fabric_stream_open(root).unwrap();
    }
    nodes[leader]
        .fabric_stream_create("stable-policy-growth", FabricStreamConfig::default())
        .unwrap();

    nodes[leader]
        .fabric_stream_replicated_append("stable-policy-growth", 0, 2, b"one")
        .unwrap();
    nodes[follower].process_network();
    nodes[leader].process_network();
    nodes[follower].process_network();
    assert_eq!(
        nodes[leader]
            .fabric_stream_committed_sequence("stable-policy-growth")
            .unwrap(),
        1
    );

    // Add healthy non-replica members until raw rendezvous would select a
    // different RF2 set. They deliberately have no transport endpoints.
    let mut dynamic_changed = false;
    for port in 35510..35600 {
        let address: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let node_id = NodeId::new(&address);
        if node_id == node_a || node_id == node_b {
            continue;
        }
        for node in &mut nodes {
            node.distributed
                .cluster
                .as_mut()
                .unwrap()
                .handle_heartbeat(node_id, address);
        }
        let dynamic = nodes[0]
            .fabric_stream_placement("stable-policy-growth", 0, 2)
            .unwrap();
        if dynamic.replicas != initial.replicas
            || dynamic.membership_fingerprint != initial.membership_fingerprint
        {
            dynamic_changed = true;
            break;
        }
    }
    assert!(
        dynamic_changed,
        "test must make raw rendezvous differ from installed stream policy"
    );

    let second = nodes[leader]
        .fabric_stream_replicated_append("stable-policy-growth", 0, 2, b"two")
        .unwrap();
    assert_eq!(second.dispatch.intended_remote, 1);
    assert_eq!(second.dispatch.dispatched, 1);
    assert!(!second.status.committed);

    nodes[follower].process_network();
    nodes[leader].process_network();
    nodes[follower].process_network();

    for node in &mut nodes {
        assert_eq!(
            node.fabric_stream_committed_sequence("stable-policy-growth")
                .unwrap(),
            2
        );
        assert_eq!(
            node.fabric_stream_read_committed("stable-policy-growth", 1, 10)
                .unwrap()
                .len(),
            2
        );
    }

    for root in roots {
        let _ = std::fs::remove_dir_all(root);
    }
}
