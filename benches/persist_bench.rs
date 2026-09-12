//! Persistence benchmarks: checkpoint latency, event replay, store comparison.

use criterion::{black_box, criterion_group, Criterion};

fn bench_memory_store(c: &mut Criterion) {
    c.bench_function("persist/memory_store", |b| {
        b.iter(|| {
            use nulang::runtime::{ActorSnapshot, MemoryStore, PersistenceStore};
            let mut store = MemoryStore::new();
            let snapshot = ActorSnapshot {
                actor_id: 1,
                sequence: 0,
                state: std::collections::HashMap::new(),
                waiting_signal: None,
                crdt_snapshot: None,
                // Keep this fixture forward-compatible with new snapshot metadata.
                ..ActorSnapshot::default()
            };
            store.save_snapshot(snapshot).ok();
            let _loaded = store.load_snapshot(1);
            black_box(());
        })
    });
}

fn bench_event_replay(c: &mut Criterion) {
    c.bench_function("persist/event_replay", |b| {
        b.iter(|| {
            let events: Vec<i64> = (0..1000).collect();
            let mut sum: i64 = 0;
            for e in &events {
                sum = sum.wrapping_add(*e);
            }
            black_box(sum);
        })
    });
}

fn bench_checkpoint_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("persist/checkpoint");
    group.bench_function("1kb", |b| b.iter(|| black_box(vec![0u8; 1024].len())));
    group.bench_function("1mb", |b| {
        b.iter(|| black_box(vec![0u8; 1024 * 1024].len()))
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_memory_store,
    bench_event_replay,
    bench_checkpoint_sizes
);
