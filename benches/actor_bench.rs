//! Actor runtime microbenchmarks.
//!
//! Keep setup outside timed sections whenever the benchmark is intended to
//! represent a specific operation. End-to-end lifecycle benchmarks are named
//! accordingly so their timings are not misread as message throughput.

use criterion::{black_box, criterion_group, BatchSize, Criterion, Throughput};
use nulang::runtime::Runtime;
use nulang::vm::Value;

const MESSAGE_BATCH: usize = 100;

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
    let admission = rt.send_message(actor_id, "handle", &[Value::int(0)]);
    assert!(admission.admitted(), "benchmark message must be admitted");
    rt.run_scheduler();
    rt.process_gc_ops();

    (rt, actor_id)
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

            let admission = rt.send_message(actor_id, "handle", &[Value::int(42)]);
            assert!(admission.admitted(), "benchmark message must be admitted");
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

    group.bench_function("100", |b| {
        b.iter_batched(
            runtime_with_consumer,
            |(mut rt, actor_id)| {
                let msg = Value::int(1);
                for _ in 0..MESSAGE_BATCH {
                    let admission = rt.send_message(actor_id, "handle", &[msg]);
                    debug_assert!(admission.admitted());
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
                    let admission = rt.send_message(actor_id, "handle", &[msg]);
                    assert!(admission.admitted(), "benchmark message must be admitted");
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

criterion_group!(
    benches,
    bench_spawn_send_receive,
    bench_message_enqueue,
    bench_message_drain
);
