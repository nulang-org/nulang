//! Contract tests for the *non-mutating* retention decision layer.
use nulang::database::retention::{Coverage, RetentionBlock, RetentionClass, RetentionPolicy};

fn all_clear() -> RetentionPolicy {
    let mut policy = RetentionPolicy::default();
    for class in [
        RetentionClass::Snapshot,
        RetentionClass::Changefeed,
        RetentionClass::Backup,
        RetentionClass::Replica,
        RetentionClass::Branch,
    ] {
        policy.set_coverage(class, Coverage::None);
    }
    policy
}

#[test]
fn unknown_sources_fail_closed_independently_by_domain() {
    let policy = RetentionPolicy::default();
    assert_eq!(
        policy.mvcc_snapshot_floor(10),
        Err(RetentionBlock::Unknown(vec![RetentionClass::Snapshot]))
    );
    assert_eq!(
        policy.wal_reclaim_bound(Some(8), 10),
        Err(RetentionBlock::Unknown(vec![
            RetentionClass::Changefeed,
            RetentionClass::Backup,
            RetentionClass::Replica,
            RetentionClass::Branch,
        ]))
    );
}

#[test]
fn no_consumers_still_require_a_valid_checkpoint_before_wal_reclaim() {
    let policy = all_clear();
    assert_eq!(policy.mvcc_snapshot_floor(10), Ok(None));
    assert_eq!(
        policy.wal_reclaim_bound(None, 10),
        Err(RetentionBlock::NoCheckpoint)
    );
    assert_eq!(policy.wal_reclaim_bound(Some(8), 10), Ok(8));
    assert_eq!(
        policy.wal_reclaim_bound(Some(11), 10),
        Err(RetentionBlock::CheckpointAhead {
            checkpoint: 11,
            durable_tail: 10
        })
    );
}

#[test]
fn all_wal_cursors_constrain_the_reclaim_bound() {
    let mut policy = all_clear();
    policy.set_coverage(RetentionClass::Changefeed, Coverage::Earliest(9));
    policy.set_coverage(RetentionClass::Backup, Coverage::Earliest(7));
    policy.set_coverage(RetentionClass::Replica, Coverage::Earliest(6));
    policy.set_coverage(RetentionClass::Branch, Coverage::Earliest(8));
    // A consumer still needs the record at sequence 6: only <= 5 may go.
    assert_eq!(policy.wal_reclaim_bound(Some(10), 10), Ok(5));
    policy.set_coverage(RetentionClass::Replica, Coverage::None);
    assert_eq!(policy.wal_reclaim_bound(Some(10), 10), Ok(6));
    // Checkpoint is a separate hard upper bound.
    assert_eq!(policy.wal_reclaim_bound(Some(3), 10), Ok(3));
}

#[test]
fn first_wal_record_pin_halts_log_reclamation() {
    let mut policy = all_clear();
    policy.set_coverage(RetentionClass::Changefeed, Coverage::Earliest(1));
    assert_eq!(policy.wal_reclaim_bound(Some(8), 10), Ok(0));
}

#[test]
fn snapshot_floor_is_not_a_wal_reclaim_permission() {
    let mut policy = all_clear();
    policy.set_coverage(RetentionClass::Snapshot, Coverage::Earliest(3));
    assert_eq!(policy.mvcc_snapshot_floor(10), Ok(Some(3)));
    // Snapshot pin does not, by itself, define a WAL cursor.
    assert_eq!(policy.wal_reclaim_bound(Some(10), 10), Ok(10));
}

#[test]
fn future_or_invalid_requirements_fail_closed() {
    let mut policy = all_clear();
    policy.set_coverage(RetentionClass::Snapshot, Coverage::Earliest(11));
    assert_eq!(
        policy.mvcc_snapshot_floor(10),
        Err(RetentionBlock::OutOfRange {
            source: RetentionClass::Snapshot,
            sequence: 11,
            durable_tail: 10,
        })
    );
    policy.set_coverage(RetentionClass::Snapshot, Coverage::Earliest(0));
    assert_eq!(policy.mvcc_snapshot_floor(10), Ok(Some(0)));

    policy.set_coverage(RetentionClass::Changefeed, Coverage::Earliest(0));
    assert_eq!(
        policy.wal_reclaim_bound(Some(9), 10),
        Err(RetentionBlock::OutOfRange {
            source: RetentionClass::Changefeed,
            sequence: 0,
            durable_tail: 10,
        })
    );
    policy.set_coverage(RetentionClass::Changefeed, Coverage::Earliest(12));
    assert!(matches!(
        policy.wal_reclaim_bound(Some(9), 10),
        Err(RetentionBlock::OutOfRange {
            source: RetentionClass::Changefeed,
            ..
        })
    ));
    // End-cursor (tail+1) is valid: there are no unconsumed WAL entries.
    policy.set_coverage(RetentionClass::Changefeed, Coverage::Earliest(11));
    assert_eq!(policy.wal_reclaim_bound(Some(9), 10), Ok(9));
}

#[test]
fn any_new_uncertainty_revokes_the_decision_to_reclaim() {
    let mut policy = all_clear();
    assert_eq!(policy.wal_reclaim_bound(Some(5), 7), Ok(5));
    policy.set_coverage(RetentionClass::Backup, Coverage::Unknown);
    assert_eq!(
        policy.wal_reclaim_bound(Some(5), 7),
        Err(RetentionBlock::Unknown(vec![RetentionClass::Backup]))
    );
}

#[test]
fn maximum_sequence_does_not_overflow_when_validating_cursors() {
    let mut policy = all_clear();
    policy.set_coverage(RetentionClass::Replica, Coverage::Earliest(u64::MAX));
    assert_eq!(
        policy.wal_reclaim_bound(Some(u64::MAX), u64::MAX),
        Ok(u64::MAX - 1)
    );
}
