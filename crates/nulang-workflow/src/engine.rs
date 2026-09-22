use crate::{ActivityInvocationId, RetryPolicy, WorkflowEvent, WorkflowHistory, WorkflowId};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivitySpec {
    pub step: String,
    pub operation: String,
    pub occurrence: u32,
    pub request: Vec<u8>,
    pub retry: RetryPolicy,
}

impl ActivitySpec {
    pub fn new(
        step: impl Into<String>,
        operation: impl Into<String>,
        request: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            step: step.into(),
            operation: operation.into(),
            occurrence: 0,
            request: request.into(),
            retry: RetryPolicy::none(),
        }
    }

    pub fn occurrence(mut self, occurrence: u32) -> Self {
        self.occurrence = occurrence;
        self
    }

    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityDispatchRequest {
    pub workflow_id: WorkflowId,
    pub invocation_id: ActivityInvocationId,
    pub step: String,
    pub operation: String,
    pub attempt: u32,
    pub idempotency_key: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityDispatchResult {
    Completed(Vec<u8>),
    Failed { error: String, retryable: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityProgress {
    Completed(Vec<u8>),
    WaitingForRetry {
        next_attempt: u32,
        ready_at_millis: u64,
        last_error: String,
    },
    Failed { attempt: u32, error: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended { new_revision: u64 },
    Conflict,
}

pub trait WorkflowRuntime {
    type Error;

    fn now_millis(&mut self) -> u64;

    fn load_history(
        &mut self,
        workflow_id: &WorkflowId,
    ) -> Result<WorkflowHistory, Self::Error>;

    fn append_event(
        &mut self,
        workflow_id: &WorkflowId,
        expected_revision: u64,
        event: WorkflowEvent,
    ) -> Result<AppendOutcome, Self::Error>;

    fn dispatch_activity(
        &mut self,
        request: ActivityDispatchRequest,
    ) -> Result<ActivityDispatchResult, Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowEngineError<E> {
    Runtime(E),
    HistoryConflict(String),
    ConcurrencyConflict,
}

impl<E: fmt::Display> fmt::Display for WorkflowEngineError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(err) => write!(f, "workflow runtime error: {err}"),
            Self::HistoryConflict(msg) => write!(f, "workflow history conflict: {msg}"),
            Self::ConcurrencyConflict => f.write_str("workflow history changed concurrently"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for WorkflowEngineError<E> {}

#[derive(Debug, Default, Clone, Copy)]
pub struct DurableWorkflowExecutor;

impl DurableWorkflowExecutor {
    pub fn execute_activity<R: WorkflowRuntime>(
        &self,
        runtime: &mut R,
        workflow_id: &WorkflowId,
        spec: &ActivitySpec,
    ) -> Result<ActivityProgress, WorkflowEngineError<R::Error>> {
        let invocation_id = ActivityInvocationId::derive(
            workflow_id,
            &spec.step,
            spec.occurrence,
            &spec.operation,
        );
        let mut history = runtime
            .load_history(workflow_id)
            .map_err(WorkflowEngineError::Runtime)?;
        let state = inspect_activity(&history, &invocation_id, spec)?;

        if let Some(result) = state.completed {
            return Ok(ActivityProgress::Completed(result));
        }

        if let Some((attempt, error, retryable)) = state.last_failure.clone() {
            if !state.retry.should_retry(attempt, retryable) {
                return Ok(ActivityProgress::Failed { attempt, error });
            }
        }

        if !state.prepared {
            history.revision = append(
                runtime,
                workflow_id,
                history.revision,
                WorkflowEvent::ActivityPrepared {
                    invocation_id: invocation_id.clone(),
                    step: spec.step.clone(),
                    operation: spec.operation.clone(),
                    request: spec.request.clone(),
                    retry: spec.retry,
                },
            )?;
        }

        let attempt = state
            .last_failure
            .as_ref()
            .map_or(1, |(attempt, _, _)| attempt.saturating_add(1));

        if let Some((next_attempt, ready_at_millis)) = state.retry_schedule {
            if next_attempt != attempt {
                return Err(WorkflowEngineError::HistoryConflict(format!(
                    "activity {} scheduled retry attempt {}, expected {}",
                    invocation_id, next_attempt, attempt
                )));
            }
            if runtime.now_millis() < ready_at_millis {
                return Ok(ActivityProgress::WaitingForRetry {
                    next_attempt,
                    ready_at_millis,
                    last_error: state
                        .last_failure
                        .map(|(_, error, _)| error)
                        .unwrap_or_default(),
                });
            }
        }

        let dispatch = ActivityDispatchRequest {
            workflow_id: workflow_id.clone(),
            invocation_id: invocation_id.clone(),
            step: spec.step.clone(),
            operation: spec.operation.clone(),
            attempt,
            idempotency_key: invocation_id.idempotency_key(),
            payload: spec.request.clone(),
        };

        match runtime
            .dispatch_activity(dispatch)
            .map_err(WorkflowEngineError::Runtime)?
        {
            ActivityDispatchResult::Completed(result) => {
                append(
                    runtime,
                    workflow_id,
                    history.revision,
                    WorkflowEvent::ActivityCompleted {
                        invocation_id,
                        result: result.clone(),
                    },
                )?;
                Ok(ActivityProgress::Completed(result))
            }
            ActivityDispatchResult::Failed { error, retryable } => {
                history.revision = append(
                    runtime,
                    workflow_id,
                    history.revision,
                    WorkflowEvent::ActivityAttemptFailed {
                        invocation_id: invocation_id.clone(),
                        attempt,
                        error: error.clone(),
                        retryable,
                    },
                )?;

                if !state.retry.should_retry(attempt, retryable) {
                    return Ok(ActivityProgress::Failed { attempt, error });
                }

                let delay_ms = state.retry.delay_after_failure(attempt).unwrap_or(0);
                let ready_at_millis = runtime.now_millis().saturating_add(delay_ms);
                append(
                    runtime,
                    workflow_id,
                    history.revision,
                    WorkflowEvent::ActivityRetryScheduled {
                        invocation_id,
                        next_attempt: attempt.saturating_add(1),
                        ready_at_millis,
                    },
                )?;
                Ok(ActivityProgress::WaitingForRetry {
                    next_attempt: attempt.saturating_add(1),
                    ready_at_millis,
                    last_error: error,
                })
            }
        }
    }
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

#[derive(Debug)]
struct ActivityState {
    prepared: bool,
    retry: RetryPolicy,
    completed: Option<Vec<u8>>,
    last_failure: Option<(u32, String, bool)>,
    retry_schedule: Option<(u32, u64)>,
}

fn inspect_activity<E>(
    history: &WorkflowHistory,
    invocation_id: &ActivityInvocationId,
    spec: &ActivitySpec,
) -> Result<ActivityState, WorkflowEngineError<E>> {
    let mut prepared = false;
    let mut retry = spec.retry;
    let mut completed = None;
    let mut last_failure: Option<(u32, String, bool)> = None;
    let mut retry_schedule = None;

    for event in &history.events {
        match event {
            WorkflowEvent::ActivityPrepared {
                invocation_id: id,
                step,
                operation,
                request,
                retry: persisted_retry,
            } if id == invocation_id => {
                if prepared {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "activity {} has multiple prepare records",
                        invocation_id
                    )));
                }

                if step != &spec.step
                    || operation != &spec.operation
                    || request != &spec.request
                    || persisted_retry != &spec.retry
                {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "activity {} replay does not match its prepared request",
                        invocation_id
                    )));
                }

                prepared = true;
                retry = *persisted_retry;
            }
            WorkflowEvent::ActivityAttemptFailed {
                invocation_id: id,
                attempt,
                error,
                retryable,
            } if id == invocation_id => {
                if !prepared {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "activity {} failed before it was prepared",
                        invocation_id
                    )));
                }

                let expected = last_failure
                    .as_ref()
                    .map_or(1, |(prev, _, _)| prev.saturating_add(1));
                if *attempt != expected {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "activity {} failure attempt {} is not contiguous after {}",
                        invocation_id,
                        attempt,
                        expected.saturating_sub(1)
                    )));
                }

                last_failure = Some((*attempt, error.clone(), *retryable));
                retry_schedule = None;
            }
            WorkflowEvent::ActivityRetryScheduled {
                invocation_id: id,
                next_attempt,
                ready_at_millis,
            } if id == invocation_id => {
                let expected = last_failure
                    .as_ref()
                    .map(|(attempt, _, _)| attempt.saturating_add(1))
                    .ok_or_else(|| {
                        WorkflowEngineError::HistoryConflict(format!(
                            "activity {} scheduled a retry without a failed attempt",
                            invocation_id
                        ))
                    })?;

                if *next_attempt != expected {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "activity {} scheduled retry attempt {}, expected {}",
                        invocation_id, next_attempt, expected
                    )));
                }

                retry_schedule = Some((*next_attempt, *ready_at_millis));
            }
            WorkflowEvent::ActivityCompleted {
                invocation_id: id,
                result,
            } if id == invocation_id => {
                if !prepared {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "activity {} completed before it was prepared",
                        invocation_id
                    )));
                }

                if completed.is_some() {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "activity {} has multiple completion records",
                        invocation_id
                    )));
                }

                completed = Some(result.clone());
            }
            _ => {}
        }
    }

    Ok(ActivityState {
        prepared,
        retry,
        completed,
        last_failure,
        retry_schedule,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

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
        dispatches: Vec<ActivityDispatchRequest>,
        results: VecDeque<ActivityDispatchResult>,
        force_conflict: bool,
    }

    impl MockRuntime {
        fn new(results: Vec<ActivityDispatchResult>) -> Self {
            Self {
                now: 1_000,
                history: WorkflowHistory::default(),
                dispatches: Vec::new(),
                results: results.into(),
                force_conflict: false,
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
            if self.force_conflict || expected_revision != self.history.revision {
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
            request: ActivityDispatchRequest,
        ) -> Result<ActivityDispatchResult, Self::Error> {
            self.dispatches.push(request);
            self.results
                .pop_front()
                .ok_or(TestError("missing dispatch result"))
        }
    }

    fn workflow() -> WorkflowId {
        WorkflowId::new("order-42")
    }

    fn activity() -> ActivitySpec {
        ActivitySpec::new("charge", "payments.charge", b"request".to_vec())
    }

    #[test]
    fn test_success_is_prepared_then_completed_and_replay_is_cached() {
        let mut rt = MockRuntime::new(vec![ActivityDispatchResult::Completed(
            b"receipt".to_vec(),
        )]);
        let executor = DurableWorkflowExecutor;

        assert_eq!(
            executor
                .execute_activity(&mut rt, &workflow(), &activity())
                .unwrap(),
            ActivityProgress::Completed(b"receipt".to_vec())
        );
        assert_eq!(rt.dispatches.len(), 1);
        assert_eq!(rt.history.events.len(), 2);

        assert_eq!(
            executor
                .execute_activity(&mut rt, &workflow(), &activity())
                .unwrap(),
            ActivityProgress::Completed(b"receipt".to_vec())
        );
        assert_eq!(rt.dispatches.len(), 1);
    }

    #[test]
    fn test_prepared_crash_window_redispatches_same_idempotency_key() {
        let wf = workflow();
        let spec = activity();
        let id =
            ActivityInvocationId::derive(&wf, &spec.step, spec.occurrence, &spec.operation);
        let mut rt = MockRuntime::new(vec![ActivityDispatchResult::Completed(
            b"receipt".to_vec(),
        )]);
        rt.history.events.push(WorkflowEvent::ActivityPrepared {
            invocation_id: id.clone(),
            step: spec.step.clone(),
            operation: spec.operation.clone(),
            request: spec.request.clone(),
            retry: spec.retry,
        });
        rt.history.revision = 1;

        DurableWorkflowExecutor
            .execute_activity(&mut rt, &wf, &spec)
            .unwrap();

        assert_eq!(rt.dispatches[0].idempotency_key, id.idempotency_key());
    }

    #[test]
    fn test_replay_with_changed_request_fails_closed() {
        let wf = workflow();
        let original = activity();
        let id = ActivityInvocationId::derive(
            &wf,
            &original.step,
            original.occurrence,
            &original.operation,
        );
        let mut rt = MockRuntime::new(vec![]);
        rt.history = WorkflowHistory::new(
            1,
            vec![WorkflowEvent::ActivityPrepared {
                invocation_id: id,
                step: original.step.clone(),
                operation: original.operation.clone(),
                request: original.request.clone(),
                retry: original.retry,
            }],
        );

        let changed =
            ActivitySpec::new("charge", "payments.charge", b"different".to_vec());
        assert!(matches!(
            DurableWorkflowExecutor.execute_activity(&mut rt, &wf, &changed),
            Err(WorkflowEngineError::HistoryConflict(_))
        ));
        assert!(rt.dispatches.is_empty());
    }

    #[test]
    fn test_retry_waits_until_recorded_deadline_and_reuses_idempotency_key() {
        let spec = activity().retry(RetryPolicy::fixed(3, 500));
        let mut rt = MockRuntime::new(vec![
            ActivityDispatchResult::Failed {
                error: "timeout".into(),
                retryable: true,
            },
            ActivityDispatchResult::Completed(b"ok".to_vec()),
        ]);
        let wf = workflow();
        let executor = DurableWorkflowExecutor;

        let first = executor.execute_activity(&mut rt, &wf, &spec).unwrap();
        assert_eq!(
            first,
            ActivityProgress::WaitingForRetry {
                next_attempt: 2,
                ready_at_millis: 1_500,
                last_error: "timeout".into(),
            }
        );
        assert_eq!(rt.dispatches.len(), 1);

        rt.now = 1_400;
        assert!(matches!(
            executor.execute_activity(&mut rt, &wf, &spec).unwrap(),
            ActivityProgress::WaitingForRetry { .. }
        ));
        assert_eq!(rt.dispatches.len(), 1);

        rt.now = 1_500;
        assert_eq!(
            executor.execute_activity(&mut rt, &wf, &spec).unwrap(),
            ActivityProgress::Completed(b"ok".to_vec())
        );
        assert_eq!(rt.dispatches.len(), 2);
        assert_eq!(
            rt.dispatches[0].idempotency_key,
            rt.dispatches[1].idempotency_key
        );
        assert_eq!(rt.dispatches[1].attempt, 2);
    }

    #[test]
    fn test_nonretryable_failure_is_terminal_on_replay() {
        let mut rt = MockRuntime::new(vec![ActivityDispatchResult::Failed {
            error: "declined".into(),
            retryable: false,
        }]);
        let spec = activity().retry(RetryPolicy::fixed(5, 100));
        let wf = workflow();
        let executor = DurableWorkflowExecutor;

        assert_eq!(
            executor.execute_activity(&mut rt, &wf, &spec).unwrap(),
            ActivityProgress::Failed {
                attempt: 1,
                error: "declined".into(),
            }
        );
        assert_eq!(
            executor.execute_activity(&mut rt, &wf, &spec).unwrap(),
            ActivityProgress::Failed {
                attempt: 1,
                error: "declined".into(),
            }
        );
        assert_eq!(rt.dispatches.len(), 1);
    }

    #[test]
    fn test_append_conflict_prevents_dispatch() {
        let mut rt =
            MockRuntime::new(vec![ActivityDispatchResult::Completed(vec![])]);
        rt.force_conflict = true;

        assert_eq!(
            DurableWorkflowExecutor.execute_activity(
                &mut rt,
                &workflow(),
                &activity()
            ),
            Err(WorkflowEngineError::ConcurrencyConflict)
        );
        assert!(rt.dispatches.is_empty());
    }
}
