//! Batch versus scalar MVCC reads on the same committed in-memory tablet.
//! Execute: cargo bench --bench bench_main -- nudb/batch_scan
//! Throughput is logical candidate keys, not visible rows.
//! Results are diagnostic only until collected on paired exact-head builds.

use std::num::NonZeroUsize;

use criterion::{black_box, criterion_group, Criterion, Throughput};
use nulang::database::selection::SelectionBitmap;
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

/// Compare full scalar predicate evaluation with a batch-local selection
/// bitmap pipeline. Bitmap generation is presently row-wise and may be slower
/// for a single filter; the purpose is to measure its downstream composition
/// overhead before introducing encoded-page pushdown or SIMD operators.
fn benchmark_selection(c: &mut Criterion) {
    let mut group = c.benchmark_group("nudb/selection");
    let (tablet, _keys, range) = fixture(16_384);
    group.throughput(Throughput::Elements(16_384));

    for target in [64, 1024] {
        let batches: Vec<_> = tablet
            .scan_range_at(&range, 2, NonZeroUsize::new(target).unwrap())
            .unwrap()
            .collect();

        group.bench_function(format!("scalar/{target}"), |b| {
            b.iter(|| {
                let mut matches = 0;
                for batch in &batches {
                    for row in &batch.rows {
                        if black_box(row.key.ends_with(b"0")) && row.value.starts_with(b"y") {
                            black_box(row.value);
                            matches += 1;
                        }
                    }
                }
                black_box(matches)
            });
        });

        group.bench_function(format!("bitmap_and/{target}"), |b| {
            b.iter(|| {
                let mut matches = 0;
                for batch in &batches {
                    let by_key = SelectionBitmap::from_predicate(batch, |row| {
                        row.key.ends_with(b"0")
                    });
                    let by_value = SelectionBitmap::from_predicate(batch, |row| {
                        row.value.starts_with(b"y")
                    });
                    let selected = by_key.and(&by_value).unwrap();
                    for index in selected.selected_indices() {
                        black_box(batch.rows[index].value);
                        matches += 1;
                    }
                }
                black_box(matches)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, benchmark_scan, benchmark_selection);
