//! Controlled actor dispatch profile; temporary local measurement fixture.
//! Enqueue outside the timed section and verify admission and drain counts.
//! Compare traced and untraced receives on the *same* runtime configuration.

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use nulang::runtime::{Actor, Runtime, TraceContext};
use nulang::vm::Value;

const BATCH: usize = 100;

fn noop_handler(_actor: &mut Actor, _args: &[Value]) {}

fn make_runtime(with_traceparent: bool) -> (Runtime, u64) {
    let mut rt = Runtime::new();
    let id = rt.spawn_actor(Box::new(Vec::new));
    rt.actors
        .get_mut(&id)
        .expect("actor exists")
        .register_behavior("handle", noop_handler);

    // Drain spawn and handler-initialization side effects.
    rt.send_message(id, "handle", &[Value::int(0)]);
    rt.run_scheduler();
    rt.process_gc_ops();
    assert!(rt.actors.get(&id).expect("actor exists").mailbox.is_empty());

    if with_traceparent {
        rt.current_trace = Some(TraceContext::root());
    }
    for i in 0..BATCH {
        rt.send_message(id, "handle", &[Value::int(i as i64)]);
    }
    rt.current_trace = None;
    assert_eq!(
        rt.actors.get(&id).expect("actor exists").mailbox.len(),
        BATCH,
        "fixture cannot silently drop/admission-reject messages"
    );
    (rt, id)
}

fn actor_trace_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("trace_actor/drain_100");
    group.throughput(Throughput::Elements(BATCH as u64));

    for (name, traced) in [
        ("root_per_receive", false),
        ("parse_child_per_receive", true),
    ] {
        group.bench_function(name, |b| {
            b.iter_batched_ref(
                || make_runtime(traced),
                |(rt, id)| {
                    rt.run_scheduler();
                    assert!(
                        rt.actors
                            .get(&*id)
                            .expect("actor exists")
                            .mailbox
                            .is_empty(),
                        "benchmark must execute all enqueued messages"
                    );
                    black_box(rt);
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

criterion_group!(benches, actor_trace_benches);
criterion_main!(benches);
