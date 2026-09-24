//! Actor runtime microbenchmarks.
//!
//! Keep setup outside timed sections whenever the benchmark is intended to
//! represent a specific operation. End-to-end lifecycle benchmarks are named
//! accordingly so their timings are not misread as message throughput.

use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput};
use nulang::runtime::{Mailbox, Message, MessagePayload, MessagePriority, Runtime};
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

fn register_roundtrip_counter(rt: &mut Runtime, actor_id: u64) {
    rt.actors
        .get_mut(&actor_id)
        .expect("roundtrip actor")
        .register_behavior("inc", |actor, args| {
            let count = actor
                .get_state_field("count")
                .and_then(|value| value.as_int())
                .unwrap_or(0);
            let by = args.first().and_then(|value| value.as_int()).unwrap_or(1);
            actor.set_state_field("count", Value::int(count + by));
        });
}

fn roundtrip_count(rt: &Runtime, actor_id: u64) -> i64 {
    rt.actors
        .get(&actor_id)
        .and_then(|actor| actor.get_state_field("count"))
        .and_then(|value| value.as_int())
        .unwrap_or(-1)
}

fn local_roundtrip_fixture() -> (Runtime, u64) {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| {
        vec![("count".to_string(), Value::int(0))]
    }));
    register_roundtrip_counter(&mut rt, actor_id);
    // Consume the spawn-time ready token so the timed section measures only
    // the message burst plus handler execution.
    rt.run_scheduler();
    (rt, actor_id)
}

fn cross_shard_roundtrip_fixture() -> (Runtime, Runtime, u64) {
    let mut shards = Runtime::new_sharded(2);
    let mut actor_id = shards[1].spawn_actor(Box::new(|| {
        vec![("count".to_string(), Value::int(0))]
    }));
    while actor_id % 2 != 1 {
        actor_id = shards[1].spawn_actor(Box::new(|| {
            vec![("count".to_string(), Value::int(0))]
        }));
    }
    register_roundtrip_counter(&mut shards[1], actor_id);
    shards[1].run_scheduler();

    let receiver = shards.pop().expect("receiver shard");
    let sender = shards.pop().expect("sender shard");
    (sender, receiver, actor_id)
}

/// End-to-end message burst comparison with runtime construction excluded.
/// The cross-shard batch stays below the 1024-entry bounded shard-bus capacity,
/// so this measures transport admission + destination drain + the same native
/// handler work without making backpressure/drop behavior part of the result.
fn bench_message_roundtrip(c: &mut Criterion) {
    const ROUNDTRIP_BATCH: usize = 512;
    let mut group = c.benchmark_group("actor/message_roundtrip");
    group.throughput(Throughput::Elements(ROUNDTRIP_BATCH as u64));

    group.bench_function("same_shard_512", |b| {
        b.iter_batched(
            local_roundtrip_fixture,
            |(mut rt, actor_id)| {
                for _ in 0..ROUNDTRIP_BATCH {
                    rt.send_message_by_id(actor_id, 0, &[Value::int(1)]);
                }
                rt.run_scheduler();
                let count = roundtrip_count(&rt, actor_id);
                debug_assert_eq!(count, ROUNDTRIP_BATCH as i64);
                black_box(count);
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function("cross_shard_512", |b| {
        b.iter_batched(
            cross_shard_roundtrip_fixture,
            |(mut sender, mut receiver, actor_id)| {
                for _ in 0..ROUNDTRIP_BATCH {
                    sender.send_message_by_id(actor_id, 0, &[Value::int(1)]);
                }
                receiver.run_scheduler();
                let count = roundtrip_count(&receiver, actor_id);
                debug_assert_eq!(count, ROUNDTRIP_BATCH as i64);
                black_box(count);
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
    bench_message_roundtrip,
    bench_message_drain,
    bench_selective_receive
);
