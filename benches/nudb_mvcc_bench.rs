//! NuDB MVCC point-read microbenchmarks.
//!
//! The identical harness should be run against the #1437 reverse-scan baseline
//! and the #1446 binary-search candidate. This measures the public tablet path
//! (including BTreeMap key lookup), not just a standalone partition_point.
//!
//! cargo bench --locked --no-default-features --bench bench_main -- nudb/mvcc

use criterion::{black_box, criterion_group, BenchmarkId, Criterion, Throughput};
use nulang::database::tablet::{KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation};

fn tablet_with_versions(versions: usize, tombstone_latest: bool) -> MemoryTablet {
    let descriptor = TabletDescriptor::new(
        TabletId::new(4242).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        7,
    )
    .unwrap();
    let mut tablet = MemoryTablet::new(descriptor);
    for sequence in 1..=versions {
        let mutation = if tombstone_latest && sequence == versions {
            TabletMutation::Delete { key: b"k".to_vec() }
        } else {
            TabletMutation::Put {
                key: b"k".to_vec(),
                value: (sequence as u64).to_le_bytes().to_vec(),
            }
        };
        let write = tablet
            .prepare_write(7, (sequence - 1) as u64, vec![mutation])
            .unwrap();
        tablet.commit(write).unwrap();
    }
    tablet
}

fn bench_point_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb/mvcc_point_read");
    group.throughput(Throughput::Elements(1));
    for versions in [1usize, 8, 64, 512, 4096] {
        let tablet = tablet_with_versions(versions, false);
        for (position, snapshot) in [
            ("latest", versions as u64),
            ("middle", (versions / 2).max(1) as u64),
            ("oldest", 1),
            ("before_first", 0),
        ] {
            group.bench_with_input(
                BenchmarkId::new(versions.to_string(), position),
                &snapshot,
                |b, &snapshot| {
                    b.iter(|| {
                        let value = tablet
                            .read_at(black_box(b"k"), black_box(snapshot))
                            .expect("valid committed snapshot");
                        black_box(value)
                    })
                },
            );
        }
    }
    group.finish();
}

fn bench_mixed_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb/mvcc_mixed_90_latest_10_oldest");
    for versions in [8usize, 64, 512, 4096] {
        let tablet = tablet_with_versions(versions, false);
        let mut snapshots = [versions as u64; 10];
        snapshots[9] = 1;
        group.throughput(Throughput::Elements(snapshots.len() as u64));
        group.bench_function(format!("{versions}_versions"), |b| {
            b.iter(|| {
                for &snapshot in black_box(&snapshots) {
                    let value = tablet
                        .read_at(black_box(b"k"), black_box(snapshot))
                        .expect("valid mixed snapshot");
                    black_box(value);
                }
            });
        });
    }
    group.finish();
}

fn bench_tombstone_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb/mvcc_tombstone");
    group.throughput(Throughput::Elements(1));
    for versions in [8usize, 512, 4096] {
        let tablet = tablet_with_versions(versions, true);
        for (position, snapshot) in [
            ("latest_absent", versions as u64),
            ("previous_present", (versions - 1) as u64),
        ] {
            group.bench_with_input(
                BenchmarkId::new(versions.to_string(), position),
                &snapshot,
                |b, &snapshot| {
                    b.iter(|| {
                        let value = tablet
                            .read_at(black_box(b"k"), black_box(snapshot))
                            .expect("valid tombstone snapshot");
                        black_box(value)
                    })
                },
            );
        }
    }
    group.finish();
}

criterion_group!(benches, bench_point_reads, bench_mixed_reads, bench_tombstone_reads);
