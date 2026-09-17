use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use nulang::runtime::{Actor, DeterministicNetworkTransport, NodeId, Runtime};
use nulang::vm::Value;

fn noop(_actor: &mut Actor, _args: &[Value]) {}

fn distributed_runtime(
    addr: SocketAddr,
    bus: Arc<
        parking_lot::Mutex<
            HashMap<
                NodeId,
                (
                    std::sync::mpsc::SyncSender<nulang::runtime::IncomingPacket>,
                    std::sync::mpsc::SyncSender<nulang::runtime::OutgoingPacket>,
                ),
            >,
        >,
    >,
) -> Runtime {
    let mut runtime = Runtime::new();
    let transport =
        DeterministicNetworkTransport::bind_with_bus(addr, bus).expect("transport should bind");
    transport.register_on_bus();
    runtime
        .enable_distribution_with_transport(Box::new(transport))
        .expect("distribution should enable");
    runtime
}

#[test]
fn fabric_remote_publish_reuses_distributed_actor_transport() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:32101".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:32102".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut a = distributed_runtime(addr_a, bus.clone());
    let mut b = distributed_runtime(addr_b, bus);

    // Install each peer in the real ClusterState. Two heartbeats also cover
    // implementations that probation a newly discovered peer before marking
    // it healthy.
    for _ in 0..2 {
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
    }
    assert!(a.distributed.cluster.as_ref().unwrap().get_node(node_b).is_some());

    let target = b.spawn_actor(Box::new(Vec::new));
    b.actors
        .get_mut(&target)
        .unwrap()
        .register_behavior("handle", noop);
    b.fabric_subscribe("events.*", target, "handle").unwrap();

    // This manually performs the control-plane exchange that the next Fabric
    // slice will piggyback on cluster gossip. The data plane is already the
    // production distributed actor path.
    let advertisements = b.fabric_advertisements(16);
    assert_eq!(advertisements.len(), 1);
    a.fabric_replace_remote_advertisements(node_b, advertisements)
        .unwrap();

    assert_eq!(
        a.fabric_publish("events.created", &[Value::int(42)])
            .unwrap(),
        1
    );

    // DeterministicNetworkTransport delivers synchronously to B's incoming
    // queue. Processing the real network loop must turn the ActorMessage into
    // an ordinary mailbox delivery on the subscribed actor.
    b.process_network();
    assert_eq!(b.actors.get(&target).unwrap().mailbox.len(), 1);
}
