//! GC benchmarks: ORCA throughput, cycle detection.

use criterion::{black_box, criterion_group, Criterion};
use nulang::runtime::Runtime;
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
            assert_eq!(admitted, 20 * 19, "ORCA benchmark must time admitted actor traffic");
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

criterion_group!(benches, bench_orca_throughput, bench_cycle_detection);
