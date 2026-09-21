//! Actor throughput and density benchmarks.

use criterion::{black_box, criterion_group, BatchSize, Criterion};
use nulang::runtime::{Actor, Runtime};
use nulang::vm::Value;

fn noop(_actor: &mut Actor, _args: &[Value]) {}

fn runtime_with_consumer() -> (Runtime, u64) {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(Vec::new));
    rt.actors
        .get_mut(&actor_id)
        .expect("spawned actor")
        .register_behavior("handle", noop);
    // Drain the spawn-time scheduler entry so message benchmarks time only
    // message enqueue + dispatch rather than actor startup bookkeeping.
    rt.run_scheduler();
    (rt, actor_id)
}

fn bench_spawn_send_receive(c: &mut Criterion) {
    c.bench_function("actor/spawn_send_receive", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            let actor_id = rt.spawn_actor(Box::new(Vec::new));
            rt.actors
                .get_mut(&actor_id)
                .expect("spawned actor")
                .register_behavior("handle", noop);
            rt.send_message(actor_id, "handle", &[Value::int(42)]);
            rt.run_scheduler();

            let reductions = rt
                .actors
                .get(&actor_id)
                .expect("actor remains alive")
                .reduction_count;
            black_box((actor_id, reductions));
        })
    });
}

fn bench_message_throughput(c: &mut Criterion) {
    const MESSAGES: usize = 10_000;

    c.bench_function("actor/message_throughput_10k", |b| {
        b.iter_batched(
            runtime_with_consumer,
            |(mut rt, consumer)| {
                let msg = Value::int(1);
                for _ in 0..MESSAGES {
                    rt.send_message(consumer, "handle", &[msg]);
                }
                rt.run_scheduler();

                let actor = rt.actors.get(&consumer).expect("consumer remains alive");
                black_box((actor.reduction_count, rt.scheduler_stats()));
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_idle_actor_spawn(c: &mut Criterion) {
    const ACTORS: usize = 1_000;

    c.bench_function("actor/idle_spawn_1k", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            for _ in 0..ACTORS {
                black_box(rt.spawn_actor(Box::new(Vec::new)));
            }

            let materialized_heaps = rt
                .actors
                .values()
                .filter(|actor| actor.heap.has_active_bump_block())
                .count();
            black_box((rt.actors.len(), materialized_heaps));
        })
    });
}

criterion_group!(
    benches,
    bench_spawn_send_receive,
    bench_message_throughput,
    bench_idle_actor_spawn
);
