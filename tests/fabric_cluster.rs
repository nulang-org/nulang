use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use nulang::runtime::{
    Actor, ActorAdmissionStatus, DeterministicNetworkTransport, FabricAdvertisement,
    FabricAdvertisementSnapshot, Mailbox, NetworkTransport, NodeId, Packet, Runtime,
    TransportAdmission,
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
    // Drain B's heartbeat/gossip emitted during that turn while both nodes
    // still share the same virtual timestamp. No later packet from B should
    // refresh A's liveness clock during the simulated outage below.
    a.process_network();

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

#[test]
fn fabric_tracked_publish_resolves_remote_mailbox_admission() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:32111".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:32112".parse().unwrap();
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
    {
        let actor = b.actors.get_mut(&target).unwrap();
        actor.mailbox = Mailbox::new(1);
        actor.register_behavior("handle", noop);
    }
    b.fabric_subscribe("events.*", target, "handle").unwrap();

    b.advance_time(Duration::from_millis(600));
    a.advance_time(Duration::from_millis(600));
    b.process_network();
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 1);

    let first = a
        .fabric_publish_tracked("events.created", &[Value::int(1)])
        .unwrap();
    let second = a
        .fabric_publish_tracked("events.created", &[Value::int(2)])
        .unwrap();

    assert_eq!(first.immediate.selected, 1);
    assert_eq!(first.immediate.forwarded_remote, 1);
    assert_eq!(first.remote_deliveries.len(), 1);
    assert_eq!(second.immediate.selected, 1);
    assert_eq!(second.immediate.forwarded_remote, 1);
    assert_eq!(second.remote_deliveries.len(), 1);

    let accepted = first.remote_deliveries[0].delivery_id;
    let backpressured = second.remote_deliveries[0].delivery_id;

    b.process_network();
    a.process_network();

    assert_eq!(
        a.take_remote_admission(accepted),
        Some(ActorAdmissionStatus::Accepted)
    );
    assert_eq!(
        a.take_remote_admission(backpressured),
        Some(ActorAdmissionStatus::Backpressured)
    );
    assert_eq!(b.actors.get(&target).unwrap().mailbox.len(), 1);
    assert_eq!(b.dlq_depth(), 1);
}

#[test]
fn fabric_tracked_publish_reports_local_transport_backpressure_without_ticket() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:32121".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:32122".parse().unwrap();
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
    b.fabric_subscribe("events.*", target, "handle").unwrap();

    b.advance_time(Duration::from_millis(600));
    a.advance_time(Duration::from_millis(600));
    b.process_network();
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 1);

    // Saturate B's bounded deterministic incoming channel without allowing B
    // to drain it. The exact capacity remains an implementation detail.
    let mut saturated = false;
    for i in 0..4096_u64 {
        let admission = a
            .distributed
            .transport
            .as_mut()
            .unwrap()
            .try_send(
                node_b,
                addr_b,
                Packet::Heartbeat {
                    node_id: node_a,
                    timestamp: i,
                },
            );
        if admission == TransportAdmission::Backpressured {
            saturated = true;
            break;
        }
        assert_eq!(admission, TransportAdmission::Accepted);
    }
    assert!(saturated, "bounded transport channel should saturate");

    let report = a
        .fabric_publish_tracked("events.created", &[Value::int(1)])
        .unwrap();

    assert_eq!(report.immediate.selected, 1);
    assert_eq!(report.immediate.forwarded_remote, 0);
    assert_eq!(report.immediate.backpressured, 1);
    assert_eq!(report.immediate.rejected, 0);
    assert!(report.remote_deliveries.is_empty());
    assert_eq!(
        a.pending_remote_admission_count(),
        0,
        "transport refusal must not leak an outstanding remote ticket"
    );
}

#[test]
fn fabric_gossip_reordering_keeps_newest_snapshot_generation() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:32201".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:32202".parse().unwrap();
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

    let older = FabricAdvertisementSnapshot {
        node_id: node_b,
        generation: 1,
        subscriptions: vec![FabricAdvertisement {
            node_id: node_b,
            pattern: "events.old".into(),
            actor_id: target,
            behavior: "handle".into(),
            group: None,
        }],
    };
    let newer = FabricAdvertisementSnapshot {
        node_id: node_b,
        generation: 2,
        subscriptions: vec![FabricAdvertisement {
            node_id: node_b,
            pattern: "events.new".into(),
            actor_id: target,
            behavior: "handle".into(),
            group: None,
        }],
    };

    let transport = b.distributed.transport.as_mut().unwrap();
    transport.set_reorder(true);
    transport.send(
        node_a,
        addr_a,
        Packet::Gossip {
            members: vec![],
            directory: vec![],
            fabric: Some(older),
        },
    );
    transport.send(
        node_a,
        addr_a,
        Packet::Gossip {
            members: vec![],
            directory: vec![],
            fabric: Some(newer),
        },
    );
    transport.flush_held();

    // Bounded-adjacent reordering delivers generation 2 before generation 1.
    // The second arrival must therefore be ignored as stale.
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 1);
    assert_eq!(a.fabric_publish("events.old", &[]).unwrap(), 0);

    let report = a.fabric_publish_report("events.new", &[]).unwrap();
    assert_eq!(report.selected, 1);
    assert_eq!(report.forwarded_remote, 1);
}

#[test]
fn fabric_partition_removes_routes_and_heals_via_gossip() {
    let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let addr_a: SocketAddr = "127.0.0.1:32301".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:32302".parse().unwrap();
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
    b.fabric_subscribe("events.*", target, "handle").unwrap();

    // While B -> A is partitioned, B's automatic Fabric gossip cannot
    // populate A's routing directory.
    b.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([node_a]));
    b.advance_time(Duration::from_millis(600));
    a.advance_time(Duration::from_millis(600));
    b.process_network();
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 0);

    // Heal the link and verify the next gossip round converges the route.
    b.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());
    b.advance_time(Duration::from_millis(600));
    a.advance_time(Duration::from_millis(600));
    b.process_network();
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 1);

    // Drain any already-enqueued traffic, then isolate both directions so the
    // failure detector sees a clean outage window.
    let _ = a.distributed.transport.as_ref().unwrap().receive();
    let _ = b.distributed.transport.as_ref().unwrap().receive();
    a.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([node_b]));
    b.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::from([node_a]));

    a.advance_time(Duration::from_secs(8));
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 0);

    // Clear the partition. A's next failed-node probe reaches B; B processes
    // it and emits heartbeat/gossip back, restoring both membership and Fabric.
    a.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());
    b.distributed
        .transport
        .as_mut()
        .unwrap()
        .set_partition(HashSet::new());

    a.advance_time(Duration::from_secs(6));
    b.advance_time(Duration::from_secs(6));
    a.process_network();
    b.process_network();
    a.process_network();
    assert_eq!(a.fabric_remote_subscription_count(), 1);
}
