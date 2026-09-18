use std::collections::HashMap;
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
