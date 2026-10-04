use nulang::durable_time::{
    ClosedTimestamp, ClosedTimestampError, DurableTransitionTime, DurableTransitionTimeError,
    TimedDurableTransition,
};
use nulang::hlc::HlcTimestamp;
use nulang::runtime::{DurableTransition, DURABLE_TRANSITION_VERSION};

fn transition(actor_id: u64, activation_epoch: u64, sequence: u64) -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch,
        sequence,
        expected_previous_sequence: sequence.saturating_sub(1),
        command: None,
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

#[test]
fn durable_transition_time_binds_hlc_to_fenced_transition_identity() {
    let transition = transition(42, 7, 11);
    let timestamp = HlcTimestamp::new(1_000_000, 3);

    let ordering = DurableTransitionTime::for_transition(&transition, timestamp);

    assert_eq!(ordering.actor_id(), 42);
    assert_eq!(ordering.activation_epoch(), 7);
    assert_eq!(ordering.sequence(), 11);
    assert_eq!(ordering.timestamp(), timestamp);
}

#[test]
fn durable_transition_time_is_stable_across_serde_round_trip() {
    let ordering = DurableTransitionTime::for_transition(
        &transition(42, 7, 11),
        HlcTimestamp::new(1_000_000, 3),
    );

    let encoded = serde_json::to_string(&ordering).unwrap();
    let decoded: DurableTransitionTime = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, ordering);
}

#[test]
fn durable_transition_time_rejects_metadata_for_a_different_transition() {
    let persisted = DurableTransitionTime::for_transition(
        &transition(42, 7, 11),
        HlcTimestamp::new(1_000_000, 3),
    );

    let error = persisted.validate_for(&transition(42, 8, 11)).unwrap_err();

    assert_eq!(
        error,
        DurableTransitionTimeError::ActivationEpochMismatch {
            expected: 8,
            actual: 7,
        }
    );
}

#[test]
fn legacy_timed_transition_preserves_the_existing_digest_byte_for_byte() {
    let transition = transition(42, 7, 11);
    let legacy_digest = transition.digest().unwrap();

    let timed = TimedDurableTransition::legacy(transition);

    assert_eq!(timed.timestamp(), None);
    assert_eq!(timed.digest().unwrap(), legacy_digest);
}

#[test]
fn timestamped_transition_digest_is_stable_and_binds_the_supplied_hlc() {
    let timestamp = HlcTimestamp::new(1_000_000, 3);
    let same = HlcTimestamp::new(1_000_000, 3);
    let later = HlcTimestamp::new(1_000_000, 4);

    let first = TimedDurableTransition::with_timestamp(transition(42, 7, 11), timestamp);
    let retry = TimedDurableTransition::with_timestamp(transition(42, 7, 11), same);
    let different = TimedDurableTransition::with_timestamp(transition(42, 7, 11), later);

    assert_eq!(first.timestamp(), Some(timestamp));
    assert_eq!(first.digest().unwrap(), retry.digest().unwrap());
    assert_ne!(first.digest().unwrap(), different.digest().unwrap());
}

#[test]
fn recovered_timed_transition_rejects_mismatched_persisted_identity() {
    let persisted = DurableTransitionTime::for_transition(
        &transition(42, 7, 11),
        HlcTimestamp::new(1_000_000, 3),
    );

    let error =
        TimedDurableTransition::from_parts(transition(99, 7, 11), Some(persisted)).unwrap_err();

    assert_eq!(
        error,
        DurableTransitionTimeError::ActorIdMismatch {
            expected: 99,
            actual: 42,
        }
    );
}

#[test]
fn unopened_closed_timestamp_does_not_authorize_reads() {
    let closed = ClosedTimestamp::default();

    assert_eq!(closed.get(), None);
    assert!(!closed.permits_read(HlcTimestamp::new(0, 0)));
}

#[test]
fn closed_timestamp_advances_monotonically_and_authorizes_prefix_reads() {
    let mut closed = ClosedTimestamp::default();
    let first = HlcTimestamp::new(5_000, 2);
    let second = HlcTimestamp::new(6_000, 0);

    assert!(closed.advance(first).unwrap());
    assert!(!closed.advance(first).unwrap());
    assert!(closed.advance(second).unwrap());

    assert_eq!(closed.get(), Some(second));
    assert!(closed.permits_read(first));
    assert!(closed.permits_read(second));
    assert!(!closed.permits_read(HlcTimestamp::new(6_000, 1)));
}

#[test]
fn closed_timestamp_regression_fails_closed_without_mutating_state() {
    let mut closed = ClosedTimestamp::default();
    let current = HlcTimestamp::new(9_000, 4);
    let attempted = HlcTimestamp::new(9_000, 3);
    closed.advance(current).unwrap();

    let error = closed.advance(attempted).unwrap_err();

    assert_eq!(
        error,
        ClosedTimestampError::Regression { current, attempted }
    );
    assert_eq!(closed.get(), Some(current));
}

#[test]
fn closed_timestamp_serde_round_trip_preserves_unopened_and_advanced_states() {
    let unopened = ClosedTimestamp::default();
    let unopened_json = serde_json::to_string(&unopened).unwrap();
    assert_eq!(
        serde_json::from_str::<ClosedTimestamp>(&unopened_json).unwrap(),
        unopened
    );

    let mut advanced = ClosedTimestamp::default();
    advanced.advance(HlcTimestamp::new(12_345, 6)).unwrap();
    let advanced_json = serde_json::to_string(&advanced).unwrap();
    assert_eq!(
        serde_json::from_str::<ClosedTimestamp>(&advanced_json).unwrap(),
        advanced
    );
}
