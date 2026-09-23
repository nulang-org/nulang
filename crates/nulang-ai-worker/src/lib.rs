//! Worker agents execute assigned tasks.

use chrono::Utc;
use nulang_ai_core::{Task, TaskStatus};

pub trait Worker: Send + Sync {
    fn agent_id(&self) -> &str;

    /// Execute one task and return its terminal state.
    ///
    /// Runtimes currently recognize `Completed`, `Blocked`, and `Failed`
    /// as terminal worker outcomes. Other returned states are normalized to
    /// `Failed` so an agent cannot silently leave an active intention stuck.
    fn execute(&self, task: &Task) -> Task;
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
