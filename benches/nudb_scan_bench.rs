//! Batch versus scalar MVCC reads on the same committed in-memory tablet.
//! Execute: cargo bench --bench bench_main -- nudb/batch_scan
//! Throughput is logical candidate keys, not visible rows.
//! Results are diagnostic only until collected on paired exact-head builds.

use std::num::NonZeroUsize;

use criterion::{black_box, criterion_group, Criterion, Throughput};
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation,
};

fn fixture(count: usize) -> (MemoryTablet, Vec<Vec<u8>>, KeyRange) {
    let owned = KeyRange::new(b"k".to_vec(), Some(b"l".to_vec())).unwrap();
    let descriptor = TabletDescriptor::new(TabletId::new(120).unwrap(), owned, 1).unwrap();
    let mut tablet = MemoryTablet::new(descriptor);
    let keys: Vec<Vec<u8>> = (0..count).map(|index| format!("k{index:05}").into_bytes()).collect();
    let inserts = keys.iter().map(|key| TabletMutation::Put {
        key: key.clone(),
        value: vec![b'x'; 64],
    }).collect();
    let write = tablet.prepare_write(1, 0, inserts).unwrap();
    tablet.commit(write).unwrap();

    // A second sequence creates tombstones and a mixed MVCC history.
    let second = keys.iter().enumerate().filter_map(|(index, key)| {
        if index % 7 == 0 {
            Some(TabletMutation::Delete { key: key.clone() })
        } else if index % 4 == 0 {
            Some(TabletMutation::Put { key: key.clone(), value: vec![b'y'; 64] })
        } else {
            None
        }
    }).collect();
    let write = tablet.prepare_write(1, 1, second).unwrap();
    tablet.commit(write).unwrap();

    let scan = KeyRange::new(b"k00000".to_vec(), Some(b"k99999".to_vec())).unwrap();
    (tablet, keys, scan)
}

fn benchmark_scan(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb/batch_scan");
    for rows in [1024, 16384] {
        let (tablet, keys, range) = fixture(rows);
        group.throughput(Throughput::Elements(rows as u64));
        for snapshot in [1, 2] {
            group.bench_function(format!("scalar_points/{rows}/s{snapshot}"), |b| {
                b.iter(|| {
                    let mut visible = 0usize;
                    for key in &keys {
                        if let Some(value) = tablet.read_at(black_box(key), snapshot).unwrap() {
                            black_box(value);
                            visible += 1;
                        }
                    }
                    black_box(visible)
                });
            });

            for target in [64usize, 256, 1024] {
                group.bench_function(format!("batched_{target}/{rows}/s{snapshot}"), |b| {
                    b.iter(|| {
                        let mut visible = 0usize;
                        let scan = tablet.scan_range_at(&range, snapshot, NonZeroUsize::new(target).unwrap()).unwrap();
                        for batch in scan {
                            for row in batch.rows {
                                black_box(row.key);
                                black_box(row.value);
                                visible += 1;
                            }
                        }
                        black_box(visible)
                    });
                });
            }
        }
    }
    group.finish();
}

criterion_group!(benches, benchmark_scan);
