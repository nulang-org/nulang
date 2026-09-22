//! GC benchmarks: ORCA throughput, cycle detection.

use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput};
use nulang::runtime::{ActorHeap, Runtime, TypeTag};
use nulang::types::ExitReason;
use nulang::vm::Value;

fn bench_orca_throughput(c: &mut Criterion) {
    c.bench_function("gc/orca_throughput", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            let mut actors = Vec::new();
            for _ in 0..20 {
                let a = rt.spawn_actor(Box::new(|| vec![]));
                actors.push(a);
            }
            let msg = Value::int(42);
            for i in 0..20 {
                for j in 0..20 {
                    if i != j {
                        rt.send_message(actors[i], "handle", &[msg]);
                    }
                }
            }
            for _ in 0..200 {
                rt.run_scheduler();
                rt.process_gc_ops();
            }
            for a in &actors {
                rt.exit_actor(*a, ExitReason::Normal);
            }
            for _ in 0..100 {
                rt.run_scheduler();
                rt.process_gc_ops();
            }
            black_box(());
        })
    });
}

fn bench_cycle_detection(c: &mut Criterion) {
    c.bench_function("gc/cycle_detection", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            let a = rt.spawn_actor(Box::new(|| vec![]));
            let b = rt.spawn_actor(Box::new(|| vec![]));
            let c = rt.spawn_actor(Box::new(|| vec![]));
            rt.monitor(a, b);
            rt.monitor(b, c);
            rt.monitor(c, a);
            rt.exit_actor(a, ExitReason::Normal);
            rt.exit_actor(b, ExitReason::Normal);
            rt.exit_actor(c, ExitReason::Normal);
            for _ in 0..200 {
                rt.run_scheduler();
                rt.process_gc_ops();
            }
            black_box(());
        })
    });
}


fn bench_actor_heap_medium_alloc(c: &mut Criterion) {
    const ALLOCS: usize = 64;
    let mut group = c.benchmark_group("gc/actor_heap_medium_alloc");

    for payload_size in [256usize, 512, 1024, 2048, 3000] {
        group.throughput(Throughput::Elements(ALLOCS as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(payload_size),
            &payload_size,
            |b, &payload_size| {
                b.iter_batched(
                    || ActorHeap::new(16 * 1024),
                    |mut heap| {
                        let mut ptrs = Vec::with_capacity(ALLOCS);
                        for _ in 0..ALLOCS {
                            ptrs.push(
                                heap.alloc(payload_size, TypeTag::Raw)
                                    .expect("actor heap allocation"),
                            );
                        }
                        black_box(&ptrs);
                        for ptr in ptrs {
                            // SAFETY: each pointer was allocated once from
                            // this heap and has not yet been freed.
                            unsafe { heap.free(ptr) };
                        }
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_orca_throughput,
    bench_cycle_detection,
    bench_actor_heap_medium_alloc
);
