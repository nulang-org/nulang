//! Actor runtime microbenchmarks.
//!
//! Keep setup outside timed sections whenever the benchmark is intended to
//! represent a specific operation. End-to-end lifecycle benchmarks are named
//! accordingly so their timings are not misread as message throughput.

use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput};
use nulang::runtime::{
    FrozenGraph, FrozenNode, Mailbox, Message, MessagePayload, MessagePriority, Runtime,
};
use nulang::vm::Value;

const MESSAGE_BATCH: usize = 100;
const IDLE_ACTOR_BATCH: usize = 1_000;

fn noop_handler(_actor: &mut nulang::runtime::Actor, _args: &[Value]) {}

/// Build an idle runtime with one actor that can actually accept the behavior
/// used by these benchmarks.
///
/// Registering the behavior matters: a bare actor with no "handle" behavior can
/// reject the send path, which would turn a supposed execution benchmark into
/// a benchmark of rejection/setup overhead.
fn runtime_with_consumer() -> (Runtime, u64) {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![]));
    rt.actors
        .get_mut(&actor_id)
        .expect("spawned actor")
        .register_behavior("handle", noop_handler);

    // Drain any spawn-time scheduling state and verify the measured behavior is
    // admissible before entering a timed iteration.
    rt.send_message(actor_id, "handle", &[Value::int(0)]);
    rt.run_scheduler();
    rt.process_gc_ops();

    (rt, actor_id)
}

/// Idle actor spawn throughput. This keeps message execution out of the timed
/// path so eager per-actor allocations are visible directly.
fn bench_spawn_idle_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("actor/spawn_idle");
    group.throughput(Throughput::Elements(IDLE_ACTOR_BATCH as u64));

    group.bench_function("1000", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            for _ in 0..IDLE_ACTOR_BATCH {
                black_box(rt.spawn_actor(Box::new(|| vec![])));
            }
            black_box(rt.actor_count());
        })
    });

    group.finish();
}

/// End-to-end runtime lifecycle cost: construct runtime, spawn actor, register a
/// native behavior, enqueue one message, execute it, and process pending GC.
///
/// This is intentionally *not* reported as message throughput.
fn bench_spawn_send_receive(c: &mut Criterion) {
    c.bench_function("actor/lifecycle_spawn_send_receive_gc", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            let actor_id = rt.spawn_actor(Box::new(|| vec![]));
            rt.actors
                .get_mut(&actor_id)
                .expect("spawned actor")
                .register_behavior("handle", noop_handler);

            rt.send_message(actor_id, "handle", &[Value::int(42)]);
            rt.run_scheduler();
            rt.process_gc_ops();
            black_box(actor_id);
        })
    });
}

/// Local mailbox admission/enqueue cost for a batch of primitive messages.
///
/// Runtime construction, actor creation, and behavior registration are setup
/// and therefore excluded from the timed body.
fn bench_message_enqueue(c: &mut Criterion) {
    let mut group = c.benchmark_group("actor/message_enqueue");
    group.throughput(Throughput::Elements(MESSAGE_BATCH as u64));

    // Preserve the existing benchmark name for rolling-history continuity.
    group.bench_function("100", |b| {
        b.iter_batched(
            runtime_with_consumer,
            |(mut rt, actor_id)| {
                let msg = Value::int(1);
                for _ in 0..MESSAGE_BATCH {
                    rt.send_message(actor_id, "handle", &[msg]);
                }
                black_box(rt);
            },
            BatchSize::SmallInput,
        )
    });

    // Compiler-generated sends already carry numeric behavior ids. Keep a
    // separate signal for the scheduler/mailbox hot path without behavior-name
    // lookup so ready-token dedup is measurable independently.
    group.bench_function("by_id_100", |b| {
        b.iter_batched(
            runtime_with_consumer,
            |(mut rt, actor_id)| {
                let msg = Value::int(1);
                for _ in 0..MESSAGE_BATCH {
                    rt.send_message_by_id(actor_id, 0, &[msg]);
                }
                black_box(rt);
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("by_id_shared_5arg_100", |b| {
        b.iter_batched(
            runtime_with_consumer,
            |(mut rt, actor_id)| {
                let args = [
                    Value::int(1),
                    Value::int(2),
                    Value::int(3),
                    Value::int(4),
                    Value::int(5),
                ];
                for _ in 0..MESSAGE_BATCH {
                    rt.send_message_by_id(actor_id, 0, &args);
                }
                black_box(rt);
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

/// Scheduler + native-handler execution cost for an already-enqueued batch.
///
/// Enqueueing is performed in setup, outside the timed section. Primitive
/// payloads avoid cross-actor heap/ORCA work so this specifically tracks the
/// local mailbox/scheduler/handler path.
fn bench_message_drain(c: &mut Criterion) {
    let mut group = c.benchmark_group("actor/message_drain");
    group.throughput(Throughput::Elements(MESSAGE_BATCH as u64));

    group.bench_function("100", |b| {
        b.iter_batched(
            || {
                let (mut rt, actor_id) = runtime_with_consumer();
                let msg = Value::int(1);
                for _ in 0..MESSAGE_BATCH {
                    rt.send_message(actor_id, "handle", &[msg]);
                }
                rt
            },
            |mut rt| {
                rt.run_scheduler();
                black_box(rt);
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn selective_receive_mailbox(depth: usize, hit_behavior: u16) -> Mailbox {
    let mut mailbox = Mailbox::new(0);
    for _ in 0..depth.saturating_sub(1) {
        mailbox
            .push_local(Message {
                behavior_id: 1,
                payload: MessagePayload::from_slice(&[Value::int(1)]),
                sender: 0,
                priority: MessagePriority::Normal,
                trace_id: None,
            })
            .unwrap();
    }
    mailbox
        .push_local(Message {
            behavior_id: hit_behavior,
            payload: MessagePayload::from_slice(&[Value::int(42)]),
            sender: 0,
            priority: MessagePriority::Normal,
            trace_id: None,
        })
        .unwrap();
    mailbox
}

fn frozen_fanout_fixture(payload_bytes: usize, fanout: usize) -> (Runtime, Vec<u64>, Value) {
    let mut rt = Runtime::new();
    let mut receivers = Vec::with_capacity(fanout);
    for _ in 0..fanout {
        receivers.push(rt.spawn_actor(Box::new(|| vec![])));
    }
    // Remove spawn-time ready tokens from the measured path.
    rt.run_scheduler();

    let graph = FrozenGraph::new(vec![FrozenNode::Bytes(vec![7u8; payload_bytes])], 0)
        .expect("valid frozen benchmark graph");
    let object_id = rt
        .object_store
        .put_frozen(&graph)
        .expect("frozen benchmark object");
    (rt, receivers, Value::object(object_id))
}

/// Frozen-object fan-out should scale with receiver count, not payload bytes:
/// each local send carries one ObjectId and takes one receiver hold over the
/// node-shared immutable allocation.
fn bench_frozen_object_fanout(c: &mut Criterion) {
    let mut group = c.benchmark_group("actor/frozen_object_fanout");

    for payload_bytes in [1usize << 10, 64usize << 10, 1usize << 20] {
        for fanout in [1usize, 16, 256] {
            group.throughput(Throughput::Elements(fanout as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("{payload_bytes}_bytes"), fanout),
                &(payload_bytes, fanout),
                |b, &(payload_bytes, fanout)| {
                    b.iter_batched(
                        || frozen_fanout_fixture(payload_bytes, fanout),
                        |(mut rt, receivers, object)| {
                            for actor_id in receivers {
                                rt.send_message_by_id(actor_id, 0, &[object]);
                            }
                            black_box(rt);
                        },
                        BatchSize::SmallInput,
                    )
                },
            );
        }
    }

    group.finish();
}

fn bench_selective_receive(c: &mut Criterion) {
    const HIT: u16 = 60_000;

    let mut group = c.benchmark_group("actor/selective_receive_depth");
    for depth in [64usize, 1024, 16_384] {
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            b.iter_batched(
                || selective_receive_mailbox(depth, HIT),
                |mut mailbox| black_box(mailbox.receive_match(black_box(&[HIT]))),
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();

    let mut group = c.benchmark_group("actor/selective_receive_arms");
    for arm_count in [1usize, 8, 32] {
        let mut behavior_ids: Vec<u16> = (10_000..10_000 + arm_count as u16).collect();
        behavior_ids.push(HIT);
        group.bench_with_input(
            BenchmarkId::from_parameter(arm_count),
            &arm_count,
            |b, _| {
                b.iter_batched(
                    || selective_receive_mailbox(4096, HIT),
                    |mut mailbox| {
                        black_box(mailbox.receive_match(black_box(behavior_ids.as_slice())))
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }
    group.finish();

    let mut group = c.benchmark_group("actor/selective_receive_guard_retry");
    group.bench_function("32_rejections", |b| {
        b.iter_batched(
            || {
                let mut mailbox = Mailbox::new(0);
                for sender in 0..32u64 {
                    mailbox
                        .push_local(Message {
                            behavior_id: HIT,
                            payload: MessagePayload::from_slice(&[Value::int(sender as i64)]),
                            sender,
                            priority: MessagePriority::Normal,
                            trace_id: None,
                        })
                        .unwrap();
                }
                mailbox
            },
            |mut mailbox| {
                for _ in 0..32 {
                    black_box(mailbox.receive_match(black_box(&[HIT])));
                }
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_spawn_idle_batch,
    bench_spawn_send_receive,
    bench_message_enqueue,
    bench_message_drain,
    bench_frozen_object_fanout,
    bench_selective_receive
);
