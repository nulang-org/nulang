//! Distribution microbenchmarks.
//!
//! Names intentionally describe the operation being measured. Local CRDT delta
//! computation and membership merge are not network synchronization or full
//! cluster convergence; NUL0 packet codec benchmarks cover the wire-format
//! work separately. The loopback round-trip benchmark is the end-to-end
//! steady-state control: it includes TCP, NUL0 framing/codec work, routing,
//! mailbox admission, scheduler dispatch, native handler execution, and the
//! return hop, while excluding connection establishment and cluster bootstrap.

use criterion::{black_box, criterion_group, BenchmarkId, Criterion, Throughput};
use nulang::runtime::{
    ClusterState, GCounter, MessagePriority, NodeGossip, NodeId, NodeStatus, Packet,
};
use nulang::vm::Value;

#[cfg(feature = "tcp")]
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
#[cfg(feature = "tcp")]
use std::thread;
#[cfg(feature = "tcp")]
use std::time::{Duration, Instant};
#[cfg(feature = "tcp")]
use nulang::runtime::{Actor, ActorAddress, Runtime, TlsConfig};

fn bench_crdt_delta_compute(c: &mut Criterion) {
    c.bench_function("dist/crdt_delta_compute", |b| {
        b.iter(|| {
            let mut counter = GCounter::new(1);
            counter.increment();
            counter.increment();
            let base = GCounter::new(2);
            let delta = counter.delta_since(&base);
            black_box(delta);
        })
    });
}

fn bench_gossip_membership_merge(c: &mut Criterion) {
    c.bench_function("dist/gossip_membership_merge_4", |b| {
        b.iter(|| {
            use std::net::SocketAddr;

            let addr: SocketAddr = "127.0.0.1:9001".parse().unwrap();
            let node_id = NodeId::new(&addr);
            let mut cluster = ClusterState::new(node_id, addr);
            let gossip: Vec<NodeGossip> = (2..=5)
                .map(|n| {
                    let address: SocketAddr = format!("127.0.0.1:900{n}").parse().unwrap();
                    NodeGossip {
                        node_id: NodeId::new(&address),
                        address,
                        status: NodeStatus::Healthy,
                        incarnation: 1,
                    }
                })
                .collect();
            cluster.merge_membership(gossip);
            black_box(cluster);
        })
    });
}

fn actor_message_packet(payload_values: usize) -> Packet {
    Packet::ActorMessage {
        target_actor: 7,
        behavior_name: "handle".to_string(),
        content_hash: None,
        required_protocol_id: None,
        payload: vec![Value::int(42); payload_values],
        string_table: Vec::new(),
        object_table: Vec::new(),
        sender_actor: 3,
        sender_node: NodeId(11),
        priority: MessagePriority::Normal,
        trace_id: None,
    }
}

/// Hand-rolled NUL0 ActorMessage encoding cost.
///
/// This measures deterministic packet serialization only, not sockets, TLS,
/// queueing, routing, or remote mailbox admission.
fn bench_actor_message_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("dist/nul0_actor_message_encode");

    for payload_values in [1usize, 16, 128] {
        let packet = actor_message_packet(payload_values);
        group.throughput(Throughput::Elements(payload_values as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(payload_values),
            &payload_values,
            |b, _| {
                b.iter(|| {
                    let bytes = black_box(&packet).to_bytes(black_box(1));
                    black_box(bytes);
                })
            },
        );
    }

    group.finish();
}

/// Hand-rolled NUL0 ActorMessage decoding cost over bytes produced by the real
/// encoder.
fn bench_actor_message_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("dist/nul0_actor_message_decode");

    for payload_values in [1usize, 16, 128] {
        let bytes = actor_message_packet(payload_values).to_bytes(1);
        group.throughput(Throughput::Elements(payload_values as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(payload_values),
            &payload_values,
            |b, _| {
                b.iter(|| {
                    let decoded = Packet::from_bytes(black_box(bytes.as_slice()))
                        .expect("benchmark packet must decode");
                    black_box(decoded);
                })
            },
        );
    }

    group.finish();
}

#[cfg(feature = "tcp")]
const ROUNDTRIP_HOP_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(feature = "tcp")]
fn record_roundtrip_sequence(actor: &mut Actor, args: &[Value]) {
    if let Some(sequence) = args.first().and_then(|value| value.as_int()) {
        actor.set_state_field("seen", Value::int(sequence));
    }
}

#[cfg(feature = "tcp")]
fn bind_roundtrip_runtime() -> Runtime {
    let mut runtime = Runtime::new();
    runtime
        .enable_distribution(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            TlsConfig::PlaintextInsecure,
        )
        .expect("benchmark loopback transport should bind");
    runtime
}

#[cfg(feature = "tcp")]
fn roundtrip_endpoint(runtime: &Runtime) -> (NodeId, SocketAddr) {
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

#[cfg(feature = "tcp")]
fn mark_roundtrip_peer_healthy(runtime: &mut Runtime, peer: NodeId, addr: SocketAddr) {
    let cluster = runtime
        .distributed
        .cluster
        .as_mut()
        .expect("distribution should own cluster state");
    cluster.join_cluster_with_id(peer, addr);
    cluster.handle_heartbeat(peer, addr);
}

#[cfg(feature = "tcp")]
fn spawn_roundtrip_probe(runtime: &mut Runtime) -> u64 {
    let actor_id = runtime.spawn_actor(Box::new(|| {
        vec![("seen".to_string(), Value::int(0))]
    }));
    runtime
        .actors
        .get_mut(&actor_id)
        .expect("spawned benchmark actor should exist")
        .register_behavior("record", record_roundtrip_sequence);
    // Remove spawn-time ready state before any measured send.
    runtime.run_scheduler();
    actor_id
}

#[cfg(feature = "tcp")]
fn drive_roundtrip_hop(runtime: &mut Runtime, actor_id: u64, sequence: i64) {
    let deadline = Instant::now() + ROUNDTRIP_HOP_TIMEOUT;
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
            "remote round-trip benchmark timed out waiting for sequence {sequence}; last={seen:?}"
        );
        thread::yield_now();
    }
}

/// Reusable two-node fixture for steady-state remote actor round trips.
///
/// We explicitly seed each runtime's membership view as Healthy because cluster
/// convergence is a different benchmark. The first round trip is performed in
/// `new()` to establish the TCP connection and NUL0 handshake before Criterion
/// starts timing.
#[cfg(feature = "tcp")]
struct LoopbackRoundTrip {
    left: Runtime,
    right: Runtime,
    left_node: NodeId,
    right_node: NodeId,
    left_actor: u64,
    right_actor: u64,
}

#[cfg(feature = "tcp")]
impl LoopbackRoundTrip {
    fn new() -> Self {
        let mut left = bind_roundtrip_runtime();
        let mut right = bind_roundtrip_runtime();
        let (left_node, left_addr) = roundtrip_endpoint(&left);
        let (right_node, right_addr) = roundtrip_endpoint(&right);

        mark_roundtrip_peer_healthy(&mut left, right_node, right_addr);
        mark_roundtrip_peer_healthy(&mut right, left_node, left_addr);

        let left_actor = spawn_roundtrip_probe(&mut left);
        let right_actor = spawn_roundtrip_probe(&mut right);
        let mut fixture = Self {
            left,
            right,
            left_node,
            right_node,
            left_actor,
            right_actor,
        };

        // Connection establishment, NUL0 handshake, and first-use thread
        // scheduling are setup costs, not steady-state message latency.
        fixture.roundtrip(-1);
        fixture
    }

    fn roundtrip(&mut self, sequence: i64) {
        self.left.send_distributed(
            ActorAddress::remote(self.right_node, self.right_actor),
            "record",
            &[Value::int(sequence)],
        );
        drive_roundtrip_hop(&mut self.right, self.right_actor, sequence);

        self.right.send_distributed(
            ActorAddress::remote(self.left_node, self.left_actor),
            "record",
            &[Value::int(sequence)],
        );
        drive_roundtrip_hop(&mut self.left, self.left_actor, sequence);
    }

    fn measure(&mut self, iterations: u64) -> Duration {
        let started = Instant::now();
        for iteration in 0..iterations {
            let sequence = i64::try_from(iteration + 1).expect("criterion iteration count fits i64");
            self.roundtrip(sequence);
        }
        started.elapsed()
    }
}

/// Client-observed steady-state remote actor round-trip latency over real
/// loopback TCP in explicit plaintext mode.
///
/// One Criterion iteration is one request hop plus one return hop. Both hops
/// traverse the NUL0 transport, receiver routing, mailbox admission, scheduler,
/// and a native actor handler. Cluster bootstrap and connection establishment
/// are deliberately outside the timed body.
#[cfg(feature = "tcp")]
fn bench_remote_actor_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("dist/remote_actor_roundtrip");
    group.throughput(Throughput::Elements(1));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(3));
    group.bench_function("tcp_plaintext_1_value", |b| {
        let mut fixture = LoopbackRoundTrip::new();
        b.iter_custom(|iterations| fixture.measure(iterations));
    });
    group.finish();
}

#[cfg(not(feature = "tcp"))]
fn bench_remote_actor_roundtrip(_c: &mut Criterion) {}

criterion_group!(
    benches,
    bench_crdt_delta_compute,
    bench_gossip_membership_merge,
    bench_actor_message_encode,
    bench_actor_message_decode,
    bench_remote_actor_roundtrip
);
