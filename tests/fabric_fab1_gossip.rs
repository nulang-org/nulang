use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use nulang::runtime::{
    Actor, DeterministicNetworkTransport, FabricAdvertisement, FabricAdvertisementSnapshot, NodeId,
    Packet, Runtime,
};
use nulang::vm::Value;

const FAB1_TUNNEL_PATTERN: &str = "__nulang_fab1..metadata";
const FAB1_TUNNEL_PREFIX: &str = "FAB1HEX:";

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

fn gossip_round(sender: &mut Runtime, receiver: &mut Runtime) {
    sender.advance_time(Duration::from_millis(600));
    receiver.advance_time(Duration::from_millis(600));
    sender.process_network();
    receiver.process_network();
}

fn drive_until_remote_count(sender: &mut Runtime, receiver: &mut Runtime, expected: usize) {
    for _ in 0..64 {
        if receiver.fabric_remote_subscription_count() == expected {
            return;
        }
        gossip_round(sender, receiver);
    }
}

#[test]
fn gossip_packet_roundtrip_preserves_reserved_fab1_tunnel_advertisement() {
    let node_id = NodeId(42);
    let tunnel = FabricAdvertisementSnapshot {
        node_id,
        generation: 9,
        subscriptions: vec![FabricAdvertisement {
            node_id,
            // Deliberately invalid as a user subscription: an old peer rejects
            // this complete snapshot instead of installing a bogus route.
            pattern: FAB1_TUNNEL_PATTERN.to_string(),
            actor_id: 1,
            behavior: format!("{FAB1_TUNNEL_PREFIX}46414231"),
            group: None,
        }],
    };
    let packet = Packet::Gossip {
        members: vec![],
        directory: vec![],
        fabric: Some(tunnel.clone()),
    };

    let bytes = packet.to_bytes(77);
    let (sequence, decoded) = Packet::from_bytes(&bytes).expect("FAB1 tunnel Gossip should decode");
    assert_eq!(sequence, 77);
    assert_eq!(decoded, packet);

    let mut runtime = Runtime::new();
    let target = runtime.spawn_actor(Box::new(Vec::new));
    runtime
        .actors
        .get_mut(&target)
        .unwrap()
        .register_behavior("handle", noop);
    assert!(runtime
        .fabric_subscribe(FAB1_TUNNEL_PATTERN, target, "handle")
        .is_err());
}

#[test]
fn crossing_fab0_limit_preserves_old_routes_until_complete_fab1_generation() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:32501".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:32502".parse().unwrap();
    let node_a = NodeId::new(&addr_a);
    let node_b = NodeId::new(&addr_b);

    let mut receiver = distributed_runtime(addr_a, bus.clone());
    let mut sender = distributed_runtime(addr_b, bus);
    receiver
        .distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_b, addr_b);
    sender
        .distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(node_a, addr_a);

    let target = sender.spawn_actor(Box::new(Vec::new));
    sender
        .actors
        .get_mut(&target)
        .unwrap()
        .register_behavior("handle", noop);

    const FAB0_LIMIT: usize = 4096;
    const FINAL_SUBSCRIPTIONS: usize = 4100;

    // First converge the largest generation representable by the legacy
    // complete FAB0 snapshot path.
    for index in 0..FAB0_LIMIT {
        assert!(sender
            .fabric_subscribe(&format!("chunked.tenant-{index:04}.created"), target, "handle")
            .unwrap());
    }
    drive_until_remote_count(&mut sender, &mut receiver, FAB0_LIMIT);
    assert_eq!(receiver.fabric_remote_subscription_count(), FAB0_LIMIT);

    // Cross the compatibility boundary. This new generation must use FAB1.
    for index in FAB0_LIMIT..FINAL_SUBSCRIPTIONS {
        assert!(sender
            .fabric_subscribe(&format!("chunked.tenant-{index:04}.created"), target, "handle")
            .unwrap());
    }
    assert!(sender.fabric_advertisements(FAB0_LIMIT).is_err());

    // A single bounded FAB1 chunk must not delete the previously committed
    // 4,096-route generation or expose any prefix of the 4,100-route one.
    gossip_round(&mut sender, &mut receiver);
    assert_eq!(receiver.fabric_remote_subscription_count(), FAB0_LIMIT);
    let old_report = receiver
        .fabric_publish_report("chunked.tenant-4095.created", &[])
        .unwrap();
    assert_eq!(old_report.selected, 1);
    assert_eq!(old_report.forwarded_remote, 1);

    // Repeated rounds rotate every chunk. The routing table changes only once
    // the complete 4,100-route generation verifies.
    drive_until_remote_count(&mut sender, &mut receiver, FINAL_SUBSCRIPTIONS);
    assert_eq!(
        receiver.fabric_remote_subscription_count(),
        FINAL_SUBSCRIPTIONS
    );

    let new_report = receiver
        .fabric_publish_report("chunked.tenant-4099.created", &[])
        .unwrap();
    assert_eq!(new_report.selected, 1);
    assert_eq!(new_report.forwarded_remote, 1);
}
