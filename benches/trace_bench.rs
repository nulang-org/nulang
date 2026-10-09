use criterion::{black_box, criterion_group, Criterion};
use nulang::runtime::TraceContext;

fn trace_codec(c: &mut Criterion) {
    let ctx = TraceContext::root();
    let traceparent = ctx.to_traceparent();
    let mut group = c.benchmark_group("trace_codec");

    group.bench_function("encode", |b| {
        b.iter(|| black_box(ctx).to_traceparent());
    });

    group.bench_function("decode", |b| {
        b.iter(|| {
            TraceContext::from_traceparent(black_box(traceparent.as_str()))
                .expect("benchmark traceparent must parse")
        });
    });

    group.finish();
}

criterion_group!(pub benches, trace_codec);
