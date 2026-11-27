//! Native infrastructure hot paths, independent of VM/actor scheduling.
use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput};
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation,
};

fn tablet() -> MemoryTablet {
    MemoryTablet::new(
        TabletDescriptor::new(
            TabletId::new(1).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            1,
        )
        .unwrap(),
    )
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

criterion_group!(benches, bench_snapshot_reads, bench_local_commit);
