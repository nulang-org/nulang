//! Scheduler dispatch microbenchmarks.
//!
//! These isolate queue mechanics from actor execution so scheduler changes can
//! be compared without VM/GC noise. Run with:
//!
//! `cargo bench --bench bench_main -- scheduler`

use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput};
use nulang::runtime::{ActorPriority, Scheduler};

fn populated_global_scheduler(count: usize) -> Scheduler {
    let scheduler = Scheduler::new(4);
    for id in 0..count {
        scheduler.enqueue(id as u64 + 1);
    }
    scheduler
}

fn populated_mixed_scheduler(count: usize) -> Scheduler {
    let scheduler = Scheduler::new(4);
    for id in 0..count {
        let priority = match id % 10 {
            0 => ActorPriority::High,
            1 => ActorPriority::Low,
            _ => ActorPriority::Normal,
        };
        scheduler.enqueue_with_priority(id as u64 + 1, priority);
    }
    scheduler
}

fn peer_populated_scheduler(count: usize) -> Scheduler {
    let scheduler = Scheduler::new(4);
    for id in 0..count {
        // Leave worker 0 empty so next_task(0) must exercise peer stealing.
        let worker = 1 + (id % 3);
        scheduler.enqueue_local(worker, id as u64 + 1);
    }
    scheduler
}

fn drain_owner(scheduler: Scheduler) -> usize {
    let mut drained = 0usize;
    while let Some(actor_id) = scheduler.dequeue() {
        black_box(actor_id);
        drained += 1;
    }
    black_box(drained)
}

fn drain_worker_zero(scheduler: Scheduler) -> usize {
    let mut drained = 0usize;
    while let Some(actor_id) = scheduler.next_task(0) {
        black_box(actor_id);
        drained += 1;
    }
    black_box(drained)
}

fn bench_global_owner_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler/global_owner_dispatch");

    for count in [1_000usize, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || populated_global_scheduler(count),
                |scheduler| {
                    let drained = drain_owner(scheduler);
                    debug_assert_eq!(drained, count);
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_mixed_priority_owner_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler/mixed_priority_owner_dispatch");

    for count in [1_000usize, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || populated_mixed_scheduler(count),
                |scheduler| {
                    let drained = drain_owner(scheduler);
                    debug_assert_eq!(drained, count);
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_peer_steal_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler/peer_steal_dispatch");

    for count in [1_000usize, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || peer_populated_scheduler(count),
                |scheduler| {
                    let drained = drain_worker_zero(scheduler);
                    debug_assert_eq!(drained, count);
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_global_owner_dispatch,
    bench_mixed_priority_owner_dispatch,
    bench_peer_steal_dispatch
);
