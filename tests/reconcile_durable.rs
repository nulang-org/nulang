use nulang::reconcile::{
    ReconcilePhase, ReconcileRestoreError, ReconcileRetryDecision, ReconcileRetryPolicy,
    ReconcileRetryPolicyError, ReconcileSnapshot, ReconcileState, RECONCILE_SNAPSHOT_VERSION,
};

#[test]
fn durable_snapshot_round_trip_preserves_retry_progress() {
    let mut state = ReconcileState::new("replicas=3".to_string());
    let attempt = state.begin_attempt().unwrap();
    let policy = ReconcileRetryPolicy::new(100, 1_000, 2, 5, 0).unwrap();

    let decision = state
        .mark_retryable_with_policy(attempt, &policy, 41)
        .unwrap();
    assert!(matches!(decision, ReconcileRetryDecision::Scheduled(_)));

    let snapshot = state.snapshot();
    let json = serde_json::to_string(&snapshot).unwrap();
    let decoded: ReconcileSnapshot<String> = serde_json::from_str(&json).unwrap();
    let restored = ReconcileState::restore(decoded).unwrap();

    assert_eq!(restored, state);
    assert_eq!(restored.retry_ordinal(), 1);
    assert_eq!(snapshot.version, RECONCILE_SNAPSHOT_VERSION);
}

#[test]
fn restore_rejects_zero_generation_without_constructing_state() {
    let state = ReconcileState::new("replicas=3".to_string());
    let mut snapshot = state.snapshot();
    snapshot.generation = 0;

    assert_eq!(
        ReconcileState::restore(snapshot),
        Err(ReconcileRestoreError::ZeroGeneration)
    );
}

#[test]
fn restore_rejects_observed_generation_ahead_of_desired_generation() {
    let state = ReconcileState::new("replicas=3".to_string());
    let mut snapshot = state.snapshot();
    snapshot.observed_generation = 2;

    assert_eq!(
        ReconcileState::restore(snapshot),
        Err(ReconcileRestoreError::ObservedGenerationAhead {
            observed_generation: 2,
            generation: 1,
        })
    );
}

#[test]
fn restore_rejects_terminal_phase_that_did_not_observe_current_generation() {
    let state = ReconcileState::new("replicas=3".to_string());
    let mut snapshot = state.snapshot();
    snapshot.phase = ReconcilePhase::Converged;

    assert_eq!(
        ReconcileState::restore(snapshot),
        Err(ReconcileRestoreError::TerminalPhaseUnobserved {
            phase: ReconcilePhase::Converged,
            observed_generation: 0,
            generation: 1,
        })
    );
}

#[test]
fn restore_rejects_unknown_snapshot_version() {
    let state = ReconcileState::new("replicas=3".to_string());
    let mut snapshot = state.snapshot();
    snapshot.version = RECONCILE_SNAPSHOT_VERSION + 1;

    assert_eq!(
        ReconcileState::restore(snapshot),
        Err(ReconcileRestoreError::UnsupportedVersion {
            found: RECONCILE_SNAPSHOT_VERSION + 1,
            supported: RECONCILE_SNAPSHOT_VERSION,
        })
    );
}

#[test]
fn retry_policy_uses_capped_exponential_backoff() {
    let policy = ReconcileRetryPolicy::new(100, 1_000, 2, 8, 0).unwrap();

    assert_eq!(policy.delay_ms(1, 0), Some(100));
    assert_eq!(policy.delay_ms(2, 0), Some(200));
    assert_eq!(policy.delay_ms(3, 0), Some(400));
    assert_eq!(policy.delay_ms(4, 0), Some(800));
    assert_eq!(policy.delay_ms(5, 0), Some(1_000));
    assert_eq!(policy.delay_ms(8, 0), Some(1_000));
    assert_eq!(policy.delay_ms(9, 0), None);
}

#[test]
fn deterministic_jitter_is_stable_and_bounded() {
    let policy = ReconcileRetryPolicy::new(1_000, 10_000, 2, 5, 20).unwrap();

    let first = policy.delay_ms(2, 0xA11CE).unwrap();
    let replay = policy.delay_ms(2, 0xA11CE).unwrap();

    assert_eq!(first, replay);
    assert!((1_600..=2_400).contains(&first));
}

#[test]
fn invalid_retry_policy_fails_closed() {
    assert_eq!(
        ReconcileRetryPolicy::new(0, 1_000, 2, 3, 0),
        Err(ReconcileRetryPolicyError::ZeroInitialDelay)
    );
    assert_eq!(
        ReconcileRetryPolicy::new(1_000, 999, 2, 3, 0),
        Err(ReconcileRetryPolicyError::MaxDelayBelowInitial)
    );
    assert_eq!(
        ReconcileRetryPolicy::new(100, 1_000, 0, 3, 0),
        Err(ReconcileRetryPolicyError::ZeroMultiplier)
    );
    assert_eq!(
        ReconcileRetryPolicy::new(100, 1_000, 2, 3, 101),
        Err(ReconcileRetryPolicyError::JitterPercentOutOfRange)
    );
}

#[test]
fn retryable_failure_emits_durable_timer_ticket_for_current_generation() {
    let mut state = ReconcileState::new("replicas=3".to_string());
    let attempt = state.begin_attempt().unwrap();
    let policy = ReconcileRetryPolicy::new(250, 2_000, 2, 3, 0).unwrap();

    let decision = state
        .mark_retryable_with_policy(attempt, &policy, 7)
        .unwrap();
    let ReconcileRetryDecision::Scheduled(ticket) = decision else {
        panic!("expected scheduled retry");
    };

    assert_eq!(state.phase(), ReconcilePhase::RetryableFailure);
    assert_eq!(state.retry_ordinal(), 1);
    assert_eq!(ticket.generation(), 1);
    assert_eq!(ticket.retry_ordinal(), 1);
    assert_eq!(ticket.delay_ms(), 250);
    assert_eq!(ticket.timer_name(), "__reconcile_retry:g1:r1");
    assert!(state.retry_ticket_is_current(ticket));
}

#[test]
fn desired_state_change_invalidates_old_retry_ticket_and_resets_retry_sequence() {
    let mut state = ReconcileState::new("replicas=3".to_string());
    let attempt = state.begin_attempt().unwrap();
    let policy = ReconcileRetryPolicy::new(100, 1_000, 2, 3, 0).unwrap();
    let ReconcileRetryDecision::Scheduled(ticket) = state
        .mark_retryable_with_policy(attempt, &policy, 9)
        .unwrap()
    else {
        panic!("expected scheduled retry");
    };

    state.update_desired("replicas=5".to_string()).unwrap();

    assert!(!state.retry_ticket_is_current(ticket));
    assert_eq!(state.retry_ordinal(), 0);
    assert_eq!(state.phase(), ReconcilePhase::Pending);
}

#[test]
fn exhausting_retry_budget_marks_generation_terminally_observed() {
    let mut state = ReconcileState::new("replicas=3".to_string());
    let policy = ReconcileRetryPolicy::new(100, 1_000, 2, 2, 0).unwrap();

    let first = state.begin_attempt().unwrap();
    assert!(matches!(
        state.mark_retryable_with_policy(first, &policy, 1).unwrap(),
        ReconcileRetryDecision::Scheduled(_)
    ));

    let second = state.begin_attempt().unwrap();
    assert!(matches!(
        state.mark_retryable_with_policy(second, &policy, 1).unwrap(),
        ReconcileRetryDecision::Scheduled(_)
    ));

    let third = state.begin_attempt().unwrap();
    assert_eq!(
        state.mark_retryable_with_policy(third, &policy, 1).unwrap(),
        ReconcileRetryDecision::Exhausted
    );

    assert_eq!(state.phase(), ReconcilePhase::TerminalFailure);
    assert_eq!(state.observed_generation(), state.generation());
    assert!(!state.needs_reconcile());
}
