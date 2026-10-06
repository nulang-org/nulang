use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use nulang::runtime::{DeterministicNetworkTransport, IncomingPacket, NodeId, OutgoingPacket, Runtime};

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
