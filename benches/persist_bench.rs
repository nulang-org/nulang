//! Persistence microbenchmarks.
//!
//! These benchmarks measure concrete persistence operations. In particular,
//! checkpoint-size tests must serialize or copy the payload; benchmarking only
//! `vec![0; N].len()` can be optimized away and produces meaningless
//! picosecond-scale "checkpoint" results.
//!
//! Atomic-transition benchmarks deliberately separate the in-memory contract
//! lower bound from the file-backed `synchronous=FULL` path. The latter is the
//! durable-storage signal; the former is useful for locating runtime/digest
//! overhead but must not be presented as durable disk throughput.

use std::collections::HashMap;

use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput};
use nulang::runtime::{
    ActorSnapshot, DurableTransition, JournalEntry, MemoryStore, PersistedValue, PersistenceStore,
    WorkflowEvent, DURABLE_TRANSITION_VERSION,
};

#[cfg(feature = "sqlite")]
use std::path::PathBuf;
#[cfg(feature = "sqlite")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "sqlite")]
use nulang::runtime::LibsqlStore;

fn snapshot_with_payload(payload_bytes: usize) -> ActorSnapshot {
    let mut state = HashMap::new();
    state.insert(
        "payload".to_string(),
        PersistedValue::String("x".repeat(payload_bytes)),
    );
    ActorSnapshot {
        actor_id: 1,
        sequence: 1,
        activation_epoch: 1,
        state,
        waiting_signal: None,
        crdt_snapshot: None,
        crdt_field_map: None,
        schema_name: None,
        authority_tokens: Default::default(),
    }
}

fn empty_snapshot() -> ActorSnapshot {
    ActorSnapshot {
        actor_id: 1,
        sequence: 0,
        activation_epoch: 1,
        state: HashMap::new(),
        waiting_signal: None,
        crdt_snapshot: None,
        crdt_field_map: None,
        schema_name: None,
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
        let encoded = serde_json::to_vec(&snapshot_with_payload(bytes))
            .expect("serialize benchmark snapshot");
        group.throughput(Throughput::Bytes(bytes as u64));
        group.bench_function(label, |b| {
            b.iter(|| {
                let snapshot: ActorSnapshot = serde_json::from_slice(black_box(encoded.as_slice()))
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
    group.bench_function("1000", |b| b.iter(|| black_box(store.read_journal(1))));
    group.finish();
}

fn benchmark_transition(sequence: u64) -> DurableTransition {
    let mut state = HashMap::new();
    state.insert("counter".to_string(), PersistedValue::Int(sequence as i64));
    state.insert(
        "payload".to_string(),
        PersistedValue::String("x".repeat(256)),
    );

    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 1,
        activation_epoch: 1,
        sequence,
        expected_previous_sequence: sequence - 1,
        command: Some(JournalEntry {
            sequence,
            behavior_id: 0,
            payload: vec![PersistedValue::Int(sequence as i64)],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: 1,
            sequence,
            activation_epoch: 1,
            state,
            waiting_signal: None,
            crdt_snapshot: None,
            crdt_field_map: None,
            schema_name: Some("BenchmarkActor".to_string()),
            authority_tokens: Default::default(),
        }),
        workflow_events: vec![WorkflowEvent::Custom {
            sequence,
            replay_id: None,
            name: "Committed".to_string(),
            args: vec![PersistedValue::Int(sequence as i64)],
        }],
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    }
}

fn transition_batch(count: usize) -> Vec<DurableTransition> {
    (1..=count)
        .map(|sequence| benchmark_transition(sequence as u64))
        .collect()
}

/// RFC 0022 contract lower bound using MemoryStore.
///
/// This measures validation, canonical transition hashing, fencing, snapshot /
/// journal / workflow publication, tail update, and transition retention. It
/// intentionally performs no durable I/O and therefore must not be reported as
/// storage durability throughput.
fn bench_atomic_transition_memory(c: &mut Criterion) {
    const TRANSITIONS: usize = 256;
    let mut group = c.benchmark_group("persist/atomic_transition");
    group.throughput(Throughput::Elements(TRANSITIONS as u64));
    group.bench_function("memory_256", |b| {
        b.iter_batched_ref(
            || (MemoryStore::new(), transition_batch(TRANSITIONS)),
            |(store, transitions)| {
                for transition in transitions.drain(..) {
                    black_box(
                        store
                            .commit_transition(transition)
                            .expect("memory atomic transition must commit"),
                    );
                }
                assert_eq!(
                    store
                        .load_durable_tail_position(1)
                        .expect("tail read must succeed")
                        .map(|tail| tail.sequence),
                    Some(TRANSITIONS as u64),
                );
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

#[cfg(feature = "sqlite")]
static ATOMIC_BENCH_FILE_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(feature = "sqlite")]
struct FullSyncAtomicFixture {
    store: Option<LibsqlStore>,
    transitions: Vec<DurableTransition>,
    path: PathBuf,
}

#[cfg(feature = "sqlite")]
impl FullSyncAtomicFixture {
    fn new(count: usize) -> Self {
        let id = ATOMIC_BENCH_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nulang-atomic-transition-bench-{}-{id}.db",
            std::process::id()
        ));
        remove_sqlite_files(&path);
        let store = LibsqlStore::new(&path)
            .expect("file-backed FULL-sync libSQL benchmark store must open");
        Self {
            store: Some(store),
            transitions: transition_batch(count),
            path,
        }
    }
}

#[cfg(feature = "sqlite")]
impl Drop for FullSyncAtomicFixture {
    fn drop(&mut self) {
        // Close the connection before deleting the database/WAL files. With
        // `iter_batched_ref`, this teardown is outside Criterion's timed body.
        self.store.take();
        remove_sqlite_files(&self.path);
    }
}

#[cfg(feature = "sqlite")]
fn remove_sqlite_files(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let base = path.to_string_lossy();
    let _ = std::fs::remove_file(format!("{base}-wal"));
    let _ = std::fs::remove_file(format!("{base}-shm"));
}

/// File-backed RFC 0022 commit path with LibsqlStore's default
/// `SqliteSyncMode::Full` durability.
///
/// Database creation, schema setup, transition construction, and file cleanup
/// are outside timing. Each measured commit still performs the real
/// `BEGIN IMMEDIATE` transaction, canonical digest, SQL writes, WAL commit, and
/// FULL synchronous stable-storage barrier required by this backend.
#[cfg(feature = "sqlite")]
fn bench_atomic_transition_libsql_full(c: &mut Criterion) {
    const TRANSITIONS: usize = 8;
    let mut group = c.benchmark_group("persist/atomic_transition");
    group.throughput(Throughput::Elements(TRANSITIONS as u64));
    group.sample_size(20);
    group.bench_function("libsql_file_full_8", |b| {
        b.iter_batched_ref(
            || FullSyncAtomicFixture::new(TRANSITIONS),
            |fixture| {
                let store = fixture.store.as_mut().expect("benchmark store is open");
                for transition in fixture.transitions.drain(..) {
                    black_box(
                        store
                            .commit_transition(transition)
                            .expect("FULL-sync atomic transition must commit"),
                    );
                }
                assert_eq!(
                    store
                        .load_durable_tail_position(1)
                        .expect("tail read must succeed")
                        .map(|tail| tail.sequence),
                    Some(TRANSITIONS as u64),
                );
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

#[cfg(not(feature = "sqlite"))]
fn bench_atomic_transition_libsql_full(_c: &mut Criterion) {}

criterion_group!(
    benches,
    bench_memory_store,
    bench_checkpoint_json_encode,
    bench_checkpoint_json_decode,
    bench_memory_journal_read,
    bench_atomic_transition_memory,
    bench_atomic_transition_libsql_full
);
