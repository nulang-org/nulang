//! Deterministic desired-state reconciliation primitives.
//!
//! The core contract is deliberately infrastructure-agnostic: callers own how
//! desired and observed state are read or applied, while this module owns the
//! generation fencing that makes retries, duplicate work, and superseded work
//! safe. An attempt may mutate controller state only while its generation is
//! still current.
//!
//! This is the runtime foundation for Kubernetes/Borg-style reconciliation in
//! Nulang without coupling the language surface to Kubernetes or any specific
//! deployment backend.

/// Lifecycle state for the current desired generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcilePhase {
    /// Desired state exists but has not yet been reconciled successfully.
    Pending,
    /// An attempt for the current generation has reported forward progress.
    Progressing,
    /// The current generation failed in a way that should be retried.
    RetryableFailure,
    /// The current generation was fully observed and matches desired state.
    Converged,
    /// The current generation was fully observed but cannot converge without a
    /// new desired generation or external intervention.
    TerminalFailure,
}

/// Opaque generation-fenced token identifying one reconciliation attempt.
///
/// Controllers should carry this token through asynchronous observe/apply work
/// and present it when reporting the result. A result from a superseded
/// generation is rejected without mutating state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReconcileAttempt {
    generation: u64,
    ordinal: u64,
}

impl ReconcileAttempt {
    /// Desired-state generation this attempt belongs to.
    pub fn generation(self) -> u64 {
        self.generation
    }

    /// Monotonic attempt number within the generation.
    pub fn ordinal(self) -> u64 {
        self.ordinal
    }
}

/// Fail-closed reconciliation errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileError {
    /// Updating desired state would wrap the generation counter.
    GenerationOverflow,
    /// Starting another attempt would wrap the per-generation attempt counter.
    AttemptOverflow,
    /// A result was produced for a generation that has already been superseded.
    StaleAttempt {
        attempt_generation: u64,
        current_generation: u64,
    },
}

/// Generation-fenced controller state for one logical resource.
///
/// `Spec` is intentionally unconstrained except where an operation needs a
/// property such as equality. This keeps the primitive usable for deployment
/// resources, actors, databases, external APIs, devices, or application-level
/// desired state without forcing serialization or storage policy into core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileState<Spec> {
    desired: Spec,
    generation: u64,
    observed_generation: u64,
    phase: ReconcilePhase,
    attempt_ordinal: u64,
}

impl<Spec> ReconcileState<Spec> {
    /// Create a resource with generation 1 and no observed generation yet.
    pub fn new(desired: Spec) -> Self {
        Self {
            desired,
            generation: 1,
            observed_generation: 0,
            phase: ReconcilePhase::Pending,
            attempt_ordinal: 0,
        }
    }

    /// Current desired state.
    pub fn desired(&self) -> &Spec {
        &self.desired
    }

    /// Monotonic desired-state generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Latest generation for which the controller reached a terminal
    /// observation (`Converged` or `TerminalFailure`).
    pub fn observed_generation(&self) -> u64 {
        self.observed_generation
    }

    /// Current controller phase.
    pub fn phase(&self) -> ReconcilePhase {
        self.phase
    }

    /// True when the controller should schedule reconciliation work.
    ///
    /// Terminal failure deliberately returns false for the current generation:
    /// repeatedly retrying a terminal condition would create an uncontrolled
    /// hot loop. A new desired generation makes this true again.
    pub fn needs_reconcile(&self) -> bool {
        self.observed_generation < self.generation
            || matches!(
                self.phase,
                ReconcilePhase::Pending
                    | ReconcilePhase::Progressing
                    | ReconcilePhase::RetryableFailure
            )
    }

    /// True only when the current generation is known to match desired state.
    pub fn is_converged(&self) -> bool {
        self.phase == ReconcilePhase::Converged
            && self.observed_generation == self.generation
    }

    /// Start one generation-fenced reconciliation attempt.
    pub fn begin_attempt(&mut self) -> Result<ReconcileAttempt, ReconcileError> {
        let ordinal = self
            .attempt_ordinal
            .checked_add(1)
            .ok_or(ReconcileError::AttemptOverflow)?;
        self.attempt_ordinal = ordinal;
        Ok(ReconcileAttempt {
            generation: self.generation,
            ordinal,
        })
    }

    /// Report forward progress for an attempt that still belongs to the current
    /// desired generation.
    pub fn mark_progressing(
        &mut self,
        attempt: ReconcileAttempt,
    ) -> Result<(), ReconcileError> {
        self.require_current(attempt)?;
        self.phase = ReconcilePhase::Progressing;
        Ok(())
    }

    /// Mark the current generation converged.
    pub fn mark_converged(
        &mut self,
        attempt: ReconcileAttempt,
    ) -> Result<(), ReconcileError> {
        self.require_current(attempt)?;
        self.observed_generation = self.generation;
        self.phase = ReconcilePhase::Converged;
        Ok(())
    }

    /// Record a retryable failure without claiming that the current generation
    /// has been fully observed.
    pub fn mark_retryable_failure(
        &mut self,
        attempt: ReconcileAttempt,
    ) -> Result<(), ReconcileError> {
        self.require_current(attempt)?;
        self.phase = ReconcilePhase::RetryableFailure;
        Ok(())
    }

    /// Record a terminal failure for the current generation.
    ///
    /// The generation becomes observed because the controller reached a final
    /// answer for that exact desired state, but `is_converged()` remains false.
    pub fn mark_terminal_failure(
        &mut self,
        attempt: ReconcileAttempt,
    ) -> Result<(), ReconcileError> {
        self.require_current(attempt)?;
        self.observed_generation = self.generation;
        self.phase = ReconcilePhase::TerminalFailure;
        Ok(())
    }

    fn require_current(&self, attempt: ReconcileAttempt) -> Result<(), ReconcileError> {
        if attempt.generation != self.generation {
            return Err(ReconcileError::StaleAttempt {
                attempt_generation: attempt.generation,
                current_generation: self.generation,
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn new_with_generation_for_test(desired: Spec, generation: u64) -> Self {
        Self {
            desired,
            generation,
            observed_generation: 0,
            phase: ReconcilePhase::Pending,
            attempt_ordinal: 0,
        }
    }
}

impl<Spec: PartialEq> ReconcileState<Spec> {
    /// Replace desired state if it changed, advancing the generation exactly
    /// once. Identical desired state is idempotent.
    ///
    /// A changed generation supersedes all in-flight attempts, returns the
    /// controller to `Pending`, and resets the per-generation attempt ordinal.
    /// The last terminally observed generation is preserved as historical
    /// progress so callers can distinguish "never observed" from "new desired
    /// state pending".
    pub fn update_desired(&mut self, desired: Spec) -> Result<bool, ReconcileError> {
        if self.desired == desired {
            return Ok(false);
        }

        let generation = self
            .generation
            .checked_add(1)
            .ok_or(ReconcileError::GenerationOverflow)?;

        self.desired = desired;
        self.generation = generation;
        self.phase = ReconcilePhase::Pending;
        self.attempt_ordinal = 0;
        Ok(true)
    }
}

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
