use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

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
fn fabric_gossip_converges_routes_and_reuses_distributed_actor_transport() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:32101".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:32102".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut a = distributed_runtime(addr_a, bus.clone());
    let mut b = distributed_runtime(addr_b, bus);

    // Seed both real ClusterState instances as healthy peers. Subsequent
    // subscription propagation is automatic through Runtime::process_network.
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

    let target = b.spawn_actor(Box::new(Vec::new));
    b.actors
        .get_mut(&target)
        .unwrap()
        .register_behavior("handle", noop);
    b.fabric_subscribe("events.*", target, "handle").unwrap();

    // B's cluster tick gossips its complete Fabric snapshot; A's next network
    // turn decodes the additive FAB0 tail and installs the remote route.
    b.advance_time(Duration::from_millis(600));
    a.advance_time(Duration::from_millis(600));
    b.process_network();
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 1);

    assert_eq!(
        a.fabric_publish("events.created", &[Value::int(42)])
            .unwrap(),
        1
    );
    b.process_network();
    assert_eq!(b.actors.get(&target).unwrap().mailbox.len(), 1);

    // Failure detection removes the learned route immediately rather than
    // leaving a dead consumer selectable until the 60-second removal window.
    a.advance_time(Duration::from_secs(8));
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 0);

    // A healthy peer with the same stable NodeId can advertise the same local
    // generation after recovery because failure cleanup forgot the old remote
    // generation together with the dead routes.
    a.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_b, addr_b);
    b.advance_time(Duration::from_millis(600));
    a.advance_time(Duration::from_millis(600));
    b.process_network();
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 1);
}
