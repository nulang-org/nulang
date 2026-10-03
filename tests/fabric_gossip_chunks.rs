use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use nulang::runtime::{
    Actor, DeterministicNetworkTransport, FabricMetadataChunk, FabricMetadataKind, NodeId, Packet,
    Runtime,
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

#[test]
fn gossip_packet_roundtrip_preserves_fab1_chunk_without_requiring_fab0() {
    let chunk = FabricMetadataChunk {
        owner: 42,
        kind: FabricMetadataKind::Subscriptions,
        generation: 9,
        snapshot_hash: [7; 32],
        chunk_index: 1,
        chunk_count: 3,
        payload: b"opaque-subscription-snapshot-bytes".to_vec(),
    };
    let packet = Packet::Gossip {
        members: vec![],
        directory: vec![],
        fabric: None,
        fabric_chunk: Some(chunk.clone()),
    };

    let bytes = packet.to_bytes(77);
    let (seq, decoded) = Packet::from_bytes(&bytes).expect("FAB1 gossip packet should decode");
    assert_eq!(seq, 77);
    assert_eq!(decoded, packet);

    match decoded {
        Packet::Gossip {
            fabric,
            fabric_chunk,
            ..
        } => {
            assert!(fabric.is_none());
            assert_eq!(fabric_chunk, Some(chunk));
        }
        other => panic!("expected Gossip packet, got {other:?}"),
    }
}

#[test]
fn oversized_subscription_directory_converges_atomically_over_multiple_gossip_rounds() {
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

    // Exceed the legacy 256-entry FAB0 all-or-nothing snapshot ceiling and
    // use long subjects so the authoritative generation spans several FAB1
    // chunks rather than fitting in one packet.
    const SUBSCRIPTIONS: usize = 300;
    for index in 0..SUBSCRIPTIONS {
        let pattern = format!(
            "events.tenant-{index:04}.region-us-east-1.resource-with-a-long-stable-name.created"
        );
        assert!(b.fabric_subscribe(&pattern, target, "handle").unwrap());
    }
    assert!(b.fabric_advertisements(256).is_err());

    // One chunk is not an authoritative generation. The receiver must not
    // partially install a prefix and thereby expose a routing set that never
    // existed on the sender.
    b.advance_time(Duration::from_millis(600));
    a.advance_time(Duration::from_millis(600));
    b.process_network();
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 0);

    // Repeated lossy-gossip-style rounds rotate through every chunk. Once the
    // final chunk verifies the complete snapshot hash, all 300 routes become
    // visible in one generation replacement.
    for _ in 0..16 {
        if a.fabric_remote_subscription_count() == SUBSCRIPTIONS {
            break;
        }
        b.advance_time(Duration::from_millis(600));
        a.advance_time(Duration::from_millis(600));
        b.process_network();
        a.process_network();
    }
    assert_eq!(a.fabric_remote_subscription_count(), SUBSCRIPTIONS);

    let report = a
        .fabric_publish_report(
            "events.tenant-0299.region-us-east-1.resource-with-a-long-stable-name.created",
            &[],
        )
        .unwrap();
    assert_eq!(report.selected, 1);
    assert_eq!(report.forwarded_remote, 1);
}
