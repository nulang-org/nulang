use crate::engine::inspect_activity;
use crate::{
    ActivityDispatchRequest, ActivityDispatchResult, ActivityProgress, ActivitySpec, AppendOutcome,
    WorkflowEngineError, WorkflowEvent, WorkflowHistory, WorkflowId,
};
use std::future::Future;
use std::pin::Pin;

/// Heap-pinned host future used by the async workflow boundary.
///
/// The SDK keeps this dependency-free instead of requiring a specific async
/// runtime or `async-trait`. Tokio, async-std, smol, and custom executors can
/// all implement the same contract.
pub type WorkflowFuture<'a, T, E> =
    Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;

/// Async host boundary for durable workflow execution.
///
/// This is semantically identical to `WorkflowRuntime`: implementations must
/// provide linearizable compare-and-set history appends and preserve the
/// supplied activity idempotency key across provider retries.
pub trait AsyncWorkflowRuntime: Send {
    type Error;

    fn now_millis(&mut self) -> u64;

    fn load_history<'a>(
        &'a mut self,
        workflow_id: &'a WorkflowId,
    ) -> WorkflowFuture<'a, WorkflowHistory, Self::Error>;

    fn append_event<'a>(
        &'a mut self,
        workflow_id: &'a WorkflowId,
        expected_revision: u64,
        event: WorkflowEvent,
    ) -> WorkflowFuture<'a, AppendOutcome, Self::Error>;

    fn dispatch_activity<'a>(
        &'a mut self,
        request: ActivityDispatchRequest,
    ) -> WorkflowFuture<'a, ActivityDispatchResult, Self::Error>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AsyncDurableWorkflowExecutor;

impl AsyncDurableWorkflowExecutor {
    pub async fn execute_activity<R: AsyncWorkflowRuntime>(
        &self,
        runtime: &mut R,
        workflow_id: &WorkflowId,
        spec: &ActivitySpec,
    ) -> Result<ActivityProgress, WorkflowEngineError<R::Error>> {
        let invocation_id = crate::ActivityInvocationId::derive(
            workflow_id,
            &spec.step,
            spec.occurrence,
            &spec.operation,
        );
        let mut history = runtime
            .load_history(workflow_id)
            .await
            .map_err(WorkflowEngineError::Runtime)?;
        let state = inspect_activity(&history, &invocation_id, spec)?;

        if let Some(result) = state.completed {
            return Ok(ActivityProgress::Completed(result));
        }

        if let Some((attempt, error, retryable, next_retry_at_millis)) =
            state.last_failure.clone()
        {
            if !state.retry.should_retry(attempt, retryable) {
                return Ok(ActivityProgress::Failed { attempt, error });
            }

            let ready_at_millis = next_retry_at_millis.ok_or_else(|| {
                WorkflowEngineError::HistoryConflict(format!(
                    "activity {} is retryable after attempt {} but has no retry deadline",
                    invocation_id, attempt
                ))
            })?;
            if runtime.now_millis() < ready_at_millis {
                return Ok(ActivityProgress::WaitingForRetry {
                    next_attempt: attempt.saturating_add(1),
                    ready_at_millis,
                    last_error: error,
                });
            }
        }

        if !state.prepared {
            history.revision = append(
                runtime,
                workflow_id,
                history.revision,
                WorkflowEvent::ActivityPrepared {
                    invocation_id,
                    step: spec.step.clone(),
                    operation: spec.operation.clone(),
                    request: spec.request.clone(),
                    retry: spec.retry,
                },
            )
            .await?;
        }

        let attempt = state
            .last_failure
            .as_ref()
            .map_or(1, |(attempt, _, _, _)| attempt.saturating_add(1));

        let dispatch = ActivityDispatchRequest {
            workflow_id: workflow_id.clone(),
            invocation_id,
            step: spec.step.clone(),
            operation: spec.operation.clone(),
            attempt,
            idempotency_key: invocation_id.idempotency_key(),
            payload: spec.request.clone(),
        };

        match runtime
            .dispatch_activity(dispatch)
            .await
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
                )
                .await?;
                Ok(ActivityProgress::Completed(result))
            }
            ActivityDispatchResult::Failed { error, retryable } => {
                let should_retry = state.retry.should_retry(attempt, retryable);
                let next_retry_at_millis = should_retry.then(|| {
                    let delay_ms = state.retry.delay_after_failure(attempt).unwrap_or(0);
                    runtime.now_millis().saturating_add(delay_ms)
                });

                append(
                    runtime,
                    workflow_id,
                    history.revision,
                    WorkflowEvent::ActivityAttemptFailed {
                        invocation_id,
                        attempt,
                        error: error.clone(),
                        retryable,
                        next_retry_at_millis,
                    },
                )
                .await?;

                match next_retry_at_millis {
                    Some(ready_at_millis) => Ok(ActivityProgress::WaitingForRetry {
                        next_attempt: attempt.saturating_add(1),
                        ready_at_millis,
                        last_error: error,
                    }),
                    None => Ok(ActivityProgress::Failed { attempt, error }),
                }
            }
        }
    }
}

async fn append<R: AsyncWorkflowRuntime>(
    runtime: &mut R,
    workflow_id: &WorkflowId,
    expected_revision: u64,
    event: WorkflowEvent,
) -> Result<u64, WorkflowEngineError<R::Error>> {
    match runtime
        .append_event(workflow_id, expected_revision, event)
        .await
        .map_err(WorkflowEngineError::Runtime)?
    {
        AppendOutcome::Appended { new_revision } => Ok(new_revision),
        AppendOutcome::Conflict => Err(WorkflowEngineError::ConcurrencyConflict),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActivityInvocationId, RetryPolicy};
    use std::collections::VecDeque;
    use std::fmt;
    use std::task::{Context, Poll, Waker};

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

    impl AsyncWorkflowRuntime for MockRuntime {
        type Error = TestError;

        fn now_millis(&mut self) -> u64 {
            self.now
        }

        fn load_history<'a>(
            &'a mut self,
            _workflow_id: &'a WorkflowId,
        ) -> WorkflowFuture<'a, WorkflowHistory, Self::Error> {
            let history = self.history.clone();
            Box::pin(async move { Ok(history) })
        }

        fn append_event<'a>(
            &'a mut self,
            _workflow_id: &'a WorkflowId,
            expected_revision: u64,
            event: WorkflowEvent,
        ) -> WorkflowFuture<'a, AppendOutcome, Self::Error> {
            Box::pin(async move {
                if expected_revision != self.history.revision {
                    return Ok(AppendOutcome::Conflict);
                }
                self.history.events.push(event);
                self.history.revision += 1;
                Ok(AppendOutcome::Appended {
                    new_revision: self.history.revision,
                })
            })
        }

        fn dispatch_activity<'a>(
            &'a mut self,
            request: ActivityDispatchRequest,
        ) -> WorkflowFuture<'a, ActivityDispatchResult, Self::Error> {
            Box::pin(async move {
                self.dispatches.push(request);
                self.results
                    .pop_front()
                    .ok_or(TestError("missing dispatch result"))
            })
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = Box::pin(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn workflow() -> WorkflowId {
        WorkflowId::new("async-order-42")
    }

    fn activity() -> ActivitySpec {
        ActivitySpec::new("charge", "payments.charge", b"request".to_vec())
    }

    #[test]
    fn async_executor_replays_completed_activity_without_redispatch() {
        let mut runtime = MockRuntime::new(vec![ActivityDispatchResult::Completed(
            b"receipt".to_vec(),
        )]);
        let executor = AsyncDurableWorkflowExecutor;

        assert_eq!(
            block_on(executor.execute_activity(&mut runtime, &workflow(), &activity())).unwrap(),
            ActivityProgress::Completed(b"receipt".to_vec())
        );
        assert_eq!(runtime.dispatches.len(), 1);

        assert_eq!(
            block_on(executor.execute_activity(&mut runtime, &workflow(), &activity())).unwrap(),
            ActivityProgress::Completed(b"receipt".to_vec())
        );
        assert_eq!(runtime.dispatches.len(), 1);
    }

    #[test]
    fn async_executor_pins_retry_deadline_and_idempotency_key() {
        let spec = activity().retry(RetryPolicy::fixed(3, 500));
        let mut runtime = MockRuntime::new(vec![
            ActivityDispatchResult::Failed {
                error: "timeout".into(),
                retryable: true,
            },
            ActivityDispatchResult::Completed(b"ok".to_vec()),
        ]);
        let workflow_id = workflow();
        let expected_id =
            ActivityInvocationId::derive(&workflow_id, &spec.step, spec.occurrence, &spec.operation);
        let executor = AsyncDurableWorkflowExecutor;

        assert_eq!(
            block_on(executor.execute_activity(&mut runtime, &workflow_id, &spec)).unwrap(),
            ActivityProgress::WaitingForRetry {
                next_attempt: 2,
                ready_at_millis: 1_500,
                last_error: "timeout".into(),
            }
        );
        assert_eq!(
            runtime.dispatches[0].idempotency_key,
            expected_id.idempotency_key()
        );

        runtime.now = 1_400;
        assert!(matches!(
            block_on(executor.execute_activity(&mut runtime, &workflow_id, &spec)).unwrap(),
            ActivityProgress::WaitingForRetry { .. }
        ));
        assert_eq!(runtime.dispatches.len(), 1);

        runtime.now = 1_500;
        assert_eq!(
            block_on(executor.execute_activity(&mut runtime, &workflow_id, &spec)).unwrap(),
            ActivityProgress::Completed(b"ok".to_vec())
        );
        assert_eq!(runtime.dispatches.len(), 2);
        assert_eq!(
            runtime.dispatches[0].idempotency_key,
            runtime.dispatches[1].idempotency_key
        );
    }

    #[test]
    fn async_executor_rejects_changed_replay_request() {
        let workflow_id = workflow();
        let original = activity();
        let invocation_id = ActivityInvocationId::derive(
            &workflow_id,
            &original.step,
            original.occurrence,
            &original.operation,
        );
        let mut runtime = MockRuntime::new(vec![]);
        runtime.history = WorkflowHistory::new(
            1,
            vec![WorkflowEvent::ActivityPrepared {
                invocation_id,
                step: original.step.clone(),
                operation: original.operation.clone(),
                request: original.request.clone(),
                retry: original.retry,
            }],
        );

        let changed = ActivitySpec::new("charge", "payments.charge", b"different".to_vec());
        assert!(matches!(
            block_on(
                AsyncDurableWorkflowExecutor
                    .execute_activity(&mut runtime, &workflow_id, &changed)
            ),
            Err(WorkflowEngineError::HistoryConflict(_))
        ));
        assert!(runtime.dispatches.is_empty());
    }
}
