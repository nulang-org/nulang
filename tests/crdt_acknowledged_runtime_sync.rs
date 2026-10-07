use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use nulang::runtime::{
    CrdtManager, DeterministicNetworkTransport, IncomingPacket, NodeGossip, NodeId, NodeStatus,
    OutgoingPacket, Packet, Runtime,
};

fn distributed_runtime(
    addr: SocketAddr,
    bus: Arc<
        parking_lot::Mutex<
            HashMap<
                NodeId,
                (
                    std::sync::mpsc::SyncSender<IncomingPacket>,
                    std::sync::mpsc::SyncSender<OutgoingPacket>,
                ),
            >,
        >,
    >,
) -> Runtime {
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

#[test]
fn lost_delta_is_retried_from_last_acknowledged_peer_frontier() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:34201".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34202".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut a = distributed_runtime(addr_a, bus.clone());
    let mut b = distributed_runtime(addr_b, bus);

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

    // Consume round 1 while there is no CRDT state. Round 1 is intentionally
    // the periodic full-repair round; every synchronization exercised below is
    // an ordinary delta round.
    a.sync_crdts();

    let id = a.crdt_manager.as_mut().unwrap().create_gcounter().0;
    a.crdt_manager
        .as_mut()
        .unwrap()
        .get_gcounter_mut(id)
        .unwrap()
        .increment_by(1);

    // Initial peer state is delivered and explicitly ACKed.
    a.sync_crdts();
    b.process_network();
    a.process_network();
    assert_eq!(
        b.crdt_manager
            .as_mut()
            .unwrap()
            .get_gcounter_mut(id)
            .unwrap()
            .value(),
        1
    );

    // The sender changes to 2, reserves a transport sequence, and the packet
    // is dropped in flight. No receiver ACK can exist for this attempt.
    a.crdt_manager
        .as_mut()
        .unwrap()
        .get_gcounter_mut(id)
        .unwrap()
        .increment_by(1);
    a.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([node_b]));
    a.sync_crdts();
    b.process_network();

    assert_eq!(
        b.crdt_manager
            .as_mut()
            .unwrap()
            .get_gcounter_mut(id)
            .unwrap()
            .value(),
        1,
        "the partitioned delta must not appear at the receiver"
    );

    // Healing the link must retry from B's last acknowledged frontier on the
    // very next delta round. The legacy global sync_base loses this delta and
    // cannot repair it until a later full-state round.
    a.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());
    a.sync_crdts();
    b.process_network();

    assert_eq!(
        b.crdt_manager
            .as_mut()
            .unwrap()
            .get_gcounter_mut(id)
            .unwrap()
            .value(),
        2,
        "healed peer must receive the unacknowledged change before periodic full repair"
    );

    // Process the successful retry ACK, then prove the frontier advances: the
    // next mutation converges normally from value 2 rather than forcing a new
    // full-state join.
    a.process_network();
    a.crdt_manager
        .as_mut()
        .unwrap()
        .get_gcounter_mut(id)
        .unwrap()
        .increment_by(1);
    a.sync_crdts();
    b.process_network();
    assert_eq!(
        b.crdt_manager
            .as_mut()
            .unwrap()
            .get_gcounter_mut(id)
            .unwrap()
            .value(),
        3
    );
}

#[test]
fn failed_peer_recovery_forgets_stale_crdt_frontier_before_next_sync() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:34211".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34212".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut a = distributed_runtime(addr_a, bus.clone());
    let mut b = distributed_runtime(addr_b, bus);

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

    let id = a.crdt_manager.as_mut().unwrap().create_gcounter().0;
    a.crdt_manager
        .as_mut()
        .unwrap()
        .get_gcounter_mut(id)
        .unwrap()
        .increment_by(1);

    // Establish and ACK receiver knowledge at value 1 using only the
    // acknowledged delta-state path. Runtime::sync_crdts also runs op-based
    // replication, which could independently repair an unknown receiver and
    // would mask the stale-frontier failure this test is pinning.
    nulang::runtime::sync_crdts_delta(&mut a);
    b.process_network();
    a.process_network();
    assert_eq!(
        b.crdt_manager
            .as_mut()
            .unwrap()
            .get_gcounter_mut(id)
            .unwrap()
            .value(),
        1
    );

    // Model a same-NodeId process restart: the peer loses its local CRDT
    // state, while the sender still remembers the previously ACKed frontier.
    b.crdt_manager = Some(CrdtManager::new(node_b.0));

    // The sender's membership view had already declared the peer failed.
    // A subsequent heartbeat promotes it back to Healthy. That recovery is
    // the point where stale receiver knowledge must be discarded.
    a.distributed
        .cluster
        .as_mut()
        .unwrap()
        .merge_membership(vec![NodeGossip {
            node_id: node_b,
            address: addr_b,
            status: NodeStatus::Failed,
            incarnation: 100,
        }]);

    b.distributed.transport.as_mut().unwrap().send(
        node_a,
        addr_a,
        Packet::Heartbeat {
            node_id: node_b,
            timestamp: 1,
        },
    );
    a.process_network();

    assert_eq!(
        a.distributed
            .cluster
            .as_ref()
            .unwrap()
            .get_node(node_b)
            .unwrap()
            .status,
        NodeStatus::Healthy
    );

    a.crdt_manager
        .as_mut()
        .unwrap()
        .get_gcounter_mut(id)
        .unwrap()
        .increment_by(1);

    // If failed->healthy recovery does not reset B's frontier, A sends only a
    // delta from value 1. B has no base entry after its restart, so it correctly
    // ignores that delta. Resetting the frontier forces a full-state join.
    nulang::runtime::sync_crdts_delta(&mut a);
    b.process_network();

    assert_eq!(
        b.crdt_manager
            .as_mut()
            .unwrap()
            .get_gcounter_mut(id)
            .expect("recovered peer must receive a fresh full-state join")
            .value(),
        2
    );
}

#[test]
fn failed_peer_recovery_via_gossip_forgets_stale_crdt_frontier_before_next_sync() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:34221".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:34222".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut a = distributed_runtime(addr_a, bus.clone());
    let mut b = distributed_runtime(addr_b, bus);

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

    let id = a.crdt_manager.as_mut().unwrap().create_gcounter().0;
    a.crdt_manager
        .as_mut()
        .unwrap()
        .get_gcounter_mut(id)
        .unwrap()
        .increment_by(1);

    nulang::runtime::sync_crdts_delta(&mut a);
    b.process_network();
    a.process_network();
    assert_eq!(
        b.crdt_manager
            .as_mut()
            .unwrap()
            .get_gcounter_mut(id)
            .unwrap()
            .value(),
        1
    );

    // Same-NodeId restart: B loses local CRDT state while A still believes
    // the old process acknowledged value 1.
    b.crdt_manager = Some(CrdtManager::new(node_b.0));
    a.distributed
        .cluster
        .as_mut()
        .unwrap()
        .merge_membership(vec![NodeGossip {
            node_id: node_b,
            address: addr_b,
            status: NodeStatus::Failed,
            incarnation: 100,
        }]);

    // Recovery can arrive as the restarted peer's authoritative self-entry
    // in gossip before its next heartbeat. That transition must invalidate
    // receiver-specific CRDT knowledge just like Failed -> Healthy heartbeat
    // recovery does.
    b.distributed.transport.as_mut().unwrap().send(
        node_a,
        addr_a,
        Packet::Gossip {
            members: vec![NodeGossip {
                node_id: node_b,
                address: addr_b,
                status: NodeStatus::Healthy,
                incarnation: 101,
            }],
            directory: Vec::new(),
            fabric: None,
        },
    );
    a.process_network();

    assert_eq!(
        a.distributed
            .cluster
            .as_ref()
            .unwrap()
            .get_node(node_b)
            .unwrap()
            .status,
        NodeStatus::Healthy
    );

    a.crdt_manager
        .as_mut()
        .unwrap()
        .get_gcounter_mut(id)
        .unwrap()
        .increment_by(1);
    nulang::runtime::sync_crdts_delta(&mut a);
    b.process_network();

    assert_eq!(
        b.crdt_manager
            .as_mut()
            .unwrap()
            .get_gcounter_mut(id)
            .expect("gossip-recovered peer must receive a fresh full-state join")
            .value(),
        2
    );
}
