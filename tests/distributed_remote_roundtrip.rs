#![cfg(feature = "tcp")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::thread;
use std::time::{Duration, Instant};

use nulang::runtime::{Actor, ActorAddress, NodeId, Runtime, TlsConfig};
use nulang::vm::Value;

const HOP_TIMEOUT: Duration = Duration::from_secs(2);

fn record_sequence(actor: &mut Actor, args: &[Value]) {
    if let Some(sequence) = args.first().and_then(|value| value.as_int()) {
        actor.set_state_field("seen", Value::int(sequence));
    }
}

fn distributed_runtime() -> Runtime {
    let mut runtime = Runtime::new();
    runtime
        .enable_distribution(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            TlsConfig::PlaintextInsecure,
        )
        .expect("loopback distribution should bind");
    runtime
}

fn endpoint(runtime: &Runtime) -> (NodeId, SocketAddr) {
    let node = runtime
        .distributed
        .node_id
        .expect("distribution should assign a node id");
    let addr = runtime
        .distributed
        .transport
        .as_ref()
        .expect("distribution should own a transport")
        .listen_addr();
    (node, addr)
}

fn make_peer_healthy(runtime: &mut Runtime, peer: NodeId, addr: SocketAddr) {
    let cluster = runtime
        .distributed
        .cluster
        .as_mut()
        .expect("distribution should own cluster state");
    cluster.join_cluster_with_id(peer, addr);
    cluster.handle_heartbeat(peer, addr);
}

fn spawn_probe(runtime: &mut Runtime) -> u64 {
    let actor_id = runtime.spawn_actor(Box::new(|| vec![("seen".to_string(), Value::int(0))]));
    runtime
        .actors
        .get_mut(&actor_id)
        .expect("spawned probe actor should exist")
        .register_behavior("record", record_sequence);
    runtime.run_scheduler();
    actor_id
}

fn drive_until(runtime: &mut Runtime, actor_id: u64, sequence: i64) {
    let deadline = Instant::now() + HOP_TIMEOUT;
    loop {
        runtime.process_network();
        runtime.run_scheduler();
        let seen = runtime
            .actors
            .get(&actor_id)
            .and_then(|actor| actor.get_state_field("seen"))
            .and_then(|value| value.as_int());
        if seen == Some(sequence) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for remote actor to observe sequence {sequence}; last={seen:?}"
        );
        thread::yield_now();
    }
}

#[test]
fn tcp_loopback_remote_actor_roundtrip_executes_handlers_in_both_directions() {
    let mut left = distributed_runtime();
    let mut right = distributed_runtime();
    let (left_node, left_addr) = endpoint(&left);
    let (right_node, right_addr) = endpoint(&right);

    make_peer_healthy(&mut left, right_node, right_addr);
    make_peer_healthy(&mut right, left_node, left_addr);

    let left_actor = spawn_probe(&mut left);
    let right_actor = spawn_probe(&mut right);

    left.send_distributed(
        ActorAddress::remote(right_node, right_actor),
        "record",
        &[Value::int(41)],
    );
    drive_until(&mut right, right_actor, 41);

    right.send_distributed(
        ActorAddress::remote(left_node, left_actor),
        "record",
        &[Value::int(42)],
    );
    drive_until(&mut left, left_actor, 42);

    assert_eq!(
        left.actors
            .get(&left_actor)
            .and_then(|actor| actor.get_state_field("seen"))
            .and_then(|value| value.as_int()),
        Some(42)
    );
    assert_eq!(
        right
            .actors
            .get(&right_actor)
            .and_then(|actor| actor.get_state_field("seen"))
            .and_then(|value| value.as_int()),
        Some(41)
    );
}
