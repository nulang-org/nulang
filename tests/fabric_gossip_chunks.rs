use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use nulang::runtime::{
    Actor, DeterministicNetworkTransport, FabricAdvertisement, FabricAdvertisementSnapshot, NodeId,
    Packet, Runtime,
};
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

fn drive_gossip_until(
    sender: &mut Runtime,
    receiver: &mut Runtime,
    expected_remote_subscriptions: usize,
) {
    for _ in 0..8 {
        if receiver.fabric_remote_subscription_count() == expected_remote_subscriptions {
            return;
        }
        sender.advance_time(Duration::from_millis(600));
        receiver.advance_time(Duration::from_millis(600));
        sender.process_network();
        receiver.process_network();
    }
}

fn advertisements(node_id: NodeId, count: usize) -> Vec<FabricAdvertisement> {
    (0..count)
        .map(|index| FabricAdvertisement {
            node_id,
            pattern: format!("wire.{index}"),
            actor_id: index as u64 + 1,
            behavior: "handle".to_string(),
            group: None,
        })
        .collect()
}

#[test]
fn automatic_gossip_converges_more_than_legacy_256_subscription_sender_cap() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:32401".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:32402".parse().unwrap();
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

    let target = b.spawn_actor(Box::new(Vec::new));
    b.actors
        .get_mut(&target)
        .unwrap()
        .register_behavior("handle", noop);

    const SUBSCRIPTIONS: usize = 300;
    for index in 0..SUBSCRIPTIONS {
        let pattern = format!("events.tenant-{index:04}.created");
        assert!(b.fabric_subscribe(&pattern, target, "handle").unwrap());
    }

    // This proves the historical sender cap itself is exceeded. The wire
    // decoder already accepts a larger complete FAB0 snapshot, so automatic
    // gossip should use that compatibility window rather than disappearing.
    assert!(b.fabric_advertisements(256).is_err());

    drive_gossip_until(&mut b, &mut a, SUBSCRIPTIONS);
    assert_eq!(a.fabric_remote_subscription_count(), SUBSCRIPTIONS);

    let report = a
        .fabric_publish_report("events.tenant-0299.created", &[])
        .unwrap();
    assert_eq!(report.selected, 1);
    assert_eq!(report.forwarded_remote, 1);
}

#[test]
fn complete_snapshot_export_still_fails_closed_above_fab0_decoder_bound() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr: SocketAddr = "127.0.0.1:32403".parse().unwrap();
    let mut runtime = distributed_runtime(addr, bus);
    let target = runtime.spawn_actor(Box::new(Vec::new));
    runtime
        .actors
        .get_mut(&target)
        .unwrap()
        .register_behavior("handle", noop);

    for index in 0..4097 {
        let pattern = format!("s.{index}");
        assert!(runtime
            .fabric_subscribe(&pattern, target, "handle")
            .unwrap());
    }

    // Until FAB1 is wired into Packet::Gossip, the compatibility bridge must
    // never manufacture a partial authoritative FAB0 replacement above the
    // decoder's existing complete-snapshot bound.
    let error = runtime.fabric_advertisements(4096).unwrap_err();
    assert!(error.contains("4097 local subscriptions"));
    assert!(error.contains("exceeding limit 4096"));
}

#[test]
fn fab0_wire_boundary_matches_sender_compatibility_window() {
    let node_id = NodeId(44);
    let max_snapshot = FabricAdvertisementSnapshot {
        node_id,
        generation: 7,
        subscriptions: advertisements(node_id, 4096),
    };
    let max_packet = Packet::Gossip {
        members: vec![],
        directory: vec![],
        fabric: Some(max_snapshot.clone()),
    };
    let bytes = max_packet.to_bytes(81);
    let (sequence, decoded) = Packet::from_bytes(&bytes).expect("4096-entry FAB0 should decode");
    assert_eq!(sequence, 81);
    assert_eq!(decoded, max_packet);

    let too_large = Packet::Gossip {
        members: vec![],
        directory: vec![],
        fabric: Some(FabricAdvertisementSnapshot {
            node_id,
            generation: 8,
            subscriptions: advertisements(node_id, 4097),
        }),
    };
    assert!(Packet::from_bytes(&too_large.to_bytes(82)).is_none());
}
