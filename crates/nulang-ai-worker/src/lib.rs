//! Worker agents execute assigned tasks.

use chrono::Utc;
use nulang_ai_core::{Task, TaskStatus};

#[derive(Debug, Clone)]
pub struct TaskExecution {
    pub task: Task,
    pub reason: Option<String>,
}

impl TaskExecution {
    pub fn new(task: Task) -> Self {
        Self { task, reason: None }
    }

    pub fn with_reason(task: Task, reason: impl Into<String>) -> Self {
        Self {
            task,
            reason: Some(reason.into()),
        }
    }
}

pub trait Worker: Send + Sync {
    fn agent_id(&self) -> &str;

    /// Execute one task and return its terminal state.
    ///
    /// Runtimes currently recognize `Completed`, `Blocked`, and `Failed`
    /// as terminal worker outcomes. Other returned states are normalized to
    /// `Failed` so an agent cannot silently leave an active intention stuck.
    fn execute(&self, task: &Task) -> Task;

    /// Execute a task while preserving optional worker-supplied outcome evidence.
    ///
    /// Existing workers only implementing `execute` remain source-compatible.
    /// The local runtime consumes the returned task's status as an outcome and
    /// preserves the originally scheduled task identity and plan metadata.
    fn execute_with_report(&self, task: &Task) -> TaskExecution {
        TaskExecution::new(self.execute(task))
    }
}

pub struct LocalWorker {
    id: String,
}

impl LocalWorker {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

impl Worker for LocalWorker {
    fn agent_id(&self) -> &str {
        &self.id
    }

    fn execute(&self, task: &Task) -> Task {
        let mut task = task.clone();
        task.status = TaskStatus::Completed;
        task.updated_at = Utc::now();
        task
    }
}
