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
use nulang::runtime::Runtime;
use nulang::vm::Value;

fn bench_spawn_idle_actors(c: &mut Criterion) {
    let mut group = c.benchmark_group("beam_parity/spawn_idle");
    // Large actor batches are expensive on shared CI runners. Twenty samples
    // is enough to detect order-of-magnitude changes while keeping the main
    // benchmark job bounded.
    group.sample_size(20);

    for count in [1_000usize, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let mut rt = Runtime::new();
                for _ in 0..count {
                    black_box(rt.spawn_actor(Box::new(|| vec![])));
                }
                black_box(rt.actors.len());
            });
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
                    let actor = rt.spawn_actor(Box::new(|| vec![]));
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
                        .map(|_| rt.spawn_actor(Box::new(|| vec![])))
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
