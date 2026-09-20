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

criterion_group!(
    benches,
    bench_crdt_delta_compute,
    bench_gossip_membership_merge,
    bench_actor_message_encode,
    bench_actor_message_decode
);
