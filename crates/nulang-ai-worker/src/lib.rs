//! Worker agents execute assigned tasks.

use chrono::Utc;
use nulang_ai_core::{Task, TaskStatus};

pub trait Worker: Send + Sync {
    fn agent_id(&self) -> &str;
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
