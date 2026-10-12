use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation, TabletWrite,
};

#[test]
fn key_ranges_are_half_open_and_support_unbounded_ends() {
    let bounded = KeyRange::new(b"a".to_vec(), Some(b"m".to_vec())).unwrap();
    assert!(bounded.contains(b"a"));
    assert!(bounded.contains(b"l"));
    assert!(!bounded.contains(b"m"));
    assert!(!bounded.contains(b"z"));

    let unbounded = KeyRange::new(b"m".to_vec(), None).unwrap();
    assert!(unbounded.contains(b"m"));
    assert!(unbounded.contains(b"z"));
}

#[test]
fn key_ranges_reject_empty_or_reversed_intervals() {
    assert_eq!(
        KeyRange::new(b"m".to_vec(), Some(b"m".to_vec())).unwrap_err(),
        TabletError::InvalidKeyRange
    );
    assert_eq!(
        KeyRange::new(b"z".to_vec(), Some(b"a".to_vec())).unwrap_err(),
        TabletError::InvalidKeyRange
    );
}

#[test]
fn tablet_split_preserves_complete_non_overlapping_coverage() {
    let source = TabletDescriptor::new(
        TabletId::new(7).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap();

    let split = source
        .plan_split(
            b"m",
            TabletId::new(7).unwrap(),
            TabletId::new(8).unwrap(),
            4,
        )
        .unwrap();

    assert_eq!(split.left.range().start(), b"a");
    assert_eq!(split.left.range().end(), Some(&b"m"[..]));
    assert_eq!(split.right.range().start(), b"m");
    assert_eq!(split.right.range().end(), Some(&b"z"[..]));

    assert!(split.left.range().contains(b"l"));
    assert!(!split.left.range().contains(b"m"));
    assert!(split.right.range().contains(b"m"));
    assert!(!split.right.range().contains(b"z"));
}

#[test]
fn tablet_split_requires_interior_key_new_epoch_and_distinct_children() {
    let source = TabletDescriptor::new(
        TabletId::new(7).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap();

    assert_eq!(
        source
            .plan_split(
                b"a",
                TabletId::new(7).unwrap(),
                TabletId::new(8).unwrap(),
                4,
            )
            .unwrap_err(),
        TabletError::SplitKeyOutsideInterior
    );
    assert_eq!(
        source
            .plan_split(
                b"m",
                TabletId::new(7).unwrap(),
                TabletId::new(8).unwrap(),
                3,
            )
            .unwrap_err(),
        TabletError::EpochNotAdvanced {
            current: 3,
            proposed: 3,
        }
    );
    assert_eq!(
        source
            .plan_split(
                b"m",
                TabletId::new(8).unwrap(),
                TabletId::new(8).unwrap(),
                4,
            )
            .unwrap_err(),
        TabletError::DuplicateChildTablet
    );
}

#[test]
fn tablet_writes_are_epoch_fenced_sequence_checked_and_range_checked() {
    let descriptor = TabletDescriptor::new(
        TabletId::new(11).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"m".to_vec())).unwrap(),
        9,
    )
    .unwrap();

    let write = TabletWrite::prepare(
        &descriptor,
        9,
        41,
        41,
        vec![
            TabletMutation::Put {
                key: b"alpha".to_vec(),
                value: b"1".to_vec(),
            },
            TabletMutation::Delete {
                key: b"beta".to_vec(),
            },
        ],
    )
    .unwrap();

    assert_eq!(write.tablet_id(), TabletId::new(11).unwrap());
    assert_eq!(write.ownership_epoch(), 9);
    assert_eq!(write.expected_previous_sequence(), 41);
    assert_eq!(write.sequence(), 42);

    assert_eq!(
        TabletWrite::prepare(&descriptor, 8, 41, 41, vec![]).unwrap_err(),
        TabletError::StaleEpoch {
            current: 9,
            presented: 8,
        }
    );
    assert_eq!(
        TabletWrite::prepare(&descriptor, 9, 40, 41, vec![]).unwrap_err(),
        TabletError::SequenceMismatch {
            committed: 41,
            expected_previous: 40,
        }
    );
    assert_eq!(
        TabletWrite::prepare(
            &descriptor,
            9,
            41,
            41,
            vec![TabletMutation::Delete {
                key: b"zulu".to_vec(),
            }],
        )
        .unwrap_err(),
        TabletError::KeyOutsideTabletRange
    );
}
