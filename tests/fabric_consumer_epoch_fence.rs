//! Replicated consumer effects must be associated with the installed leader epoch.
//! These tests do not claim consumer ACK state is replicated across leader changes.

use std::collections::{HashMap, HashSet};
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
    let transport = DeterministicNetworkTransport::bind_with_bus(addr, bus).unwrap();
    transport.register_on_bus();
    runtime
        .enable_distribution_with_transport(Box::new(transport))
        .unwrap();
    runtime
}

fn temp_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nulang-fabric-consumer-epoch-fence-{label}-{}-{id}",
        std::process::id()
    ))
}

#[test]
fn replicated_consumer_requires_leader_and_matching_delivery_epoch() {
    let bus: Bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:39131".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:39132".parse().unwrap();
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

    let placement = a.fabric_stream_placement("fenced", 0, 2).unwrap();
    let root_a = temp_dir("leader-a");
    let root_b = temp_dir("leader-b");
    a.fabric_stream_open(&root_a).unwrap();
    b.fabric_stream_open(&root_b).unwrap();
    let (leader, follower) = if placement.leader == id_a {
        (&mut a, &mut b)
    } else {
        (&mut b, &mut a)
    };

    leader
        .fabric_stream_create("fenced", FabricStreamConfig::default())
        .unwrap();
    let appended = leader
        .fabric_stream_replicated_append("fenced", 0, 2, b"effect")
        .unwrap();
    assert!(!appended.status.committed);
    assert!(leader
        .fabric_stream_commit_cursor_fenced("fenced", "worker", 1, 1)
        .is_err());
    follower.process_network();
    leader.process_network();
    follower.process_network();
    assert_eq!(leader.fabric_stream_committed_sequence("fenced").unwrap(), 1);
    assert_eq!(follower.fabric_stream_committed_sequence("fenced").unwrap(), 1);

    // Isolate the committed follower from the leader. A replica with a valid
    // durable log must not start serving application consumer side effects.
    follower
        .distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([placement.leader]));

    // The follower stores the same committed record but cannot act as leader.
    assert!(follower
        .fabric_stream_deliver_consumer("fenced", "worker", 10, Duration::from_secs(30))
        .is_err());
    assert!(follower.fabric_stream_read_consumer("fenced", "worker", 10).is_err());
    assert!(follower.fabric_stream_commit_cursor("fenced", "worker", 1).is_err());
    assert!(follower
        .fabric_stream_ack_consumer_fenced("fenced", "worker", 1, 1)
        .is_err());

    let delivery = leader
        .fabric_stream_deliver_consumer("fenced", "worker", 10, Duration::from_secs(30))
        .unwrap();
    assert_eq!(delivery.len(), 1);
    assert_eq!(delivery[0].leader_epoch, Some(1));
    assert_eq!(delivery[0].record.sequence, 1);

    // Old APIs cannot bypass the epoch token on a replicated stream.
    assert!(leader.fabric_stream_ack_consumer("fenced", "worker", 1).is_err());
    assert!(leader.fabric_stream_nack_consumer("fenced", "worker", 1).is_err());
    assert!(leader.fabric_stream_commit_cursor("fenced", "worker", 1).is_err());
    assert_eq!(leader.fabric_stream_cursor("fenced", "worker").unwrap(), 0);

    assert!(leader
        .fabric_stream_ack_consumer_fenced("fenced", "worker", 1, 0)
        .is_err());
    assert!(leader
        .fabric_stream_ack_consumer_fenced("fenced", "worker", 1, 2)
        .is_err());
    assert!(leader
        .fabric_stream_nack_consumer_fenced("fenced", "worker", 1, 2)
        .is_err());
    assert_eq!(leader.fabric_stream_cursor("fenced", "worker").unwrap(), 0);

    leader
        .fabric_stream_ack_consumer_fenced(
            "fenced",
            "worker",
            delivery[0].record.sequence,
            delivery[0].leader_epoch.unwrap(),
        )
        .unwrap();
    assert_eq!(leader.fabric_stream_cursor("fenced", "worker").unwrap(), 1);

    let _ = std::fs::remove_dir_all(root_a);
    let _ = std::fs::remove_dir_all(root_b);
}

#[test]
fn unreplicated_runtime_consumer_preserves_unfenced_ack() {
    let root = temp_dir("standalone");
    let mut runtime = Runtime::new();
    runtime.fabric_stream_open(&root).unwrap();
    runtime
        .fabric_stream_create("local", FabricStreamConfig::default())
        .unwrap();
    runtime.fabric_stream_append("local", b"standalone").unwrap();
    let delivery = runtime
        .fabric_stream_deliver_consumer("local", "worker", 10, Duration::from_secs(30))
        .unwrap();
    assert_eq!(delivery.len(), 1);
    assert_eq!(delivery[0].leader_epoch, None);
    runtime.fabric_stream_ack_consumer("local", "worker", 1).unwrap();
    assert_eq!(runtime.fabric_stream_cursor("local", "worker").unwrap(), 1);
    let _ = std::fs::remove_dir_all(root);
}
