//! Benchmarks of NuDB's public single-node storage paths.
//!
//! Run: cargo bench --locked --no-default-features --bench bench_main -- nudb/
//! These are measurements, not correctness gates. Durable I/O tests must run
//! on the same filesystem and with the same sync semantics for valid comparisons.

use std::fs;
use std::time::Duration;

use criterion::{black_box, criterion_group, BenchmarkId, Criterion, Throughput};
use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation, TabletWrite,
};
use nulang::database::wal::FileWal;

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(42).expect("tablet id"),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).expect("tablet range"),
        1,
    )
    .expect("tablet descriptor")
}

fn tablet_with_versions(version_count: usize) -> MemoryTablet {
    let mut tablet = MemoryTablet::new(descriptor());
    for sequence in 0..version_count {
        let write = tablet
            .prepare_write(
                1,
                sequence as u64,
                vec![TabletMutation::Put {
                    key: b"k".to_vec(),
                    value: (sequence as u64).to_le_bytes().to_vec(),
                }],
            )
            .expect("prepare MVCC history");
        tablet.commit(write).expect("commit MVCC history");
    }
    tablet
}

fn bench_snapshot_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb/snapshot_point_read");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(1));

    for count in [1_usize, 8, 64, 512, 4096] {
        let tablet = tablet_with_versions(count);
        for (label, snapshot) in [
            ("latest", count as u64),
            ("middle", (count as u64 + 1) / 2),
            ("oldest", 1),
            ("before_first", 0),
        ] {
            group.bench_function(BenchmarkId::new(label, count), |b| {
                b.iter(|| {
                    black_box(
                        black_box(&tablet)
                            .read_at(black_box(b"k"), black_box(snapshot))
                            .expect("historical read"),
                    )
                });
            });
        }
    }
    group.finish();
}

fn bench_durable_wal_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb/wal_durable_append");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(3));
    let desc = descriptor();

    for payload_bytes in [128_usize, 4096] {
        let directory = std::env::temp_dir().join(format!(
            "nudb-bench-wal-{}-{payload_bytes}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create WAL benchmark directory");
        let mut wal = FileWal::open(directory.join("data.wal")).expect("open WAL");
        let value = vec![b'v'; payload_bytes];
        group.throughput(Throughput::Bytes(payload_bytes as u64));
        group.bench_function(BenchmarkId::from_parameter(payload_bytes), |b| {
            b.iter(|| {
                let previous = wal.last_sequence();
                let write = TabletWrite::prepare(
                    &desc,
                    1,
                    previous,
                    previous,
                    vec![TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: value.clone(),
                    }],
                )
                .expect("prepare WAL write");
                wal.append_write(black_box(&write))
                    .expect("durable WAL append");
            });
        });
        drop(wal);
        fs::remove_dir_all(directory).expect("remove WAL benchmark directory");
    }
    group.finish();
}

fn bench_checkpoint_publication(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb/checkpoint_publication");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(3));

    for payload_bytes in [64 * 1024_usize, 1024 * 1024] {
        let directory = std::env::temp_dir().join(format!(
            "nudb-bench-checkpoint-{}-{payload_bytes}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create checkpoint benchmark directory");
        let mut tablet = WalBackedTablet::open(descriptor(), directory.join("data.wal"))
            .expect("open WAL-backed tablet");
        let value_bytes = payload_bytes / 128;
        let mutations = (0..128)
            .map(|i| TabletMutation::Put {
                key: format!("key-{i:06}").into_bytes(),
                value: vec![b'v'; value_bytes],
            })
            .collect();
        let write = tablet
            .prepare_write(1, 0, mutations)
            .expect("prepare checkpoint contents");
        tablet.commit(write).expect("commit checkpoint contents");

        group.throughput(Throughput::Bytes(payload_bytes as u64));
        group.bench_function(BenchmarkId::from_parameter(payload_bytes), |b| {
            b.iter(|| tablet.publish_checkpoint().expect("publish checkpoint"));
        });
        drop(tablet);
        fs::remove_dir_all(directory).expect("remove checkpoint benchmark directory");
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_snapshot_reads,
    bench_durable_wal_append,
    bench_checkpoint_publication
);
