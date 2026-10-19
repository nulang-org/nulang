use nulang_numeric::{NumericBuffer, ViewError};

#[test]
fn empty_buffers_and_end_slices_are_valid() {
    let empty = NumericBuffer::<i64>::new(vec![]);
    assert!(empty.is_empty());
    assert_eq!(empty.view(0..0).unwrap().as_slice(), &[]);

    let xs = NumericBuffer::new(vec![1_i64, 2, 3]);
    assert_eq!(xs.view(3..3).unwrap().as_slice(), &[]);
}

#[test]
fn read_only_views_share_allocation_and_keep_owner_alive() {
    let view = {
        let values = NumericBuffer::new(vec![10_i64, 20, 30, 40]);
        let view = values.view(1..3).unwrap();
        assert_eq!(view.as_slice().as_ptr(), values.as_slice()[1..].as_ptr());
        view
    };
    assert_eq!(view.as_slice(), &[20, 30]);
    assert_eq!(view.get(0), Some(&20));
    assert_eq!(view.get(2), None);
}

#[test]
fn nested_views_retain_correct_offset_and_length() {
    let values = NumericBuffer::new(vec![1_i64, 2, 3, 4, 5]);
    let middle = values.view(1..5).unwrap();
    assert_eq!(middle.view(1..3).unwrap().as_slice(), &[3, 4]);
}

#[test]
fn invalid_ranges_never_truncate_or_overflow() {
    let values = NumericBuffer::new(vec![7_i64, 8, 9]);
    for range in [4..4, 2..1, 0..4, usize::MAX..usize::MAX] {
        assert!(matches!(
            values.view(range),
            Err(ViewError::InvalidRange { .. })
        ));
    }
    let subview = values.view(1..3).unwrap();
    assert!(matches!(
        subview.view(0..3),
        Err(ViewError::InvalidRange { .. })
    ));
}

#[test]
fn floating_point_views_preserve_bits_without_computation() {
    let values = NumericBuffer::new(vec![-0.0_f64, f64::NAN, f64::INFINITY]);
    let view = values.view(0..3).unwrap();
    assert_eq!(view.get(0).unwrap().to_bits(), (-0.0_f64).to_bits());
    assert!(view.get(1).unwrap().is_nan());
    assert_eq!(view.get(2), Some(&f64::INFINITY));
}
