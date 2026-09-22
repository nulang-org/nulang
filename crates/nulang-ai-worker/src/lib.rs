//! Worker agents execute assigned tasks.

use chrono::Utc;
use nulang_ai_core::{Task, TaskStatus};

pub trait Worker: Send + Sync {
    fn agent_id(&self) -> &str;
    fn execute(&self, task: &Task) -> Task;

    /// Execute one logical task attempt with a replay-stable idempotency key.
    ///
    /// Workers that call external systems should override this method and
    /// propagate the key to those systems. Pure/local workers may rely on the
    /// default implementation.
    fn execute_idempotent(&self, task: &Task, idempotency_key: &str) -> Task {
        let _ = idempotency_key;
        self.execute(task)
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
