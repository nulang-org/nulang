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
        let mut capabilities: Vec<String> = capabilities.into_iter().map(Into::into).collect();
        capabilities.sort();
        capabilities.dedup();
        Self {
            id: id.into(),
            capabilities,
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

/// In-memory agent registry with deterministic least-privilege scheduling.
///
/// Eligible workers must satisfy every task capability. Among eligible workers,
/// the scheduler prefers the worker with the fewest extra capabilities, then
/// breaks ties by agent id. This reduces accidental privilege exposure while
/// remaining deterministic for tests and durable replay.
pub struct WorkerRegistry {
    workers: Vec<LocalWorker>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self { workers: Vec::new() }
    }

    pub fn register(&mut self, worker: LocalWorker) {
        if let Some(existing) = self
            .workers
            .iter_mut()
            .find(|existing| existing.agent_id() == worker.agent_id())
        {
            *existing = worker;
        } else {
            self.workers.push(worker);
        }
    }

    pub fn workers(&self) -> &[LocalWorker] {
        &self.workers
    }

    pub fn get(&self, agent_id: &str) -> Option<&LocalWorker> {
        self.workers
            .iter()
            .find(|worker| worker.agent_id() == agent_id)
    }

    pub fn select_agent_id(&self, task: &Task) -> Result<String, WorkerError> {
        self.workers
            .iter()
            .filter(|worker| worker.can_execute(task))
            .min_by(|a, b| {
                extra_capability_count(a, task)
                    .cmp(&extra_capability_count(b, task))
                    .then_with(|| a.agent_id().cmp(b.agent_id()))
            })
            .map(|worker| worker.agent_id().to_string())
            .ok_or_else(|| WorkerError::NoEligibleWorker {
                required: task.required_capabilities.clone(),
            })
    }

    pub fn execute_on(&self, agent_id: &str, task: &Task) -> Result<Task, WorkerError> {
        let worker = self
            .get(agent_id)
            .ok_or_else(|| WorkerError::UnknownWorker {
                agent_id: agent_id.to_string(),
            })?;
        worker.execute(task)
    }
}

impl Default for WorkerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn extra_capability_count(worker: &dyn Worker, task: &Task) -> usize {
    worker
        .capabilities()
        .iter()
        .filter(|capability| !task.required_capabilities.contains(capability))
        .count()
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerError {
    #[error("agent {agent_id} is missing required capabilities: {missing:?}")]
    MissingCapabilities {
        agent_id: String,
        missing: Vec<String>,
    },
    #[error("no eligible worker provides required capabilities: {required:?}")]
    NoEligibleWorker { required: Vec<String> },
    #[error("unknown worker: {agent_id}")]
    UnknownWorker { agent_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_core::ManagerKind;

    fn task_with(capabilities: &[&str]) -> Task {
        let mut task = Task::new(uuid::Uuid::new_v4(), "work", ManagerKind::Engineering);
        task.required_capabilities = capabilities.iter().map(|value| (*value).into()).collect();
        task
    }

    #[test]
    fn worker_executes_task_when_capabilities_match() {
        let worker = LocalWorker::with_capabilities("worker", ["code", "test", "repo.write"]);
        let task = task_with(&["code", "repo.write"]);

        let completed = worker.execute(&task).unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
    }

    #[test]
    fn worker_rejects_task_when_capability_is_missing() {
        let worker = LocalWorker::new("worker");
        let task = task_with(&["code", "deploy.execute"]);

        let error = worker.execute(&task).unwrap_err();
        assert_eq!(
            error,
            WorkerError::MissingCapabilities {
                agent_id: "worker".into(),
                missing: vec!["deploy.execute".into()],
            }
        );
    }

    #[test]
    fn registry_selects_least_privileged_eligible_worker() {
        let mut registry = WorkerRegistry::new();
        registry.register(LocalWorker::with_capabilities(
            "admin",
            ["code", "test", "repo.write", "deploy.execute", "resource.delete"],
        ));
        registry.register(LocalWorker::with_capabilities(
            "writer",
            ["code", "test", "repo.write"],
        ));
        registry.register(LocalWorker::with_capabilities("reader", ["code", "test"]));

        let task = task_with(&["code", "test", "repo.write"]);
        assert_eq!(registry.select_agent_id(&task).unwrap(), "writer");
    }

    #[test]
    fn registry_rejects_when_no_worker_is_eligible() {
        let mut registry = WorkerRegistry::new();
        registry.register(LocalWorker::new("worker"));
        let task = task_with(&["code", "deploy.execute"]);

        assert_eq!(
            registry.select_agent_id(&task).unwrap_err(),
            WorkerError::NoEligibleWorker {
                required: vec!["code".into(), "deploy.execute".into()],
            }
        );
    }

    #[test]
    fn registry_replaces_duplicate_agent_registration() {
        let mut registry = WorkerRegistry::new();
        registry.register(LocalWorker::with_capabilities("worker", ["code"]));
        registry.register(LocalWorker::with_capabilities("worker", ["code", "test"]));

        assert_eq!(registry.workers().len(), 1);
        assert_eq!(registry.get("worker").unwrap().capabilities().len(), 2);
    }
}
