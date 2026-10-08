//! Regressions for the durable leased-consumer/quorum boundary.
//! This file is stacked on the Fabric consumer-ACK implementation (PR #1070).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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

fn node(addr: SocketAddr, bus: Bus) -> Runtime {
    let mut runtime = Runtime::new();
    runtime.install_virtual_clock();
    let transport = DeterministicNetworkTransport::bind_with_bus(addr, bus)
        .expect("deterministic transport must bind");
    transport.register_on_bus();
    runtime
        .enable_distribution_with_transport(Box::new(transport))
        .expect("distribution must enable");
    runtime
}

fn temp_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nulang-fabric-leased-quorum-{label}-{}-{id}",
        std::process::id()
    ))
}

#[test]
fn leased_delivery_and_ack_remain_blocked_until_application_quorum() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:39111".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:39112".parse().unwrap();
    let id_a = NodeId::new(&addr_a);
    let id_b = NodeId::new(&addr_b);

    let mut a = node(addr_a, bus.clone());
    let mut b = node(addr_b, bus);
    a.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(id_b, addr_b);
    b.distributed
        .cluster
        .as_mut()
        .unwrap()
        .handle_heartbeat(id_a, addr_a);

    let placement = a.fabric_stream_placement("leases", 0, 2).unwrap();
    let root_a = temp_dir("a");
    let root_b = temp_dir("b");
    a.fabric_stream_open(&root_a).unwrap();
    b.fabric_stream_open(&root_b).unwrap();
    let (leader, follower, leader_dir) = if placement.leader == id_a {
        (&mut a, &mut b, &root_a)
    } else {
        (&mut b, &mut a, &root_b)
    };
    leader
        .fabric_stream_create("leases", FabricStreamConfig::default())
        .unwrap();

    let append = leader
        .fabric_stream_replicated_append("leases", 0, 2, b"do-not-run")
        .unwrap();
    assert!(!append.status.committed);
    assert_eq!(
        leader.fabric_stream_read("leases", 1, 10).unwrap().len(),
        1
    );
    assert!(leader
        .fabric_stream_read_committed("leases", 1, 10)
        .unwrap()
        .is_empty());

    // Neither legacy nor leased consumers may observe/ACK a local-only record.
    assert!(leader
        .fabric_stream_read_consumer("leases", "worker", 10)
        .unwrap()
        .is_empty());
    assert!(leader
        .fabric_stream_deliver_consumer("leases", "worker", 10, Duration::from_secs(30))
        .unwrap().is_empty());
    assert!(leader.fabric_stream_ack_consumer("leases", "worker", 1).is_err());
    assert!(leader.fabric_stream_commit_cursor("leases", "worker", 1).is_err());
    assert_eq!(leader.fabric_stream_cursor("leases", "worker").unwrap(), 0);

    // Persisted raw tail exists on reopen, but quorum is still absent.
    let mut reopened = Runtime::new();
    reopened.fabric_stream_open(leader_dir).unwrap();
    assert_eq!(
        reopened.fabric_stream_read("leases", 1, 10).unwrap().len(),
        1
    );
    assert!(reopened
        .fabric_stream_deliver_consumer("leases", "worker", 10, Duration::from_secs(30))
        .unwrap().is_empty());
    assert!(reopened.fabric_stream_ack_consumer("leases", "worker", 1).is_err());

    follower.process_network();
    leader.process_network();

    assert_eq!(leader.fabric_stream_committed_sequence("leases").unwrap(), 1);
    let delivery = leader
        .fabric_stream_deliver_consumer("leases", "worker", 10, Duration::from_secs(30))
        .unwrap();
    assert_eq!(delivery.len(), 1);
    assert_eq!(delivery[0].record.sequence, 1);
    assert_eq!(delivery[0].record.payload, b"do-not-run");
    leader.fabric_stream_ack_consumer("leases", "worker", 1).unwrap();
    assert_eq!(leader.fabric_stream_cursor("leases", "worker").unwrap(), 1);
    assert!(leader
        .fabric_stream_deliver_consumer("leases", "worker", 10, Duration::from_secs(30))
        .unwrap().is_empty());

    drop(reopened);
    let _ = std::fs::remove_dir_all(root_a);
    let _ = std::fs::remove_dir_all(root_b);
}
