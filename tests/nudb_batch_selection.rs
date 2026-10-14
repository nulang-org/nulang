//! Batch predicate selections must agree with scalar MVCC reads.
//! A selection bitmap indexes visible rows within exactly one scan batch.

use std::num::NonZeroUsize;

use nulang::database::selection::{SelectionBitmap, SelectionError};
use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation, TabletScanBatch,
    TabletScanRow,
};

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(151).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    ).unwrap()
}

fn insert(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

#[test]
fn selection_bitmaps_preserve_tails_and_boolean_algebra_across_word_boundaries() {
    // Exercise exact 64-bit boundaries, sparse tails and the zero-length case.
    for len in [0, 1, 63, 64, 65, 127, 128, 129, 257] {
        let rows: Vec<_> = (0..len)
            .map(|i| TabletScanRow {
                key: if i % 2 == 0 { b"even".as_slice() } else { b"odd".as_slice() },
                value: if i % 3 == 0 { b"three".as_slice() } else { b"other".as_slice() },
            })
            .collect();
        let batch = TabletScanBatch { rows };
        let a = SelectionBitmap::from_predicate(&batch, |row| row.key == b"even");
        let b = SelectionBitmap::from_predicate(&batch, |row| row.value == b"three");

        let intersection = a.and(&b).unwrap();
        let union = a.or(&b).unwrap();
        assert_eq!(a.len(), len);
        assert_eq!(b.len(), len);

        let expected_intersection: Vec<usize> = (0..len).filter(|i| i % 2 == 0 && i % 3 == 0).collect();
        let expected_union: Vec<usize> = (0..len).filter(|i| i % 2 == 0 || i % 3 == 0).collect();
        assert_eq!(intersection.selected_indices().collect::<Vec<_>>(), expected_intersection);
        assert_eq!(union.selected_indices().collect::<Vec<_>>(), expected_union);
        assert_eq!(intersection.count_ones(), expected_intersection.len());
        assert_eq!(union.count_ones(), expected_union.len());
        assert_eq!(a.selected_indices().count(), a.count_ones());
        assert_eq!(b.selected_indices().count(), b.count_ones());
        assert_eq!(a.get(len), None, "out of range access must never alias padding bits");

        for (i, row) in batch.rows.iter().enumerate() {
            assert_eq!(a.get(i), Some(row.key == b"even"));
            assert_eq!(b.get(i), Some(row.value == b"three"));
        }
    }
}

#[test]
fn selections_reject_cross_batch_length_mismatch() {
    let small = TabletScanBatch {
        rows: vec![TabletScanRow { key: b"a", value: b"1" }],
    };
    let large = TabletScanBatch {
        rows: vec![
            TabletScanRow { key: b"a", value: b"1" },
            TabletScanRow { key: b"b", value: b"2" },
        ],
    };
    let one = SelectionBitmap::from_predicate(&small, |_| true);
    let two = SelectionBitmap::from_predicate(&large, |_| true);

    assert_eq!(one.and(&two), Err(SelectionError::LengthMismatch { left: 1, right: 2 }));
    assert_eq!(two.or(&one), Err(SelectionError::LengthMismatch { left: 2, right: 1 }));

    // Same shape is not enough: physical positions belong to their own batch.
    let another_small = TabletScanBatch {
        rows: vec![TabletScanRow { key: b"other", value: b"3" }],
    };
    let other_one = SelectionBitmap::from_predicate(&another_small, |_| true);
    assert_eq!(one.and(&other_one), Err(SelectionError::DifferentBatch));
    assert_eq!(other_one.or(&one), Err(SelectionError::DifferentBatch));
}

#[test]
fn selections_match_scalar_reference_across_mvcc_versions_and_batch_sizes() {
    let mut tablet = MemoryTablet::new(descriptor());
    let keys: Vec<_> = (0..130).map(|i| format!("k{i:03}").into_bytes()).collect();
    let write = tablet.prepare_write(1, 0, keys.iter().enumerate().map(|(i, key)| {
        insert(key, if i % 2 == 0 { b"active" } else { b"inactive" })
    }).collect()).unwrap();
    tablet.commit(write).unwrap();

    let write = tablet.prepare_write(1, 1, keys.iter().enumerate().filter_map(|(i, key)| {
        if i % 11 == 0 {
            Some(TabletMutation::Delete { key: key.clone() })
        } else if i % 5 == 0 {
            Some(insert(key, b"active-new"))
        } else {
            None
        }
    }).collect()).unwrap();
    tablet.commit(write).unwrap();

    let range = KeyRange::new(b"k000".to_vec(), Some(b"k130".to_vec())).unwrap();
    for snapshot in [0, 1, 2] {
        let expected: Vec<_> = keys.iter().filter_map(|key| {
            let value = tablet.read_at(key, snapshot).unwrap()?;
            (value.starts_with(b"active") && key.ends_with(b"0"))
                .then(|| (key.clone(), value.to_vec()))
        }).collect();

        for target in [1, 7, 64, 65, 256] {
            let mut actual = Vec::new();
            let scan = tablet.scan_range_at(&range, snapshot, NonZeroUsize::new(target).unwrap()).unwrap();
            for batch in scan {
                let by_value = SelectionBitmap::from_predicate(&batch, |row| row.value.starts_with(b"active"));
                let by_key = SelectionBitmap::from_predicate(&batch, |row| row.key.ends_with(b"0"));
                let both = by_value.and(&by_key).unwrap();
                assert_eq!(both.count_ones(), both.selected_indices().count());
                for index in both.selected_indices() {
                    let row = batch.rows[index];
                    actual.push((row.key.to_vec(), row.value.to_vec()));
                }
            }
            assert_eq!(actual, expected, "snapshot={snapshot}, target={target}");
        }
    }
}
