use criterion::{black_box, criterion_group, BenchmarkId, Criterion, Throughput};
use nulang::database::compaction::compact_tablet_sstables_to_v2;
use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation, TabletWrite,
};
use nulang::database::wal::FileWal;
use nulang::database::wal_batch::BinaryBatchWal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_BENCH: AtomicU64 = AtomicU64::new(1);
const READ_KEYS: usize = 1_024;
const READ_ROUNDS: usize = 8;

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(7001).unwrap(),
        KeyRange::new(Vec::new(), None).unwrap(),
        1,
    )
    .unwrap()
}

fn read_descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(7002).unwrap(),
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

fn bench_read_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_read_bench_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_BENCH.fetch_add(1, Ordering::Relaxed)
    ))
}

fn cleanup_read_path(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("checkpoint"));
    let _ = std::fs::remove_file(path.with_extension("manifest"));
    let _ = std::fs::remove_dir_all(path.with_extension("sstables"));
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

struct ReadBenchCase {
    path: PathBuf,
    tablet: Option<WalBackedTablet>,
    hit: Vec<u8>,
    miss_inside_range: Vec<u8>,
}

impl ReadBenchCase {
    fn resident() -> Self {
        let path = bench_read_path("resident");
        let tablet = populate_read_tablet(&path, false);
        Self::new(path, tablet)
    }

    fn overlapping_v2() -> Self {
        let path = bench_read_path("v2-overlap");
        let tablet = populate_read_tablet(&path, true);
        Self::new(path, tablet)
    }

    fn compacted_v2() -> Self {
        let path = bench_read_path("v2-compacted");
        let tablet = populate_read_tablet(&path, true);
        drop(tablet);
        assert!(compact_tablet_sstables_to_v2(&read_descriptor(), &path).unwrap());
        let tablet = WalBackedTablet::open(read_descriptor(), &path).unwrap();
        Self::new(path, tablet)
    }

    fn new(path: PathBuf, tablet: WalBackedTablet) -> Self {
        Self {
            path,
            tablet: Some(tablet),
            hit: b"key-00000512".to_vec(),
            // This key is lexicographically between key-00000512 and
            // key-00000513, so all overlapping SSTables admit it by range but
            // the key itself is absent. It is the useful pre-Bloom baseline.
            miss_inside_range: b"key-00000512x".to_vec(),
        }
    }
}

impl Drop for ReadBenchCase {
    fn drop(&mut self) {
        self.tablet.take();
        cleanup_read_path(&self.path);
    }
}

fn populate_read_tablet(path: &Path, flush_each_round: bool) -> WalBackedTablet {
    cleanup_read_path(path);
    let mut tablet = WalBackedTablet::open(read_descriptor(), path).unwrap();

    for round in 0..READ_ROUNDS {
        let mut previous = tablet.current_sequence();
        let mut writes = Vec::with_capacity(READ_KEYS);
        for key_index in 0..READ_KEYS {
            let write = TabletWrite::prepare(
                tablet.descriptor(),
                1,
                previous,
                previous,
                vec![TabletMutation::Put {
                    key: format!("key-{key_index:08}").into_bytes(),
                    value: vec![round as u8; 128],
                }],
            )
            .unwrap();
            previous = write.sequence();
            writes.push(write);
        }
        tablet.commit_batch(writes).unwrap();

        if flush_each_round {
            let bytes = tablet.mutable_memtable_bytes();
            assert!(bytes > 0);
            assert!(tablet.rotate_memtable_if_bytes_at_least(bytes));
            assert!(tablet.flush_oldest_immutable_to_sstable().unwrap());
        }
    }

    tablet
}

fn point_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb_point_reads");
    group.throughput(Throughput::Elements(1));

    let resident = ReadBenchCase::resident();
    let overlapping = ReadBenchCase::overlapping_v2();
    let compacted = ReadBenchCase::compacted_v2();

    group.bench_function("resident_hit", |b| {
        let tablet = resident.tablet.as_ref().unwrap();
        b.iter(|| black_box(tablet.read_latest(black_box(&resident.hit)).unwrap()))
    });
    group.bench_function("resident_miss_inside_range", |b| {
        let tablet = resident.tablet.as_ref().unwrap();
        b.iter(|| {
            black_box(
                tablet
                    .read_latest(black_box(&resident.miss_inside_range))
                    .unwrap(),
            )
        })
    });
    group.bench_function("v2_8_overlapping_hit", |b| {
        let tablet = overlapping.tablet.as_ref().unwrap();
        b.iter(|| black_box(tablet.read_latest(black_box(&overlapping.hit)).unwrap()))
    });
    group.bench_function("v2_8_overlapping_miss_inside_range", |b| {
        let tablet = overlapping.tablet.as_ref().unwrap();
        b.iter(|| {
            black_box(
                tablet
                    .read_latest(black_box(&overlapping.miss_inside_range))
                    .unwrap(),
            )
        })
    });
    group.bench_function("v2_compacted_hit", |b| {
        let tablet = compacted.tablet.as_ref().unwrap();
        b.iter(|| black_box(tablet.read_latest(black_box(&compacted.hit)).unwrap()))
    });
    group.bench_function("v2_compacted_miss_inside_range", |b| {
        let tablet = compacted.tablet.as_ref().unwrap();
        b.iter(|| {
            black_box(
                tablet
                    .read_latest(black_box(&compacted.miss_inside_range))
                    .unwrap(),
            )
        })
    });

    group.finish();
}

criterion_group!(benches, batch_wal, canonical_group_commit, point_reads);
