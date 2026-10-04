use super::{
    ReconcileAttempt, ReconcileError, ReconcileRestoreError, ReconcileRetryDecision,
    ReconcileRetryIdentity, ReconcileRetryPolicy, ReconcileSnapshot, ReconcileState,
};
use crate::runtime::Runtime;

/// Classification of one timer context presented to a reconciliation controller.
///
/// Only the exact current generation/retry identity is allowed to start another
/// reconciliation attempt. Older retry timers are harmlessly classified as
/// stale, and unrelated workflow timers remain outside reconciliation entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileTimerAdmission {
    Ready(ReconcileAttempt),
    Stale(ReconcileRetryIdentity),
    Unrelated,
}

/// Runtime-adapter failures that occur outside the deterministic state machine.
#[derive(Debug)]
pub enum ReconcileControllerError {
    State(ReconcileError),
    ActorNotDurableWorkflow { actor_id: u64 },
    Timer(std::io::Error),
}

impl std::fmt::Display for ReconcileControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State(error) => write!(f, "reconciliation state error: {error:?}"),
            Self::ActorNotDurableWorkflow { actor_id } => write!(
                f,
                "actor {actor_id} is not a durable workflow and cannot own reconciliation retries"
            ),
            Self::Timer(error) => write!(f, "failed to persist reconciliation retry timer: {error}"),
        }
    }
}

impl std::error::Error for ReconcileControllerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Timer(error) => Some(error),
            Self::State(_) | Self::ActorNotDurableWorkflow { .. } => None,
        }
    }
}

impl From<ReconcileError> for ReconcileControllerError {
    fn from(error: ReconcileError) -> Self {
        Self::State(error)
    }
}

/// Thin runtime adapter around [`ReconcileState`].
///
/// This type deliberately does not own a scheduler or persistence backend.
/// Generation/attempt/retry fencing remains in `ReconcileState`, while durable
/// retry timing is delegated to Nulang's existing workflow timer path.
pub struct ReconcileController<Spec> {
    state: ReconcileState<Spec>,
    retry_policy: ReconcileRetryPolicy,
    jitter_seed: u64,
}

impl<Spec> ReconcileController<Spec> {
    pub fn new(desired: Spec, retry_policy: ReconcileRetryPolicy, jitter_seed: u64) -> Self {
        Self {
            state: ReconcileState::new(desired),
            retry_policy,
            jitter_seed,
        }
    }

    pub fn restore(
        snapshot: ReconcileSnapshot<Spec>,
        retry_policy: ReconcileRetryPolicy,
        jitter_seed: u64,
    ) -> Result<Self, ReconcileRestoreError> {
        Ok(Self {
            state: ReconcileState::restore(snapshot)?,
            retry_policy,
            jitter_seed,
        })
    }

    pub fn state(&self) -> &ReconcileState<Spec> {
        &self.state
    }

    pub fn begin_attempt(&mut self) -> Result<ReconcileAttempt, ReconcileError> {
        self.state.begin_attempt()
    }

    /// Classify a fired workflow timer and start a new attempt only for the
    /// exact currently scheduled retry identity.
    pub fn admit_retry_timer(
        &mut self,
        timer_name: &str,
    ) -> Result<ReconcileTimerAdmission, ReconcileError> {
        let Some(identity) = ReconcileRetryIdentity::parse_timer_name(timer_name) else {
            return Ok(ReconcileTimerAdmission::Unrelated);
        };

        if !self.state.retry_identity_is_current(identity) {
            return Ok(ReconcileTimerAdmission::Stale(identity));
        }

        Ok(ReconcileTimerAdmission::Ready(self.state.begin_attempt()?))
    }
}

impl<Spec: Clone> ReconcileController<Spec> {
    pub fn snapshot(&self) -> ReconcileSnapshot<Spec> {
        self.state.snapshot()
    }

    /// Record a retryable failure and, if policy permits another retry, commit
    /// its durable workflow timer before exposing the mutated controller state.
    ///
    /// `mark_retryable_with_policy` must run first to derive the fenced timer
    /// identity. The prior state is retained so any durable timer write failure
    /// can roll the in-memory controller back to the exact pre-schedule state.
    pub fn schedule_retry(
        &mut self,
        runtime: &mut Runtime,
        actor_id: u64,
        attempt: ReconcileAttempt,
    ) -> Result<ReconcileRetryDecision, ReconcileControllerError> {
        let durable_workflow = runtime
            .actors
            .get(&actor_id)
            .is_some_and(|actor| actor.persistent && actor.is_workflow);
        if !durable_workflow {
            return Err(ReconcileControllerError::ActorNotDurableWorkflow { actor_id });
        }

        let previous = self.state.clone();
        let decision = self.state.mark_retryable_with_policy(
            attempt,
            &self.retry_policy,
            self.jitter_seed,
        )?;

        let ReconcileRetryDecision::Scheduled(ticket) = decision else {
            return Ok(decision);
        };

        if let Err(error) = runtime.schedule_workflow_timer(
            actor_id,
            &ticket.timer_name(),
            ticket.delay_ms(),
        ) {
            self.state = previous;
            return Err(ReconcileControllerError::Timer(error));
        }

        Ok(decision)
    }
}

impl<Spec: PartialEq> ReconcileController<Spec> {
    pub fn update_desired(&mut self, desired: Spec) -> Result<bool, ReconcileError> {
        self.state.update_desired(desired)
    }
}
