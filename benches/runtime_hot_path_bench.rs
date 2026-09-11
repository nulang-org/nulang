//! Runtime hot-path microbenchmarks.
//!
//! These isolate the mechanisms that dominate actor turns before broader
//! runtime benchmarks add VM dispatch, behavior execution, persistence, and
//! GC coordination noise.

use criterion::{black_box, criterion_group, BenchmarkId, Criterion, Throughput};
use nulang::iso_arena::IsoArena;
use nulang::runtime::{ActorHeap, Mailbox, Message, MessagePriority, TypeTag};
use nulang::vm::Value;
use std::sync::Arc;

const HEAP_BYTES: usize = 64 * 1024;

fn bench_allocator_hot_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_hot_path/allocator");

    for payload_size in [16usize, 64, 256] {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("actor_heap_alloc_free", payload_size),
            &payload_size,
            |b, &size| {
                let mut heap = ActorHeap::new(HEAP_BYTES);
                b.iter(|| {
                    let ptr = heap
                        .alloc(black_box(size), TypeTag::Record)
                        .expect("ActorHeap allocation");
                    black_box(ptr);
                    // SAFETY: `ptr` was allocated by this exact heap in this
                    // iteration and is not retained after the call.
                    unsafe { heap.free(ptr) };
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("iso_arena_alloc_reset", payload_size),
            &payload_size,
            |b, &size| {
                let mut arena = IsoArena::new();
                b.iter(|| {
                    let ptr = arena
                        .alloc(black_box(size), TypeTag::Record)
                        .expect("IsoArena allocation");
                    black_box(ptr);
                    arena.reset();
                });
            },
        );
    }

    group.throughput(Throughput::Elements(64));
    group.bench_function("iso_arena_batch_64_alloc_reset", |b| {
        let mut arena = IsoArena::new();
        b.iter(|| {
            for _ in 0..64 {
                black_box(
                    arena
                        .alloc(32, TypeTag::Record)
                        .expect("IsoArena allocation"),
                );
            }
            arena.reset();
        });
    });

    group.finish();
}

fn message(payload: &Arc<Vec<Value>>) -> Message {
    Message {
        behavior_id: 1,
        payload: Arc::clone(payload),
        sender: 42,
        priority: MessagePriority::Normal,
        trace_id: None,
    }
}

fn bench_mailbox_hot_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("runtime_hot_path/mailbox");
    group.throughput(Throughput::Elements(1));

    group.bench_function("local_push_pop", |b| {
        let payload = Arc::new(vec![Value::int(1)]);
        let mut mailbox = Mailbox::new(0);
        b.iter(|| {
            mailbox
                .push_local(message(&payload))
                .expect("unbounded local mailbox push");
            black_box(mailbox.pop().expect("message just pushed"));
        });
    });

    group.bench_function("segqueue_push_pop", |b| {
        let payload = Arc::new(vec![Value::int(1)]);
        let mut mailbox = Mailbox::new(0);
        b.iter(|| {
            mailbox
                .push(message(&payload))
                .expect("unbounded concurrent mailbox push");
            black_box(mailbox.pop().expect("message just pushed"));
        });
    });

    group.bench_function("selective_receive_match_hit", |b| {
        let payload = Arc::new(vec![Value::int(1)]);
        let mut mailbox = Mailbox::new(0);
        b.iter(|| {
            mailbox
                .push_local(message(&payload))
                .expect("unbounded local mailbox push");
            let matched = mailbox
                .receive_match(black_box(&[1]))
                .expect("matching behavior id");
            black_box(matched);
            mailbox.commit_receive_match();
        });
    });

    group.finish();
}

criterion_group!(benches, bench_allocator_hot_paths, bench_mailbox_hot_paths);
