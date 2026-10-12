//! GC benchmarks: ORCA throughput, cycle detection.

use criterion::{black_box, criterion_group, BatchSize, Criterion, Throughput};
use nulang::runtime::{Runtime, TypeTag};
use nulang::types::ExitReason;
use nulang::vm::Value;

fn noop_handler(_actor: &mut nulang::runtime::Actor, _args: &[Value]) {}

fn bench_orca_throughput(c: &mut Criterion) {
    c.bench_function("gc/orca_throughput", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            let mut actors = Vec::new();
            for _ in 0..20 {
                let a = rt.spawn_actor(Box::new(|| vec![]));
                rt.actors
                    .get_mut(&a)
                    .expect("spawned actor")
                    .register_behavior("handle", noop_handler);
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
            let admitted: usize = actors
                .iter()
                .map(|id| rt.actors.get(id).expect("benchmark actor").mailbox.len())
                .sum();
            assert_eq!(
                admitted,
                20 * 19,
                "ORCA benchmark must time admitted actor traffic"
            );
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

fn foreign_ref_fixture(count: usize) -> (Runtime, u64, u64, Vec<*mut u8>) {
    let mut rt = Runtime::new();
    let owner = rt.spawn_actor(Box::new(|| vec![]));
    let target = rt.spawn_actor(Box::new(|| vec![]));

    for actor_id in [owner, target] {
        rt.actors
            .get_mut(&actor_id)
            .expect("spawned actor")
            .register_behavior("handle", noop_handler);
    }
    // Clear spawn-time ready ownership outside the timed section.
    rt.run_scheduler();

    let ptrs = {
        let actor = rt.actors.get_mut(&owner).expect("owner actor");
        (0..count)
            .map(|_| {
                actor
                    .heap
                    .alloc(16, TypeTag::Raw)
                    .expect("benchmark heap allocation")
            })
            .collect()
    };

    (rt, owner, target, ptrs)
}

/// Cross-actor ORCA send bookkeeping over real heap pointers.
///
/// Runtime/actor construction and heap allocation are setup. The timed body
/// includes local message admission, foreign-count bumps, coordinator ops,
/// cycle-detector edge registration, and ready-state publication. It excludes
/// handler execution and foreign-op draining.
fn bench_orca_foreign_ref_send(c: &mut Criterion) {
    const REFS: usize = 256;
    let mut group = c.benchmark_group("gc/orca_foreign_ref_send");
    group.throughput(Throughput::Elements(REFS as u64));

    group.bench_function("256", |b| {
        b.iter_batched(
            || foreign_ref_fixture(REFS),
            |(mut rt, owner, target, ptrs)| {
                rt.current_actor = Some(owner);
                for ptr in ptrs {
                    // SAFETY: every pointer comes from the live owner actor's
                    // heap in this iteration and the Runtime owns that heap for
                    // the entire send sequence.
                    let value = unsafe { Value::ptr(ptr) };
                    rt.send_message_by_id(target, 0, &[value]);
                }
                assert_eq!(
                    rt.gc_stats().foreign_refs_sent,
                    REFS as u64,
                    "ORCA benchmark must record every injected foreign ref"
                );
                black_box(rt);
            },
            BatchSize::SmallInput,
        )
    });

    group.finish();
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
    bench_orca_throughput,
    bench_orca_foreign_ref_send,
    bench_cycle_detection
);
