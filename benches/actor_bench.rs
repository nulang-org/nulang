//! Actor throughput benchmarks.

use criterion::{black_box, criterion_group, BatchSize, Criterion, Throughput};
use nulang::runtime::{FlightRecorder, Runtime};
use nulang::vm::Value;

fn bench_spawn_send_receive(c: &mut Criterion) {
    c.bench_function("actor/spawn_send_receive", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            let actor_id = rt.spawn_actor(Box::new(|| vec![]));
            let msg = Value::int(42);
            rt.send_message(actor_id, "handle", &[msg]);
            for _ in 0..20 {
                rt.run_scheduler();
                rt.process_gc_ops();
            }
            black_box(actor_id);
        })
    });
}

fn bench_message_throughput(c: &mut Criterion) {
    c.bench_function("actor/message_throughput", |b| {
        b.iter(|| {
            let mut rt = Runtime::new();
            let consumer = rt.spawn_actor(Box::new(|| vec![]));
            let msg = Value::int(1);
            for _ in 0..100 {
                rt.send_message(consumer, "handle", &[msg]);
            }
            for _ in 0..200 {
                rt.run_scheduler();
                rt.process_gc_ops();
            }
            black_box(consumer);
        })
    });
}

/// Measures the cost of populating a fresh actor flight recorder through its
/// configured 1,000-entry retention window. This specifically guards the
/// actor-density optimization that makes the recorder's backing vector lazy:
/// idle actors should reserve nothing, while the first 1,000 traced messages
/// must remain reasonably cheap.
fn bench_flight_recorder_fill(c: &mut Criterion) {
    const ENTRIES: usize = 1_000;
    let mut group = c.benchmark_group("actor/flight_recorder_fill");
    group.throughput(Throughput::Elements(ENTRIES as u64));
    group.bench_function("1000", |b| {
        b.iter_batched(
            || FlightRecorder::new(ENTRIES),
            |mut recorder| {
                for sender in 0..ENTRIES {
                    recorder.record(sender as u64, 0, &[]);
                }
                black_box(recorder.len());
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_spawn_send_receive,
    bench_message_throughput,
    bench_flight_recorder_fill
);
