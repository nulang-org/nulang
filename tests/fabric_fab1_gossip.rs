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
fn oversized_fabric_snapshot_is_installed_only_after_complete_fab1_generation() {
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

    const SUBSCRIPTIONS: usize = 4100;
    for index in 0..SUBSCRIPTIONS {
        assert!(sender
            .fabric_subscribe(&format!("chunked.tenant-{index:04}.created"), target, "handle")
            .unwrap());
    }
    assert!(sender.fabric_advertisements(4096).is_err());

    // A single bounded chunk must never become a partial authoritative routing
    // generation on the receiver.
    gossip_round(&mut sender, &mut receiver);
    assert_eq!(receiver.fabric_remote_subscription_count(), 0);

    // Repeated gossip rounds rotate the generation's chunks. Only the final
    // verified chunk makes the complete 4,100-route snapshot visible.
    for _ in 0..64 {
        if receiver.fabric_remote_subscription_count() == SUBSCRIPTIONS {
            break;
        }
        gossip_round(&mut sender, &mut receiver);
    }
    assert_eq!(receiver.fabric_remote_subscription_count(), SUBSCRIPTIONS);

    let report = receiver
        .fabric_publish_report("chunked.tenant-4099.created", &[])
        .unwrap();
    assert_eq!(report.selected, 1);
    assert_eq!(report.forwarded_remote, 1);
}
