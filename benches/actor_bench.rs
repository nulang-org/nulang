//! Actor throughput benchmarks.

use criterion::{black_box, criterion_group, Criterion};
use nulang::bytecode::{CodeModule, Constant};
use nulang::runtime::{Actor, Runtime, StateModel};
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

fn bench_state_lookup(c: &mut Criterion) {
    let mut actor = Actor::new(1, "bench", 0);
    actor
        .state_models
        .insert("count".to_string(), StateModel::Local);
    actor.set_state_field("count", Value::int(42));

    let mut module = CodeModule::new("state-bench");
    let count_idx = module.add_constant(Constant::String("count".to_string()));
    actor.install_state_schema(&module, &["count".to_string()]);

    c.bench_function("actor/state_get_string_map", |b| {
        b.iter(|| black_box(actor.get_state_field(black_box("count"))))
    });
    c.bench_function("actor/state_get_dense_slot", |b| {
        b.iter(|| black_box(actor.get_state_field_by_constant(black_box(count_idx))))
    });
}

criterion_group!(
    benches,
    bench_spawn_send_receive,
    bench_message_throughput,
    bench_state_lookup
);
