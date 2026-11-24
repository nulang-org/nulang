//! Diagnostic-only W3C trace-context microbenchmarks.
//!
//! Keep these separate from the actor benchmark history: none of these probes
//! enqueues a message, runs a scheduler, or executes an actor handler.
//!
//! Run:
//!   cargo bench --locked --no-default-features --bench trace_profile
//! Run only the smoke harness:
//!   cargo bench --locked --no-default-features --bench trace_profile -- --test

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use nulang::runtime::TraceContext;

const W3C_PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn bench_trace_context(c: &mut Criterion) {
    let incoming = TraceContext::from_traceparent(W3C_PARENT)
        .expect("fixture must be a valid W3C traceparent");

    c.bench_function("trace_context/root_generation", |b| {
        b.iter(|| black_box(TraceContext::root()))
    });
    c.bench_function("trace_context/child_generation", |b| {
        b.iter(|| black_box(black_box(incoming).child()))
    });
    c.bench_function("trace_context/parse_traceparent", |b| {
        b.iter(|| {
            black_box(
                TraceContext::from_traceparent(black_box(W3C_PARENT))
                    .expect("valid traceparent must parse"),
            )
        })
    });
    c.bench_function("trace_context/parse_and_child", |b| {
        b.iter(|| {
            let parent = TraceContext::from_traceparent(black_box(W3C_PARENT))
                .expect("valid traceparent must parse");
            black_box(parent.child())
        })
    });
    c.bench_function("trace_context/encode_traceparent", |b| {
        b.iter(|| black_box(black_box(incoming).to_traceparent()))
    });
    c.bench_function("trace_context/dispatch_span_guard", |b| {
        b.iter(|| black_box(black_box(incoming).enter_dispatch_span(black_box(42), black_box(0))))
    });
}

criterion_group!(benches, bench_trace_context);
criterion_main!(benches);
