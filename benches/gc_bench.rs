//! GC benchmarks: ORCA throughput, cycle detection.

use criterion::{black_box, criterion_group, Criterion};
use nulang::runtime::{ActorHeap, Runtime, TypeTag};
use nulang::types::ExitReason;
use nulang::vm::Value;

fn bench_idle_heap_construction(c: &mut Criterion) {
    c.bench_function("gc/idle_heap_construct_1000", |b| {
        b.iter(|| {
            for _ in 0..1_000 {
                black_box(ActorHeap::new(16 * 1024));
            }
        })
    });
}

fn bench_first_heap_allocation(c: &mut Criterion) {
    c.bench_function("gc/heap_first_alloc_1000", |b| {
        b.iter(|| {
            for _ in 0..1_000 {
                let mut heap = ActorHeap::new(16 * 1024);
                heap.set_actor_id(1);
                let ptr = heap.alloc(8, TypeTag::Raw).expect("heap allocation");
                black_box(ptr);
            }
        })
    });
}

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

criterion_group!(
    benches,
    bench_idle_heap_construction,
    bench_first_heap_allocation,
    bench_orca_throughput,
    bench_cycle_detection
);
