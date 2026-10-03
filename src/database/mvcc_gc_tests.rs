use super::mvcc_gc::{
    plan_rows_for_floor, plan_versions_for_floor, retain_versions_for_floor, MvccRetentionError,
};
use super::tablet::{TabletSnapshotRow, VersionedValue};

fn put(sequence: u64, value: &[u8]) -> VersionedValue {
    VersionedValue {
        sequence,
        value: Some(value.to_vec()),
    }
}

fn tombstone(sequence: u64) -> VersionedValue {
    VersionedValue {
        sequence,
        value: None,
    }
}

fn row(key: &[u8], versions: Vec<VersionedValue>) -> TabletSnapshotRow {
    TabletSnapshotRow {
        key: key.to_vec(),
        versions,
    }
}

fn visible_value(versions: &[VersionedValue], snapshot: u64) -> Option<&[u8]> {
    versions
        .iter()
        .rev()
        .find(|version| version.sequence <= snapshot)
        .and_then(|version| version.value.as_deref())
}

fn visible_across_authorities<'a>(
    retained: &'a [VersionedValue],
    stale: &'a [VersionedValue],
    snapshot: u64,
) -> Option<&'a [u8]> {
    retained
        .iter()
        .chain(stale)
        .filter(|version| version.sequence <= snapshot)
        .max_by_key(|version| version.sequence)
        .and_then(|version| version.value.as_deref())
}

#[test]
fn retention_keeps_newest_baseline_below_floor_and_every_version_at_or_above_it() {
    let versions = vec![
        put(1, b"one"),
        put(2, b"two"),
        tombstone(5),
        put(9, b"nine"),
    ];

    let retained = retain_versions_for_floor(&versions, 5).unwrap();
    assert_eq!(
        retained,
        vec![put(2, b"two"), tombstone(5), put(9, b"nine")]
    );
}

#[test]
fn floor_zero_retains_complete_history() {
    let versions = vec![put(1, b"one"), tombstone(3), put(7, b"seven")];
    assert_eq!(
        retain_versions_for_floor(&versions, 0).unwrap(),
        versions
    );
}

#[test]
fn floor_after_latest_version_retains_only_latest_baseline() {
    let versions = vec![put(1, b"one"), put(4, b"four"), tombstone(8)];
    assert_eq!(
        retain_versions_for_floor(&versions, 20).unwrap(),
        vec![tombstone(8)]
    );
}

#[test]
fn baseline_tombstone_is_never_dropped_by_version_retention() {
    let versions = vec![put(1, b"one"), tombstone(4), put(10, b"ten")];
    let retained = retain_versions_for_floor(&versions, 8).unwrap();

    assert_eq!(retained, vec![tombstone(4), put(10, b"ten")]);
    assert!(retained[0].value.is_none());
}

#[test]
fn retained_history_preserves_reads_for_every_snapshot_at_or_above_floor() {
    let versions = vec![
        put(1, b"one"),
        put(3, b"three"),
        tombstone(6),
        put(8, b"eight"),
        tombstone(11),
        put(14, b"fourteen"),
    ];

    for floor in 0..=16 {
        let retained = retain_versions_for_floor(&versions, floor).unwrap();
        for snapshot in floor..=18 {
            assert_eq!(
                visible_value(&retained, snapshot),
                visible_value(&versions, snapshot),
                "read changed at floor {floor}, snapshot {snapshot}"
            );
        }
    }
}

#[test]
fn retention_plan_marks_only_versions_older_than_the_retained_barrier_obsolete() {
    let versions = vec![
        put(1, b"one"),
        put(2, b"two"),
        tombstone(5),
        put(9, b"nine"),
    ];
    let plan = plan_versions_for_floor(&versions, 8).unwrap();

    assert_eq!(plan.barrier_sequence(), 5);
    assert_eq!(plan.obsolete(), &[put(1, b"one"), put(2, b"two")]);
    assert_eq!(plan.retained(), &[tombstone(5), put(9, b"nine")]);
    assert!(plan
        .obsolete()
        .iter()
        .all(|version| version.sequence < plan.barrier_sequence()));
}

#[test]
fn retained_barrier_masks_any_subset_of_stale_obsolete_authority() {
    let versions = vec![
        put(1, b"one"),
        tombstone(3),
        put(4, b"four"),
        tombstone(7),
        put(10, b"ten"),
    ];
    let floor = 9;
    let plan = plan_versions_for_floor(&versions, floor).unwrap();
    let obsolete = plan.obsolete();

    for mask in 0..(1_usize << obsolete.len()) {
        let stale: Vec<VersionedValue> = obsolete
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1_usize << index) != 0)
            .map(|(_, version)| version.clone())
            .collect();

        for snapshot in floor..=12 {
            assert_eq!(
                visible_across_authorities(plan.retained(), &stale, snapshot),
                visible_value(&versions, snapshot),
                "stale authority changed read for mask {mask}, snapshot {snapshot}"
            );
        }
    }
}

#[test]
fn tombstone_barrier_prevents_resurrection_from_stale_lower_authority() {
    let versions = vec![put(1, b"one"), put(2, b"two"), tombstone(6)];
    let plan = plan_versions_for_floor(&versions, 10).unwrap();

    assert_eq!(plan.retained(), &[tombstone(6)]);
    assert_eq!(plan.obsolete(), &[put(1, b"one"), put(2, b"two")]);
    assert_eq!(
        visible_across_authorities(plan.retained(), plan.obsolete(), 10),
        None
    );
}

#[test]
fn row_plan_prunes_each_key_independently_and_reports_exact_statistics() {
    let rows = vec![
        row(
            b"a",
            vec![put(1, b"a1"), put(4, b"a4"), tombstone(7), put(10, b"a10")],
        ),
        row(b"b", vec![put(2, b"b2"), put(8, b"b8")]),
        row(b"c", vec![tombstone(3)]),
    ];

    let plan = plan_rows_for_floor(&rows, 9).unwrap();
    assert_eq!(
        plan.rows(),
        &[
            row(b"a", vec![tombstone(7), put(10, b"a10")]),
            row(b"b", vec![put(8, b"b8")]),
            row(b"c", vec![tombstone(3)]),
        ]
    );
    assert_eq!(plan.stats().row_count(), 3);
    assert_eq!(plan.stats().versions_before(), 7);
    assert_eq!(plan.stats().versions_retained(), 4);
    assert_eq!(plan.stats().versions_obsolete(), 3);
    assert_eq!(plan.stats().tombstone_barriers(), 2);
}

#[test]
fn row_plan_at_floor_zero_is_a_noop_with_zero_obsolete_versions() {
    let rows = vec![
        row(b"a", vec![put(1, b"a1"), tombstone(3)]),
        row(b"b", vec![put(2, b"b2")]),
    ];

    let plan = plan_rows_for_floor(&rows, 0).unwrap();
    assert_eq!(plan.rows(), rows.as_slice());
    assert_eq!(plan.stats().versions_before(), 3);
    assert_eq!(plan.stats().versions_retained(), 3);
    assert_eq!(plan.stats().versions_obsolete(), 0);
}

#[test]
fn row_plan_accepts_an_empty_row_set_as_an_empty_maintenance_plan() {
    let plan = plan_rows_for_floor(&[], 5).unwrap();
    assert!(plan.rows().is_empty());
    assert_eq!(plan.stats().row_count(), 0);
    assert_eq!(plan.stats().versions_before(), 0);
    assert_eq!(plan.stats().versions_retained(), 0);
    assert_eq!(plan.stats().versions_obsolete(), 0);
    assert_eq!(plan.stats().tombstone_barriers(), 0);
}

#[test]
fn retention_rejects_empty_duplicate_unsorted_and_zero_sequence_histories() {
    assert_eq!(
        retain_versions_for_floor(&[], 1).unwrap_err(),
        MvccRetentionError::EmptyHistory
    );
    assert_eq!(
        retain_versions_for_floor(&[put(2, b"two"), put(2, b"duplicate")], 2).unwrap_err(),
        MvccRetentionError::SequencesNotStrictlyIncreasing
    );
    assert_eq!(
        retain_versions_for_floor(&[put(3, b"three"), put(2, b"two")], 2).unwrap_err(),
        MvccRetentionError::SequencesNotStrictlyIncreasing
    );
    assert_eq!(
        retain_versions_for_floor(&[put(0, b"zero")], 0).unwrap_err(),
        MvccRetentionError::ZeroSequence
    );
}
