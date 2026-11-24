use criterion::{criterion_group, BenchmarkId, Criterion, Throughput};
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation, TabletWrite,
};
use nulang::database::wal::FileWal;
use nulang::database::wal_batch::BinaryBatchWal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_BENCH: AtomicU64 = AtomicU64::new(1);

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(7001).unwrap(),
        KeyRange::new(Vec::new(), None).unwrap(),
        1,
    )
    .unwrap()
}

fn bench_wal_path(batch: usize) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_wal_bench_{batch}_{}_{}.wal",
        std::process::id(),
        NEXT_BENCH.fetch_add(1, Ordering::Relaxed)
    ))
}

struct BenchCase {
    path: PathBuf,
    wal: BinaryBatchWal,
    writes: Vec<TabletWrite>,
}

impl BenchCase {
    fn new(batch_size: usize) -> Self {
        let path = bench_wal_path(batch_size);
        let _ = std::fs::remove_file(&path);
        let wal = BinaryBatchWal::open(&path).unwrap();
        let mut tablet = MemoryTablet::new(descriptor());
        let mut writes = Vec::with_capacity(batch_size);

        for i in 0..batch_size {
            let previous = tablet.current_sequence();
            let write = tablet
                .prepare_write(
                    1,
                    previous,
                    vec![TabletMutation::Put {
                        key: format!("key-{i:08}").into_bytes(),
                        value: vec![0x5a; 128],
                    }],
                )
                .unwrap();
            tablet.commit(write.clone()).unwrap();
            writes.push(write);
        }

        Self { path, wal, writes }
    }
}

impl Drop for BenchCase {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn batch_wal(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb_binary_batch_wal");

    for batch_size in [1_usize, 16, 64, 256] {
        group.throughput(Throughput::Elements(batch_size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            &batch_size,
            |b, &batch_size| {
                b.iter_batched_ref(
                    || BenchCase::new(batch_size),
                    |case| case.wal.append_batch(&case.writes).unwrap(),
                    criterion::BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}

struct JsonWalBenchCase {
    path: PathBuf,
    wal: FileWal,
    writes: Vec<TabletWrite>,
}

impl JsonWalBenchCase {
    fn new(batch_size: usize) -> Self {
        let path = bench_wal_path(batch_size);
        let _ = std::fs::remove_file(&path);
        let wal = FileWal::open(&path).unwrap();
        let mut tablet = MemoryTablet::new(descriptor());
        let mut writes = Vec::with_capacity(batch_size);

        for i in 0..batch_size {
            let previous = tablet.current_sequence();
            let write = tablet
                .prepare_write(
                    1,
                    previous,
                    vec![TabletMutation::Put {
                        key: format!("json-key-{i:08}").into_bytes(),
                        value: vec![0x5a; 128],
                    }],
                )
                .unwrap();
            tablet.commit(write.clone()).unwrap();
            writes.push(write);
        }

        Self { path, wal, writes }
    }
}

impl Drop for JsonWalBenchCase {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn canonical_group_commit(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb_nudbwal3_group_commit");

    for batch_size in [1_usize, 16, 64, 256] {
        group.throughput(Throughput::Elements(batch_size as u64));
        group.bench_with_input(
            BenchmarkId::new("group_sync", batch_size),
            &batch_size,
            |b, &batch_size| {
                b.iter_batched_ref(
                    || JsonWalBenchCase::new(batch_size),
                    |case| case.wal.append_batch(&case.writes).unwrap(),
                    criterion::BatchSize::SmallInput,
                )
            },
        );
        group.bench_with_input(
            BenchmarkId::new("serial_sync", batch_size),
            &batch_size,
            |b, &batch_size| {
                b.iter_batched_ref(
                    || JsonWalBenchCase::new(batch_size),
                    |case| {
                        for write in &case.writes {
                            case.wal.append_write(write).unwrap();
                        }
                    },
                    criterion::BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}

criterion_group!(benches, batch_wal, canonical_group_commit);
