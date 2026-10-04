use nulang::reconcile::{
    ReconcileRetryDecision, ReconcileRetryIdentity, ReconcileRetryPolicy, ReconcileState,
};

#[test]
fn durable_retry_timer_name_round_trips_to_typed_identity() {
    let mut state = ReconcileState::new("replicas=3".to_string());
    let attempt = state.begin_attempt().unwrap();
    let policy = ReconcileRetryPolicy::new(100, 1_000, 2, 3, 0).unwrap();
    let ReconcileRetryDecision::Scheduled(ticket) = state
        .mark_retryable_with_policy(attempt, &policy, 17)
        .unwrap()
    else {
        panic!("expected scheduled retry");
    };

    let identity = ReconcileRetryIdentity::parse_timer_name(&ticket.timer_name()).unwrap();

    assert_eq!(identity.generation(), ticket.generation());
    assert_eq!(identity.retry_ordinal(), ticket.retry_ordinal());
    assert_eq!(identity.timer_name(), ticket.timer_name());
    assert!(state.retry_identity_is_current(identity));
}

#[test]
fn retry_timer_parser_rejects_malformed_or_unfenced_context() {
    for invalid in [
        "",
        "__reconcile_retry",
        "__reconcile_retry:g1",
        "__reconcile_retry:g1:r",
        "__reconcile_retry:g:r1",
        "__reconcile_retry:g0:r1",
        "__reconcile_retry:g1:r0",
        "__reconcile_retry:g1:r1:extra",
        "other:g1:r1",
    ] {
        assert_eq!(
            ReconcileRetryIdentity::parse_timer_name(invalid),
            None,
            "unexpectedly accepted {invalid:?}"
        );
    }
}

#[test]
fn newer_desired_generation_rejects_parsed_old_timer_identity() {
    let mut state = ReconcileState::new("replicas=3".to_string());
    let attempt = state.begin_attempt().unwrap();
    let policy = ReconcileRetryPolicy::new(100, 1_000, 2, 3, 0).unwrap();
    let ReconcileRetryDecision::Scheduled(ticket) = state
        .mark_retryable_with_policy(attempt, &policy, 17)
        .unwrap()
    else {
        panic!("expected scheduled retry");
    };
    let identity = ReconcileRetryIdentity::parse_timer_name(&ticket.timer_name()).unwrap();

    state.update_desired("replicas=5".to_string()).unwrap();

    assert!(!state.retry_identity_is_current(identity));
}
