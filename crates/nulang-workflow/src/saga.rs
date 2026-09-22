use crate::{
    ActivityProgress, ActivitySpec, AppendOutcome, Backoff, DurableWorkflowExecutor, RetryPolicy,
    WorkflowEngineError, WorkflowEvent, WorkflowHistory,
    WorkflowId, WorkflowRuntime,
};
use blake3::Hasher;
use std::collections::HashSet;

const SAGA_PLAN_DOMAIN: &[u8] = b"nulang.workflow.saga-plan.v1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SagaAction {
    pub operation: String,
    pub request: Vec<u8>,
    pub retry: RetryPolicy,
}

impl SagaAction {
    pub fn new(operation: impl Into<String>, request: impl Into<Vec<u8>>) -> Self {
        Self {
            operation: operation.into(),
            request: request.into(),
            retry: RetryPolicy::none(),
        }
    }

    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SagaStep {
    pub name: String,
    pub action: SagaAction,
    pub compensation: Option<SagaAction>,
}

impl SagaStep {
    pub fn new(name: impl Into<String>, action: SagaAction) -> Self {
        Self {
            name: name.into(),
            action,
            compensation: None,
        }
    }

    pub fn compensate(mut self, compensation: SagaAction) -> Self {
        self.compensation = Some(compensation);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SagaPlan {
    pub saga_id: String,
    pub steps: Vec<SagaStep>,
}

impl SagaPlan {
    pub fn new(saga_id: impl Into<String>, steps: Vec<SagaStep>) -> Self {
        Self {
            saga_id: saga_id.into(),
            steps,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SagaPhase {
    Forward,
    Compensation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SagaProgress {
    Completed,
    WaitingForRetry {
        phase: SagaPhase,
        step_index: usize,
        step_name: String,
        next_attempt: u32,
        ready_at_millis: u64,
        last_error: String,
    },
    FailedAndCompensated {
        failed_step_index: usize,
        failed_step_name: String,
        error: String,
    },
    CompensationFailed {
        failed_step_index: usize,
        failed_step_name: String,
        failed_error: String,
        compensation_step_index: usize,
        compensation_step_name: String,
        compensation_error: String,
    },
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DurableSagaExecutor;

impl DurableSagaExecutor {
    pub fn execute<R: WorkflowRuntime>(
        &self,
        runtime: &mut R,
        workflow_id: &WorkflowId,
        plan: &SagaPlan,
    ) -> Result<SagaProgress, WorkflowEngineError<R::Error>> {
        validate_plan(plan)?;
        let plan_hash = saga_plan_hash(plan);
        let mut history = runtime
            .load_history(workflow_id)
            .map_err(WorkflowEngineError::Runtime)?;
        let mut state = inspect_saga(&history, plan, plan_hash)?;

        if !state.started {
            history.revision = append(
                runtime,
                workflow_id,
                history.revision,
                WorkflowEvent::SagaStarted {
                    saga_id: plan.saga_id.clone(),
                    plan_hash,
                },
            )?;
        }

        if state.completed {
            return Ok(SagaProgress::Completed);
        }

        if let Some(failure) = state.failure.clone() {
            return compensate(runtime, workflow_id, plan, state, failure);
        }

        loop {
            let step_index = state.committed_steps.len();
            if step_index == plan.steps.len() {
                append(
                    runtime,
                    workflow_id,
                    history.revision,
                    WorkflowEvent::SagaCompleted {
                        saga_id: plan.saga_id.clone(),
                    },
                )?;
                return Ok(SagaProgress::Completed);
            }

            let step = &plan.steps[step_index];
            let activity = forward_activity(plan, step_index);
            match DurableWorkflowExecutor.execute_activity(runtime, workflow_id, &activity)? {
                ActivityProgress::Completed(_) => {
                    history = runtime
                        .load_history(workflow_id)
                        .map_err(WorkflowEngineError::Runtime)?;
                    history.revision = append(
                        runtime,
                        workflow_id,
                        history.revision,
                        WorkflowEvent::SagaStepCommitted {
                            saga_id: plan.saga_id.clone(),
                            step_index: step_index as u32,
                            step_name: step.name.clone(),
                        },
                    )?;
                    state.committed_steps.push(step_index);
                }
                ActivityProgress::WaitingForRetry {
                    next_attempt,
                    ready_at_millis,
                    last_error,
                } => {
                    return Ok(SagaProgress::WaitingForRetry {
                        phase: SagaPhase::Forward,
                        step_index,
                        step_name: step.name.clone(),
                        next_attempt,
                        ready_at_millis,
                        last_error,
                    });
                }
                ActivityProgress::Failed { error, .. } => {
                    history = runtime
                        .load_history(workflow_id)
                        .map_err(WorkflowEngineError::Runtime)?;
                    history.revision = append(
                        runtime,
                        workflow_id,
                        history.revision,
                        WorkflowEvent::SagaFailed {
                            saga_id: plan.saga_id.clone(),
                            failed_step_index: step_index as u32,
                            failed_step_name: step.name.clone(),
                            error: error.clone(),
                        },
                    )?;
                    let failure = SagaFailure {
                        step_index,
                        step_name: step.name.clone(),
                        error,
                    };
                    state.failure = Some(failure.clone());
                    return compensate(runtime, workflow_id, plan, state, failure);
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct SagaFailure {
    step_index: usize,
    step_name: String,
    error: String,
}

#[derive(Debug)]
struct SagaState {
    started: bool,
    committed_steps: Vec<usize>,
    failure: Option<SagaFailure>,
    compensated_steps: Vec<usize>,
    completed: bool,
}

fn compensate<R: WorkflowRuntime>(
    runtime: &mut R,
    workflow_id: &WorkflowId,
    plan: &SagaPlan,
    mut state: SagaState,
    failure: SagaFailure,
) -> Result<SagaProgress, WorkflowEngineError<R::Error>> {
    for step_index in state.committed_steps.iter().copied().rev() {
        let step = &plan.steps[step_index];
        let Some(_) = step.compensation.as_ref() else {
            continue;
        };
        if state.compensated_steps.contains(&step_index) {
            continue;
        }

        let activity = compensation_activity(plan, step_index);
        match DurableWorkflowExecutor.execute_activity(runtime, workflow_id, &activity)? {
            ActivityProgress::Completed(_) => {
                let history = runtime
                    .load_history(workflow_id)
                    .map_err(WorkflowEngineError::Runtime)?;
                append(
                    runtime,
                    workflow_id,
                    history.revision,
                    WorkflowEvent::SagaStepCompensated {
                        saga_id: plan.saga_id.clone(),
                        step_index: step_index as u32,
                        step_name: step.name.clone(),
                    },
                )?;
                state.compensated_steps.push(step_index);
            }
            ActivityProgress::WaitingForRetry {
                next_attempt,
                ready_at_millis,
                last_error,
            } => {
                return Ok(SagaProgress::WaitingForRetry {
                    phase: SagaPhase::Compensation,
                    step_index,
                    step_name: step.name.clone(),
                    next_attempt,
                    ready_at_millis,
                    last_error,
                });
            }
            ActivityProgress::Failed {
                error: compensation_error,
                ..
            } => {
                return Ok(SagaProgress::CompensationFailed {
                    failed_step_index: failure.step_index,
                    failed_step_name: failure.step_name,
                    failed_error: failure.error,
                    compensation_step_index: step_index,
                    compensation_step_name: step.name.clone(),
                    compensation_error,
                });
            }
        }
    }

    Ok(SagaProgress::FailedAndCompensated {
        failed_step_index: failure.step_index,
        failed_step_name: failure.step_name,
        error: failure.error,
    })
}

fn validate_plan<E>(plan: &SagaPlan) -> Result<(), WorkflowEngineError<E>> {
    if plan.saga_id.is_empty() {
        return Err(WorkflowEngineError::HistoryConflict(
            "saga id must not be empty".into(),
        ));
    }

    if plan.steps.len() > u32::MAX as usize {
        return Err(WorkflowEngineError::HistoryConflict(format!(
            "saga {} exceeds the durable u32 step-index limit",
            plan.saga_id
        )));
    }

    let mut names = HashSet::with_capacity(plan.steps.len());
    for step in &plan.steps {
        if step.name.is_empty() {
            return Err(WorkflowEngineError::HistoryConflict(
                "saga step name must not be empty".into(),
            ));
        }
        if !names.insert(step.name.as_str()) {
            return Err(WorkflowEngineError::HistoryConflict(format!(
                "saga {} contains duplicate step name {}",
                plan.saga_id, step.name
            )));
        }
        if step.action.operation.is_empty() {
            return Err(WorkflowEngineError::HistoryConflict(format!(
                "saga {} step {} has an empty operation",
                plan.saga_id, step.name
            )));
        }
        if step
            .compensation
            .as_ref()
            .is_some_and(|action| action.operation.is_empty())
        {
            return Err(WorkflowEngineError::HistoryConflict(format!(
                "saga {} step {} has an empty compensation operation",
                plan.saga_id, step.name
            )));
        }
    }
    Ok(())
}

fn inspect_saga<E>(
    history: &WorkflowHistory,
    plan: &SagaPlan,
    plan_hash: [u8; 32],
) -> Result<SagaState, WorkflowEngineError<E>> {
    let mut started = false;
    let mut committed_steps = Vec::new();
    let mut failure: Option<SagaFailure> = None;
    let mut compensated_steps = Vec::new();
    let mut completed = false;

    for event in &history.events {
        match event {
            WorkflowEvent::SagaStarted {
                saga_id,
                plan_hash: persisted_hash,
            } if saga_id == &plan.saga_id => {
                if started {
                    return history_conflict(plan, "has multiple start records");
                }
                if persisted_hash != &plan_hash {
                    return history_conflict(plan, "plan hash changed during replay");
                }
                started = true;
            }
            WorkflowEvent::SagaStepCommitted {
                saga_id,
                step_index,
                step_name,
            } if saga_id == &plan.saga_id => {
                require_started(plan, started)?;
                if failure.is_some() || completed {
                    return history_conflict(plan, "committed a forward step after terminal state");
                }
                let index = *step_index as usize;
                let expected = committed_steps.len();
                if index != expected || index >= plan.steps.len() {
                    return history_conflict(plan, "forward step commits are not contiguous");
                }
                if plan.steps[index].name != *step_name {
                    return history_conflict(plan, "forward step name does not match the plan");
                }
                committed_steps.push(index);
            }
            WorkflowEvent::SagaFailed {
                saga_id,
                failed_step_index,
                failed_step_name,
                error,
            } if saga_id == &plan.saga_id => {
                require_started(plan, started)?;
                if completed || failure.is_some() {
                    return history_conflict(plan, "has multiple or post-completion failure records");
                }
                let index = *failed_step_index as usize;
                if index != committed_steps.len() || index >= plan.steps.len() {
                    return history_conflict(plan, "failure index does not match the next forward step");
                }
                if plan.steps[index].name != *failed_step_name {
                    return history_conflict(plan, "failed step name does not match the plan");
                }
                failure = Some(SagaFailure {
                    step_index: index,
                    step_name: failed_step_name.clone(),
                    error: error.clone(),
                });
            }
            WorkflowEvent::SagaStepCompensated {
                saga_id,
                step_index,
                step_name,
            } if saga_id == &plan.saga_id => {
                require_started(plan, started)?;
                if completed || failure.is_none() {
                    return history_conflict(plan, "recorded compensation outside failure recovery");
                }
                let index = *step_index as usize;
                if index >= plan.steps.len()
                    || !committed_steps.contains(&index)
                    || plan.steps[index].compensation.is_none()
                {
                    return history_conflict(plan, "compensated a step that was not compensatable");
                }
                if plan.steps[index].name != *step_name {
                    return history_conflict(plan, "compensated step name does not match the plan");
                }
                let expected = committed_steps
                    .iter()
                    .copied()
                    .rev()
                    .find(|candidate| {
                        plan.steps[*candidate].compensation.is_some()
                            && !compensated_steps.contains(candidate)
                    });
                if expected != Some(index) {
                    return history_conflict(plan, "compensations are not in reverse commit order");
                }
                compensated_steps.push(index);
            }
            WorkflowEvent::SagaCompleted { saga_id } if saga_id == &plan.saga_id => {
                require_started(plan, started)?;
                if completed || failure.is_some() || committed_steps.len() != plan.steps.len() {
                    return history_conflict(plan, "completed before every forward step committed");
                }
                completed = true;
            }
            _ => {}
        }
    }

    Ok(SagaState {
        started,
        committed_steps,
        failure,
        compensated_steps,
        completed,
    })
}

fn require_started<E>(
    plan: &SagaPlan,
    started: bool,
) -> Result<(), WorkflowEngineError<E>> {
    if started {
        Ok(())
    } else {
        history_conflict(plan, "contains saga events before the start record")
    }
}

fn history_conflict<T, E>(
    plan: &SagaPlan,
    message: &str,
) -> Result<T, WorkflowEngineError<E>> {
    Err(WorkflowEngineError::HistoryConflict(format!(
        "saga {} {}",
        plan.saga_id, message
    )))
}

fn append<R: WorkflowRuntime>(
    runtime: &mut R,
    workflow_id: &WorkflowId,
    expected_revision: u64,
    event: WorkflowEvent,
) -> Result<u64, WorkflowEngineError<R::Error>> {
    match runtime
        .append_event(workflow_id, expected_revision, event)
        .map_err(WorkflowEngineError::Runtime)?
    {
        AppendOutcome::Appended { new_revision } => Ok(new_revision),
        AppendOutcome::Conflict => Err(WorkflowEngineError::ConcurrencyConflict),
    }
}

fn forward_activity(plan: &SagaPlan, step_index: usize) -> ActivitySpec {
    let step = &plan.steps[step_index];
    ActivitySpec::new(
        internal_step_name("forward", plan, step_index),
        step.action.operation.clone(),
        step.action.request.clone(),
    )
    .retry(step.action.retry)
}

fn compensation_activity(plan: &SagaPlan, step_index: usize) -> ActivitySpec {
    let step = &plan.steps[step_index];
    let compensation = step
        .compensation
        .as_ref()
        .expect("compensation_activity called only for compensatable steps");
    ActivitySpec::new(
        internal_step_name("compensate", plan, step_index),
        compensation.operation.clone(),
        compensation.request.clone(),
    )
    .retry(compensation.retry)
}

fn internal_step_name(phase: &str, plan: &SagaPlan, step_index: usize) -> String {
    let step_name = &plan.steps[step_index].name;
    format!(
        "saga.{phase}|id{}:{}|idx{}|step{}:{}",
        plan.saga_id.len(),
        plan.saga_id,
        step_index,
        step_name.len(),
        step_name
    )
}

fn saga_plan_hash(plan: &SagaPlan) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(SAGA_PLAN_DOMAIN);
    hash_bytes(&mut hasher, plan.saga_id.as_bytes());
    hasher.update(&(plan.steps.len() as u64).to_le_bytes());
    for step in &plan.steps {
        hash_bytes(&mut hasher, step.name.as_bytes());
        hash_action(&mut hasher, &step.action);
        match &step.compensation {
            Some(action) => {
                hasher.update(&[1]);
                hash_action(&mut hasher, action);
            }
            None => hasher.update(&[0]),
        }
    }
    *hasher.finalize().as_bytes()
}

fn hash_action(hasher: &mut Hasher, action: &SagaAction) {
    hash_bytes(hasher, action.operation.as_bytes());
    hash_bytes(hasher, &action.request);
    hasher.update(&action.retry.max_attempts.to_le_bytes());
    match action.retry.backoff {
        Backoff::None => hasher.update(&[0]),
        Backoff::Fixed { delay_ms } => {
            hasher.update(&[1]);
            hasher.update(&delay_ms.to_le_bytes());
        }
        Backoff::Exponential {
            initial_delay_ms,
            max_delay_ms,
            multiplier,
        } => {
            hasher.update(&[2]);
            hasher.update(&initial_delay_ms.to_le_bytes());
            hasher.update(&max_delay_ms.to_le_bytes());
            hasher.update(&multiplier.to_le_bytes());
        }
    };
}

fn hash_bytes(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ActivityDispatchResult;
    use std::{collections::VecDeque, fmt};

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestError(&'static str);

    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    struct MockRuntime {
        now: u64,
        history: WorkflowHistory,
        dispatches: Vec<String>,
        results: VecDeque<ActivityDispatchResult>,
    }

    impl MockRuntime {
        fn new(results: Vec<ActivityDispatchResult>) -> Self {
            Self {
                now: 1_000,
                history: WorkflowHistory::default(),
                dispatches: Vec::new(),
                results: results.into(),
            }
        }
    }

    impl WorkflowRuntime for MockRuntime {
        type Error = TestError;

        fn now_millis(&mut self) -> u64 {
            self.now
        }

        fn load_history(
            &mut self,
            _workflow_id: &WorkflowId,
        ) -> Result<WorkflowHistory, Self::Error> {
            Ok(self.history.clone())
        }

        fn append_event(
            &mut self,
            _workflow_id: &WorkflowId,
            expected_revision: u64,
            event: WorkflowEvent,
        ) -> Result<AppendOutcome, Self::Error> {
            if expected_revision != self.history.revision {
                return Ok(AppendOutcome::Conflict);
            }
            self.history.events.push(event);
            self.history.revision += 1;
            Ok(AppendOutcome::Appended {
                new_revision: self.history.revision,
            })
        }

        fn dispatch_activity(
            &mut self,
            request: crate::ActivityDispatchRequest,
        ) -> Result<ActivityDispatchResult, Self::Error> {
            self.dispatches.push(request.operation);
            self.results
                .pop_front()
                .ok_or(TestError("missing dispatch result"))
        }
    }

    fn action(operation: &str) -> SagaAction {
        SagaAction::new(operation, operation.as_bytes().to_vec())
    }

    fn plan() -> SagaPlan {
        SagaPlan::new(
            "purchase",
            vec![
                SagaStep::new("reserve", action("inventory.reserve"))
                    .compensate(action("inventory.release")),
                SagaStep::new("charge", action("payment.charge"))
                    .compensate(action("payment.refund")),
                SagaStep::new("ship", action("shipping.create")),
            ],
        )
    }

    #[test]
    fn saga_happy_path_commits_in_order() {
        let mut rt = MockRuntime::new(vec![
            ActivityDispatchResult::Completed(vec![]),
            ActivityDispatchResult::Completed(vec![]),
            ActivityDispatchResult::Completed(vec![]),
        ]);

        assert_eq!(
            DurableSagaExecutor
                .execute(&mut rt, &WorkflowId::new("order:1"), &plan())
                .unwrap(),
            SagaProgress::Completed
        );
        assert_eq!(
            rt.dispatches,
            vec!["inventory.reserve", "payment.charge", "shipping.create"]
        );
        assert!(matches!(
            rt.history.events.last(),
            Some(WorkflowEvent::SagaCompleted { .. })
        ));
    }

    #[test]
    fn saga_failure_compensates_in_reverse_commit_order() {
        let mut rt = MockRuntime::new(vec![
            ActivityDispatchResult::Completed(vec![]),
            ActivityDispatchResult::Completed(vec![]),
            ActivityDispatchResult::Failed {
                error: "carrier unavailable".into(),
                retryable: false,
            },
            ActivityDispatchResult::Completed(vec![]),
            ActivityDispatchResult::Completed(vec![]),
        ]);

        assert_eq!(
            DurableSagaExecutor
                .execute(&mut rt, &WorkflowId::new("order:2"), &plan())
                .unwrap(),
            SagaProgress::FailedAndCompensated {
                failed_step_index: 2,
                failed_step_name: "ship".into(),
                error: "carrier unavailable".into(),
            }
        );
        assert_eq!(
            rt.dispatches,
            vec![
                "inventory.reserve",
                "payment.charge",
                "shipping.create",
                "payment.refund",
                "inventory.release",
            ]
        );
    }

    #[test]
    fn compensation_retry_resumes_without_repeating_prior_compensation() {
        let mut retry_plan = plan();
        retry_plan.steps[0].compensation = Some(
            action("inventory.release").retry(RetryPolicy::fixed(3, 500)),
        );
        let mut rt = MockRuntime::new(vec![
            ActivityDispatchResult::Completed(vec![]),
            ActivityDispatchResult::Completed(vec![]),
            ActivityDispatchResult::Failed {
                error: "ship failed".into(),
                retryable: false,
            },
            ActivityDispatchResult::Completed(vec![]),
            ActivityDispatchResult::Failed {
                error: "inventory busy".into(),
                retryable: true,
            },
            ActivityDispatchResult::Completed(vec![]),
        ]);
        let workflow_id = WorkflowId::new("order:3");

        assert_eq!(
            DurableSagaExecutor
                .execute(&mut rt, &workflow_id, &retry_plan)
                .unwrap(),
            SagaProgress::WaitingForRetry {
                phase: SagaPhase::Compensation,
                step_index: 0,
                step_name: "reserve".into(),
                next_attempt: 2,
                ready_at_millis: 1_500,
                last_error: "inventory busy".into(),
            }
        );
        assert_eq!(
            rt.dispatches,
            vec![
                "inventory.reserve",
                "payment.charge",
                "shipping.create",
                "payment.refund",
                "inventory.release",
            ]
        );

        rt.now = 1_400;
        assert!(matches!(
            DurableSagaExecutor
                .execute(&mut rt, &workflow_id, &retry_plan)
                .unwrap(),
            SagaProgress::WaitingForRetry {
                phase: SagaPhase::Compensation,
                step_index: 0,
                ..
            }
        ));
        assert_eq!(rt.dispatches.len(), 5);

        rt.now = 1_500;
        assert!(matches!(
            DurableSagaExecutor
                .execute(&mut rt, &workflow_id, &retry_plan)
                .unwrap(),
            SagaProgress::FailedAndCompensated { .. }
        ));
        assert_eq!(
            rt.dispatches,
            vec![
                "inventory.reserve",
                "payment.charge",
                "shipping.create",
                "payment.refund",
                "inventory.release",
                "inventory.release",
            ]
        );
    }

    #[test]
    fn saga_plan_change_fails_closed() {
        let original = plan();
        let mut changed = plan();
        changed.steps[1].compensation = Some(action("payment.void"));

        let mut rt = MockRuntime::new(vec![]);
        rt.history = WorkflowHistory::new(
            1,
            vec![WorkflowEvent::SagaStarted {
                saga_id: original.saga_id.clone(),
                plan_hash: saga_plan_hash(&original),
            }],
        );

        assert!(matches!(
            DurableSagaExecutor.execute(&mut rt, &WorkflowId::new("order:4"), &changed),
            Err(WorkflowEngineError::HistoryConflict(_))
        ));
        assert!(rt.dispatches.is_empty());
    }

    #[test]
    fn saga_rejects_non_reverse_compensation_history() {
        let plan = plan();
        let mut rt = MockRuntime::new(vec![]);
        rt.history = WorkflowHistory::new(
            5,
            vec![
                WorkflowEvent::SagaStarted {
                    saga_id: plan.saga_id.clone(),
                    plan_hash: saga_plan_hash(&plan),
                },
                WorkflowEvent::SagaStepCommitted {
                    saga_id: plan.saga_id.clone(),
                    step_index: 0,
                    step_name: "reserve".into(),
                },
                WorkflowEvent::SagaStepCommitted {
                    saga_id: plan.saga_id.clone(),
                    step_index: 1,
                    step_name: "charge".into(),
                },
                WorkflowEvent::SagaFailed {
                    saga_id: plan.saga_id.clone(),
                    failed_step_index: 2,
                    failed_step_name: "ship".into(),
                    error: "failed".into(),
                },
                WorkflowEvent::SagaStepCompensated {
                    saga_id: plan.saga_id.clone(),
                    step_index: 0,
                    step_name: "reserve".into(),
                },
            ],
        );

        assert!(matches!(
            DurableSagaExecutor.execute(&mut rt, &WorkflowId::new("order:5"), &plan),
            Err(WorkflowEngineError::HistoryConflict(_))
        ));
    }
}
