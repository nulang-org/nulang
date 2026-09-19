//! RESP cache-kernel microbenchmarks.
//!
//! Run with:
//! `cargo bench --bench bench_main -- cache`

use criterion::{black_box, criterion_group, Criterion, Throughput};
use nulang::runtime::{execute_frame, redis_slot, CacheStore, CacheValueView};

fn bench_get_hit_inline(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache/get_hit_inline");
    group.throughput(Throughput::Elements(1));

    let mut store = CacheStore::new();
    store.set_bytes(b"tenant:{42}:profile", b"small-value", None, 0);

    group.bench_function("direct", |b| {
        b.iter(|| {
            let value = store.get(black_box(b"tenant:{42}:profile"), 0);
            debug_assert_eq!(value, Some(CacheValueView::Bytes(b"small-value")));
            black_box(value);
        });
    });
    group.finish();
}

fn bench_set_inline_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache/set_inline_churn");
    group.throughput(Throughput::Elements(1));

    let mut store = CacheStore::new();
    group.bench_function("same_key", |b| {
        b.iter(|| {
            store.set_bytes(
                black_box(b"tenant:{42}:counter"),
                black_box(b"1234567890"),
                None,
                0,
            );
        });
    });
    group.finish();
}

fn bench_set_large_reuse(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache/set_large_reuse");
    group.throughput(Throughput::Bytes(256));

    let value = vec![7u8; 256];
    let mut store = CacheStore::new();
    store.set_bytes(b"blob", &value, None, 0);

    group.bench_function("same_key", |b| {
        b.iter(|| {
            store.set_bytes(black_box(b"blob"), black_box(&value), None, 0);
        });
    });
    group.finish();
}

fn bench_resp_get_execute(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache/resp_get_execute");
    group.throughput(Throughput::Elements(1));

    let frame = b"*2\r\n$3\r\nGET\r\n$20\r\ntenant:{42}:profile\r\n";
    let mut store = CacheStore::new();
    store.set_bytes(b"tenant:{42}:profile", b"small-value", None, 0);
    let mut out = Vec::with_capacity(64);

    group.bench_function("parse_and_execute", |b| {
        b.iter(|| {
            out.clear();
            let consumed = execute_frame(&mut store, black_box(frame), 0, &mut out)
                .unwrap()
                .unwrap();
            debug_assert_eq!(consumed, frame.len());
            black_box(&out);
        });
    });
    group.finish();
}

fn bench_redis_slot(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache/redis_slot");
    group.throughput(Throughput::Elements(1));

    group.bench_function("hash_tag", |b| {
        b.iter(|| black_box(redis_slot(black_box(b"tenant:{12345}:sessions"))));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_get_hit_inline,
    bench_set_inline_churn,
    bench_set_large_reuse,
    bench_resp_get_execute,
    bench_redis_slot
);
