//! Native infrastructure hot paths, independent of VM/actor scheduling.
use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput};
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation,
};

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(1).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn tablet() -> MemoryTablet {
    MemoryTablet::new(descriptor())
}

fn commit_versions(tablet: &mut MemoryTablet, count: u64) {
    for _ in 0..count {
        let previous = tablet.current_sequence();
        let write = tablet
            .prepare_write(
                1,
                previous,
                vec![TabletMutation::Put {
                    key: b"key".to_vec(),
                    value: (previous + 1).to_le_bytes().to_vec(),
                }],
            )
            .unwrap();
        assert_eq!(tablet.commit(write).unwrap(), previous + 1);
    }
}

fn bench_snapshot_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb/snapshot_point_read");
    group.throughput(Throughput::Elements(1));
    for count in [1_u64, 64, 4096] {
        let mut tablet = tablet();
        commit_versions(&mut tablet, count);
        for (label, sequence) in [("latest", count), ("oldest", 1)] {
            let expected = sequence.to_le_bytes();
            assert_eq!(
                tablet.read_at(b"key", sequence).unwrap(),
                Some(expected.as_slice())
            );
            group.bench_with_input(BenchmarkId::new(label, count), &sequence, |b, snapshot| {
                b.iter(|| {
                    black_box(
                        tablet
                            .read_at(black_box(b"key"), black_box(*snapshot))
                            .unwrap(),
                    )
                });
            });
        }
    }
    group.finish();
}

fn bench_local_commit(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb/memory_prepare_commit");
    group.throughput(Throughput::Elements(1));
    group.bench_function("one_mutation", |b| {
        b.iter_batched(
            tablet,
            |mut tablet| {
                commit_versions(&mut tablet, 1);
                black_box(tablet)
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_snapshot_reads,
    bench_local_commit,
    bench_durable_storage
);

// The storage profile is deliberately opt-in. Never silently benchmark tmpfs.
use nulang::database::store::WalBackedTablet;
use nulang::runtime::{FabricStreamConfig, FileFabricStreamStore};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

struct StorageFixture(PathBuf);

impl StorageFixture {
    fn new(root: &Path) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = root.join(format!(
                "nulang-infra-bench-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("cannot create benchmark fixture: {e}"),
            }
        }
    }
}

impl Drop for StorageFixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove owned benchmark fixture");
    }
}

fn bench_durable_storage(c: &mut Criterion) {
    let Some(root) = std::env::var_os("NULANG_BENCH_STORAGE_DIR") else {
        return;
    };
    let root = PathBuf::from(root);
    assert!(
        root.is_dir(),
        "NULANG_BENCH_STORAGE_DIR must be an existing directory"
    );

    // Keep each sample fixture small and separate setup/cleanup from timing.
    let mut group = c.benchmark_group("nudb/durable_prepare_commit");
    for size in [64_usize, 4096] {
        let payload = vec![7; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("single_write", size),
            &payload,
            |b, payload| {
                b.iter_batched_ref(
                    || {
                        let dir = StorageFixture::new(&root);
                        let store =
                            WalBackedTablet::open(descriptor(), dir.0.join("tablet.wal")).unwrap();
                        // Fields drop in declaration order: close store before removing directory.
                        (store, dir)
                    },
                    |(store, _)| {
                        let write = store
                            .prepare_write(
                                1,
                                0,
                                vec![TabletMutation::Put {
                                    key: b"key".to_vec(),
                                    value: black_box(payload).clone(),
                                }],
                            )
                            .unwrap();
                        assert_eq!(store.commit(write).unwrap(), 1);
                    },
                    BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();

    let mut group = c.benchmark_group("fabric/durable_local_append");
    for size in [64_usize, 4096] {
        let payload = vec![7; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("existing_stream", size),
            &payload,
            |b, payload| {
                b.iter_batched_ref(
                    || {
                        let dir = StorageFixture::new(&root);
                        let mut store = FileFabricStreamStore::open(&dir.0).unwrap();
                        store
                            .create_stream("bench", FabricStreamConfig::default())
                            .unwrap();
                        assert_eq!(store.append("bench", payload).unwrap(), 1);
                        (store, dir)
                    },
                    |(store, _)| {
                        assert_eq!(store.append("bench", black_box(payload)).unwrap(), 2);
                    },
                    BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();

    // Warm, multi-segment replay; compare identical batch sizes at head and tail.
    let dir = StorageFixture::new(&root);
    let mut store = FileFabricStreamStore::open(&dir.0).unwrap();
    store
        .create_stream(
            "replay",
            FabricStreamConfig {
                segment_max_bytes: 4096,
            },
        )
        .unwrap();
    let payload = vec![7; 256];
    for seq in 1..=128 {
        assert_eq!(store.append("replay", &payload).unwrap(), seq);
    }
    let mut group = c.benchmark_group("fabric/local_replay");
    group.throughput(Throughput::Elements(16));
    for (label, start) in [("head", 1), ("tail", 113)] {
        let records = store.read_from("replay", start, 16).unwrap();
        assert_eq!(records.len(), 16);
        for (offset, record) in records.iter().enumerate() {
            assert_eq!(record.sequence, start + offset as u64);
            assert_eq!(record.payload, payload);
        }
        group.bench_with_input(BenchmarkId::new(label, 128), &start, |b, start| {
            b.iter(|| black_box(store.read_from("replay", black_box(*start), 16).unwrap()));
        });
    }
    group.finish();
}
