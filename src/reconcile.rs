//! Deterministic desired-state reconciliation primitives.
//!
//! This module starts with tests that define the controller contract before
//! the implementation is added. Reconciliation is generation-fenced so stale
//! attempts can never mark newer desired state as converged.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_resource_starts_pending_at_generation_one() {
        let state = ReconcileState::new("replicas=3");

        assert_eq!(state.generation(), 1);
        assert_eq!(state.observed_generation(), 0);
        assert_eq!(state.phase(), ReconcilePhase::Pending);
        assert!(state.needs_reconcile());
    }

    #[test]
    fn identical_desired_state_is_idempotent() {
        let mut state = ReconcileState::new("replicas=3");

        assert_eq!(state.update_desired("replicas=3"), Ok(false));
        assert_eq!(state.generation(), 1);
    }

    #[test]
    fn changed_desired_state_advances_generation_and_supersedes_progress() {
        let mut state = ReconcileState::new("replicas=3");
        let attempt = state.begin_attempt().unwrap();
        state.mark_progressing(attempt).unwrap();

        assert_eq!(state.update_desired("replicas=5"), Ok(true));
        assert_eq!(state.generation(), 2);
        assert_eq!(state.phase(), ReconcilePhase::Pending);
        assert_eq!(state.observed_generation(), 0);
    }

    #[test]
    fn current_attempt_can_mark_resource_converged() {
        let mut state = ReconcileState::new("replicas=3");
        let attempt = state.begin_attempt().unwrap();

        state.mark_converged(attempt).unwrap();

        assert_eq!(state.phase(), ReconcilePhase::Converged);
        assert_eq!(state.observed_generation(), 1);
        assert!(!state.needs_reconcile());
    }

    #[test]
    fn stale_attempt_cannot_converge_newer_desired_state() {
        let mut state = ReconcileState::new("replicas=3");
        let stale = state.begin_attempt().unwrap();
        state.update_desired("replicas=5").unwrap();
        let before = state.clone();

        let error = state.mark_converged(stale).unwrap_err();

        assert_eq!(
            error,
            ReconcileError::StaleAttempt {
                attempt_generation: 1,
                current_generation: 2,
            }
        );
        assert_eq!(state, before);
    }

    #[test]
    fn retryable_failure_keeps_generation_unobserved() {
        let mut state = ReconcileState::new("replicas=3");
        let attempt = state.begin_attempt().unwrap();

        state.mark_retryable_failure(attempt).unwrap();

        assert_eq!(state.phase(), ReconcilePhase::RetryableFailure);
        assert_eq!(state.observed_generation(), 0);
        assert!(state.needs_reconcile());
    }

    #[test]
    fn terminal_failure_observes_current_generation_but_remains_non_converged() {
        let mut state = ReconcileState::new("replicas=3");
        let attempt = state.begin_attempt().unwrap();

        state.mark_terminal_failure(attempt).unwrap();

        assert_eq!(state.phase(), ReconcilePhase::TerminalFailure);
        assert_eq!(state.observed_generation(), 1);
        assert!(!state.needs_reconcile());
        assert!(!state.is_converged());
    }

    #[test]
    fn generation_overflow_fails_without_mutating_desired_state() {
        let mut state = ReconcileState::new_with_generation_for_test("old", u64::MAX);
        let before = state.clone();

        assert_eq!(
            state.update_desired("new"),
            Err(ReconcileError::GenerationOverflow)
        );
        assert_eq!(state, before);
    }

    #[test]
    fn attempt_ordinals_are_monotonic_within_generation() {
        let mut state = ReconcileState::new("replicas=3");

        let first = state.begin_attempt().unwrap();
        let second = state.begin_attempt().unwrap();

        assert_eq!(first.generation(), 1);
        assert_eq!(first.ordinal(), 1);
        assert_eq!(second.generation(), 1);
        assert_eq!(second.ordinal(), 2);
    }
}
