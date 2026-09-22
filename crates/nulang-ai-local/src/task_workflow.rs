use crate::store::{SqliteStore, StoreError};
use chrono::Utc;
use nulang_ai_core::{Task, TaskStatus};
use nulang_ai_worker::Worker;
use nulang_workflow::{
    ActivityDispatchRequest, ActivityDispatchResult, AppendOutcome, WorkflowEvent, WorkflowHistory,
    WorkflowId, WorkflowRuntime,
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum TaskWorkflowError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported durable task operation: {0}")]
    UnsupportedOperation(String),
    #[error("workflow/task identity mismatch")]
    IdentityMismatch,
}

pub(crate) struct TaskWorkflowRuntime<'a, W: Worker> {
    store: &'a SqliteStore,
    worker: &'a W,
}

impl<'a, W: Worker> TaskWorkflowRuntime<'a, W> {
    pub(crate) fn new(store: &'a SqliteStore, worker: &'a W) -> Self {
        Self { store, worker }
    }
}

impl<W: Worker> WorkflowRuntime for TaskWorkflowRuntime<'_, W> {
    type Error = TaskWorkflowError;

    fn now_millis(&mut self) -> u64 {
        Utc::now().timestamp_millis().max(0) as u64
    }

    fn load_history(
        &mut self,
        workflow_id: &WorkflowId,
    ) -> Result<WorkflowHistory, Self::Error> {
        Ok(self.store.load_workflow_history(workflow_id)?)
    }

    fn append_event(
        &mut self,
        workflow_id: &WorkflowId,
        expected_revision: u64,
        event: WorkflowEvent,
    ) -> Result<AppendOutcome, Self::Error> {
        Ok(self
            .store
            .append_workflow_event(workflow_id, expected_revision, event)?)
    }

    fn dispatch_activity(
        &mut self,
        request: ActivityDispatchRequest,
    ) -> Result<ActivityDispatchResult, Self::Error> {
        if request.operation != "agent.worker.execute" {
            return Err(TaskWorkflowError::UnsupportedOperation(request.operation));
        }

        let task: Task = serde_json::from_slice(&request.payload)?;
        let expected_workflow_id = WorkflowId::new(format!("agent-task:{}", task.id));
        if request.workflow_id != expected_workflow_id {
            return Err(TaskWorkflowError::IdentityMismatch);
        }

        let result = self
            .worker
            .execute_idempotent(&task, &request.idempotency_key);
        match result.status {
            TaskStatus::Completed => Ok(ActivityDispatchResult::Completed(
                serde_json::to_vec(&result)?,
            )),
            TaskStatus::Failed => Ok(ActivityDispatchResult::Failed {
                error: format!("worker {} reported task failure", self.worker.agent_id()),
                retryable: true,
            }),
            TaskStatus::Cancelled => Ok(ActivityDispatchResult::Failed {
                error: "task was cancelled".into(),
                retryable: false,
            }),
            status => Ok(ActivityDispatchResult::Failed {
                error: format!(
                    "worker {} returned non-terminal task status {:?}",
                    self.worker.agent_id(),
                    status
                ),
                retryable: false,
            }),
        }
    }
}
