//! Persistence microbenchmarks.
//!
//! These benchmarks measure concrete persistence operations. In particular,
//! checkpoint-size tests must serialize or copy the payload; benchmarking only
//! `vec![0; N].len()` can be optimized away and produces meaningless
//! picosecond-scale "checkpoint" results.

use std::collections::HashMap;

use criterion::{
    black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput,
};
use nulang::runtime::{
    ActorSnapshot, JournalEntry, MemoryStore, PersistedValue, PersistenceStore,
};

fn snapshot_with_payload(payload_bytes: usize) -> ActorSnapshot {
    let mut state = HashMap::new();
    state.insert(
        "payload".to_string(),
        PersistedValue::String("x".repeat(payload_bytes)),
    );
    ActorSnapshot {
        actor_id: 1,
        sequence: 1,
        state,
        waiting_signal: None,
        crdt_snapshot: None,
        crdt_field_map: None,
        authority_tokens: Default::default(),
    }
}

fn empty_snapshot() -> ActorSnapshot {
    ActorSnapshot {
        actor_id: 1,
        sequence: 0,
        state: HashMap::new(),
        waiting_signal: None,
        crdt_snapshot: None,
        crdt_field_map: None,
        authority_tokens: Default::default(),
    }
}

/// In-memory save + load baseline.
///
/// The empty case is retained as fixed store overhead. Sized cases measure the
/// cost of loading/cloning realistic snapshot payloads; snapshot construction is
/// performed in Criterion setup and excluded from the timed body.
fn bench_memory_store(c: &mut Criterion) {
    c.bench_function("persist/memory_store_empty", |b| {
        b.iter_batched(
            || (MemoryStore::new(), empty_snapshot()),
            |(mut store, snapshot)| {
                store.save_snapshot(snapshot).expect("save snapshot");
                black_box(store.load_snapshot(1));
            },
            BatchSize::SmallInput,
        )
    });

    let mut group = c.benchmark_group("persist/memory_store_payload");
    for (label, bytes) in [("1kb", 1024usize), ("1mb", 1024 * 1024)] {
        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &bytes, |b, &bytes| {
            b.iter_batched(
                || (MemoryStore::new(), snapshot_with_payload(bytes)),
                |(mut store, snapshot)| {
                    store.save_snapshot(snapshot).expect("save snapshot");
                    black_box(store.load_snapshot(1));
                },
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

/// JSON snapshot encoding used by the file-backed persistence path.
///
/// Throughput is expressed in logical state payload bytes; JSON framing/tagging
/// adds a small amount of encoded overhead.
fn bench_checkpoint_json_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("persist/checkpoint_json_encode");
    for (label, bytes) in [("1kb", 1024usize), ("1mb", 1024 * 1024)] {
        let snapshot = snapshot_with_payload(bytes);
        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_function(label, |b| {
            b.iter(|| {
                let encoded =
                    serde_json::to_vec(black_box(&snapshot)).expect("serialize benchmark snapshot");
                black_box(encoded);
            })
        });
    }
    group.finish();
}

/// JSON snapshot decode cost for the same payload sizes.
fn bench_checkpoint_json_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("persist/checkpoint_json_decode");
    for (label, bytes) in [("1kb", 1024usize), ("1mb", 1024 * 1024)] {
        let encoded =
            serde_json::to_vec(&snapshot_with_payload(bytes)).expect("serialize benchmark snapshot");
        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_function(label, |b| {
            b.iter(|| {
                let snapshot: ActorSnapshot =
                    serde_json::from_slice(black_box(encoded.as_slice()))
                        .expect("deserialize benchmark snapshot");
                black_box(snapshot);
            })
        });
    }
    group.finish();
}

/// Read/clone a real 1,000-entry in-memory actor journal.
///
/// This replaces the old "event_replay" benchmark that only summed a local
/// integer vector and therefore did not exercise Nulang persistence at all.
fn bench_memory_journal_read(c: &mut Criterion) {
    const EVENTS: usize = 1_000;

    let mut store = MemoryStore::new();
    for sequence in 0..EVENTS {
        store
            .append_journal(
                1,
                JournalEntry {
                    sequence: sequence as u64,
                    behavior_id: 0,
                    payload: vec![PersistedValue::Int(sequence as i64)],
                },
            )
            .expect("append benchmark journal");
    }

    let mut group = c.benchmark_group("persist/memory_journal_read");
    group.throughput(Throughput::Elements(EVENTS as u64));
    group.bench_function("1000", |b| {
        b.iter(|| black_box(store.read_journal(1)))
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_memory_store,
    bench_checkpoint_json_encode,
    bench_checkpoint_json_decode,
    bench_memory_journal_read
);
