//! Persistence benchmarks: checkpoint latency, event replay, store comparison.

use criterion::{black_box, criterion_group, Criterion, Throughput};

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
                crdt_field_map: None,
                authority_tokens: Default::default(),
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

fn bench_durable_change_tail_scan(c: &mut Criterion) {
    use nulang::runtime::{
        scan_durable_changes, DurableChangeCursor, DurableChangeLane, EventEntry, MemoryStore,
        PersistedValue, PersistenceStore,
    };

    const HISTORY: u64 = 100_000;
    const BATCH: usize = 64;
    let mut store = MemoryStore::new();
    for sequence in 1..=HISTORY {
        store
            .append_event(
                7,
                EventEntry {
                    sequence,
                    field_name: "count".to_string(),
                    event_name: "Updated".to_string(),
                    args: vec![],
                    value: PersistedValue::Int(sequence as i64),
                },
            )
            .unwrap();
    }
    let after = DurableChangeCursor {
        sequence: HISTORY - 1_000,
        lane: DurableChangeLane::Event,
        ordinal: 0,
    };

    let mut group = c.benchmark_group("persist/durable_change_scan");
    group.throughput(Throughput::Elements(BATCH as u64));
    group.bench_function("memory_100k_tail_64", |b| {
        b.iter(|| {
            black_box(scan_durable_changes(&store, 7, Some(after), BATCH).unwrap());
        })
    });
    group.finish();
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
    bench_durable_change_tail_scan,
    bench_checkpoint_sizes
);
