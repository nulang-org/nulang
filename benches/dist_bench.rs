//! Distribution microbenchmarks.
//!
//! Names intentionally describe the operation being measured. Local CRDT delta
//! computation and membership merge are not network synchronization or full
//! cluster convergence; NUL0 packet codec benchmarks cover the wire-format
//! work separately.

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
use nulang::runtime::{NetworkTransport, TcpTransport, TlsConfig};

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

// TRANSPORT_STAGE_BENCH_START
#[cfg(feature = "tcp")]
const TRANSPORT_ROUNDTRIP_TIMEOUT: Duration = Duration::from_secs(2);

/// Production TCP/NUL0 only: sender queue/thread -> socket -> reader thread ->
/// incoming queue. Actor routing, mailbox admission, scheduler dispatch, and
/// handler execution are deliberately outside this measurement boundary.
#[cfg(feature = "tcp")]
struct TransportRoundTrip {
    left: TcpTransport,
    right: TcpTransport,
    left_node: NodeId,
    right_node: NodeId,
    left_addr: SocketAddr,
    right_addr: SocketAddr,
}

#[cfg(feature = "tcp")]
impl TransportRoundTrip {
    fn new() -> Self {
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let mut left = TcpTransport::bind(bind_addr, TlsConfig::PlaintextInsecure)
            .expect("left benchmark transport should bind");
        let mut right = TcpTransport::bind(bind_addr, TlsConfig::PlaintextInsecure)
            .expect("right benchmark transport should bind");
        let left_node = NetworkTransport::node_id(&left);
        let right_node = NetworkTransport::node_id(&right);
        let left_addr = NetworkTransport::listen_addr(&left);
        let right_addr = NetworkTransport::listen_addr(&right);

        left.connect(right_node, right_addr)
            .expect("left benchmark transport should connect");
        right
            .connect(left_node, left_addr)
            .expect("right benchmark transport should connect");

        let mut fixture = Self {
            left,
            right,
            left_node,
            right_node,
            left_addr,
            right_addr,
        };
        // Exclude connection establishment, NUL0 handshake, and first-use
        // thread scheduling from the steady-state measurement.
        fixture.roundtrip(0);
        fixture
    }

    fn wait_for_heartbeat(transport: &TcpTransport, from: NodeId, token: u64) {
        let deadline = Instant::now() + TRANSPORT_ROUNDTRIP_TIMEOUT;
        loop {
            for incoming in transport.receive() {
                if incoming.from_node == from
                    && matches!(
                        incoming.packet,
                        Packet::Heartbeat { node_id, timestamp }
                            if node_id == from && timestamp == token
                    )
                {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "transport benchmark timed out waiting for heartbeat token {token}"
            );
            thread::yield_now();
        }
    }

    fn roundtrip(&mut self, token: u64) {
        self.left.send(
            self.right_node,
            self.right_addr,
            Packet::Heartbeat {
                node_id: self.left_node,
                timestamp: token,
            },
        );
        Self::wait_for_heartbeat(&self.right, self.left_node, token);

        self.right.send(
            self.left_node,
            self.left_addr,
            Packet::Heartbeat {
                node_id: self.right_node,
                timestamp: token,
            },
        );
        Self::wait_for_heartbeat(&self.left, self.right_node, token);
    }

    fn measure(&mut self, iterations: u64) -> Duration {
        let started = Instant::now();
        for token in 1..=iterations {
            self.roundtrip(token);
        }
        started.elapsed()
    }
}

/// Steady-state request+return latency through the real TCP/NUL0 transport,
/// excluding all actor-runtime work.
#[cfg(feature = "tcp")]
fn bench_transport_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("dist/transport_roundtrip");
    group.throughput(Throughput::Elements(1));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(3));
    group.bench_function("tcp_plaintext_heartbeat", |b| {
        let mut fixture = TransportRoundTrip::new();
        b.iter_custom(|iterations| fixture.measure(iterations));
    });
    group.finish();
}

#[cfg(not(feature = "tcp"))]
fn bench_transport_roundtrip(_c: &mut Criterion) {}
// TRANSPORT_STAGE_BENCH_END

criterion_group!(
    benches,
    bench_crdt_delta_compute,
    bench_gossip_membership_merge,
    bench_actor_message_encode,
    bench_actor_message_decode,
    bench_transport_roundtrip
);
