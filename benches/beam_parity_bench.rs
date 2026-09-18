//! BEAM-parity actor runtime benchmarks.
//!
//! These benchmarks intentionally focus on the runtime properties where BEAM
//! is the relevant baseline: actor creation, mailbox throughput, and fan-out
//! across many runnable actors. They do not claim BEAM parity by themselves;
//! they provide stable Nulang-side measurements that can be paired with an
//! equivalent Erlang harness on the same host.
//!
//! Run with:
//!
//! `cargo bench --bench bench_main -- beam_parity`

use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput};
use nulang::runtime::{Actor, Runtime};
use nulang::vm::Value;

fn noop_handler(_actor: &mut Actor, _args: &[Value]) {}

fn spawn_noop_actor(rt: &mut Runtime) -> u64 {
    let actor = rt.spawn_actor(Box::new(|| vec![]));
    rt.actors
        .get_mut(&actor)
        .expect("fresh actor must be resident")
        .register_behavior("handle", noop_handler);
    actor
}

fn bench_spawn_idle_actors(c: &mut Criterion) {
    let mut group = c.benchmark_group("beam_parity/spawn_idle");
    // Large actor batches are expensive on shared CI runners. Twenty samples
    // is enough to detect order-of-magnitude changes while keeping the main
    // benchmark job bounded.
    group.sample_size(20);

    for count in [1_000usize, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                Runtime::new,
                |mut rt| {
                    for _ in 0..count {
                        black_box(rt.spawn_actor(Box::new(|| vec![])));
                    }
                    black_box(rt.actors.len());
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_single_mailbox_flood(c: &mut Criterion) {
    let mut group = c.benchmark_group("beam_parity/single_mailbox_flood");
    group.sample_size(20);

    for count in [1_000usize, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || {
                    let mut rt = Runtime::new();
                    let actor = spawn_noop_actor(&mut rt);
                    // Settle the actor outside the timed section so this
                    // isolates mailbox enqueue + dispatch rather than spawn.
                    rt.run_scheduler();
                    (rt, actor)
                },
                |(mut rt, actor)| {
                    let msg = Value::int(1);
                    for _ in 0..count {
                        rt.send_message(actor, "handle", &[msg]);
                    }
                    rt.run_scheduler();
                    rt.process_gc_ops();
                    black_box(rt.actors.get(&actor).map(|a| a.reduction_count));
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_fanout_one_message_each(c: &mut Criterion) {
    let mut group = c.benchmark_group("beam_parity/fanout_one_message_each");
    group.sample_size(20);

    for count in [1_000usize, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || {
                    let mut rt = Runtime::new();
                    let actors = (0..count)
                        .map(|_| spawn_noop_actor(&mut rt))
                        .collect::<Vec<_>>();
                    // Drain the initial spawn scheduling outside the timed
                    // section so this benchmark isolates fan-out + dispatch.
                    rt.run_scheduler();
                    (rt, actors)
                },
                |(mut rt, actors)| {
                    let msg = Value::int(1);
                    for actor in actors {
                        rt.send_message(actor, "handle", &[msg]);
                    }
                    rt.run_scheduler();
                    rt.process_gc_ops();
                    black_box(rt.actors.len());
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_spawn_idle_actors,
    bench_single_mailbox_flood,
    bench_fanout_one_message_each
);
