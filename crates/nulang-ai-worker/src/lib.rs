//! Worker agents execute assigned tasks.

use chrono::Utc;
use nulang_ai_core::{Task, TaskStatus};

pub trait Worker: Send + Sync {
    fn agent_id(&self) -> &str;
    fn capabilities(&self) -> &[String];

    fn missing_capabilities(&self, task: &Task) -> Vec<String> {
        task.required_capabilities
            .iter()
            .filter(|required| !self.capabilities().contains(required))
            .cloned()
            .collect()
    }

    fn can_execute(&self, task: &Task) -> bool {
        self.missing_capabilities(task).is_empty()
    }

    fn execute(&self, task: &Task) -> Result<Task, WorkerError>;
}

pub struct LocalWorker {
    id: String,
    capabilities: Vec<String>,
}

impl LocalWorker {
    pub fn new(id: impl Into<String>) -> Self {
        Self::with_capabilities(id, ["code", "test"])
    }

    pub fn with_capabilities<I, S>(id: impl Into<String>, capabilities: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            id: id.into(),
            capabilities: capabilities.into_iter().map(Into::into).collect(),
        }
    }
}

impl Worker for LocalWorker {
    fn agent_id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    fn execute(&self, task: &Task) -> Result<Task, WorkerError> {
        let missing = self.missing_capabilities(task);
        if !missing.is_empty() {
            return Err(WorkerError::MissingCapabilities {
                agent_id: self.id.clone(),
                missing,
            });
        }

        let mut task = task.clone();
        task.status = TaskStatus::Completed;
        task.updated_at = Utc::now();
        Ok(task)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerError {
    #[error("agent {agent_id} is missing required capabilities: {missing:?}")]
    MissingCapabilities {
        agent_id: String,
        missing: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_core::ManagerKind;

    #[test]
    fn worker_executes_task_when_capabilities_match() {
        let worker = LocalWorker::with_capabilities("worker", ["code", "test", "repo.write"]);
        let mut task = Task::new(uuid::Uuid::new_v4(), "edit code", ManagerKind::Engineering);
        task.required_capabilities = vec!["code".into(), "repo.write".into()];

        let completed = worker.execute(&task).unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
    }

    #[test]
    fn worker_rejects_task_when_capability_is_missing() {
        let worker = LocalWorker::new("worker");
        let mut task = Task::new(uuid::Uuid::new_v4(), "deploy", ManagerKind::Engineering);
        task.required_capabilities = vec!["code".into(), "deploy.execute".into()];

        let error = worker.execute(&task).unwrap_err();
        assert_eq!(
            error,
            WorkerError::MissingCapabilities {
                agent_id: "worker".into(),
                missing: vec!["deploy.execute".into()],
            }
        );
    }
}
