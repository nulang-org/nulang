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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerMetadata {
    pub max_concurrency: usize,
    pub estimated_latency_ms: u64,
    pub cost_microusd_per_task: u64,
    pub locality: Option<String>,
    pub accelerator: Option<String>,
}

impl Default for WorkerMetadata {
    fn default() -> Self {
        Self {
            max_concurrency: 1,
            estimated_latency_ms: 0,
            cost_microusd_per_task: 0,
            locality: None,
            accelerator: None,
        }
    }
}

pub struct WorkerRecord {
    worker: LocalWorker,
    pub metadata: WorkerMetadata,
    pub last_heartbeat_ms: i64,
    pub active_tasks: usize,
    pub available: bool,
}

impl WorkerRecord {
    pub fn agent_id(&self) -> &str {
        self.worker.agent_id()
    }

    pub fn capabilities(&self) -> &[String] {
        self.worker.capabilities()
    }

    pub fn is_fresh_at(&self, now_ms: i64, lease_timeout_ms: i64) -> bool {
        now_ms.saturating_sub(self.last_heartbeat_ms) <= lease_timeout_ms
    }

    pub fn has_capacity(&self) -> bool {
        self.available && self.active_tasks < self.metadata.max_concurrency.max(1)
    }
}

/// In-memory worker registry with leases and deterministic least-privilege scheduling.
///
/// Eligible workers must be healthy, have spare concurrency, and satisfy every
/// task capability. Among eligible workers, scheduling first minimizes excess
/// privileges, then active load, cost, latency, and finally agent id.
pub struct WorkerRegistry {
    workers: Vec<WorkerRecord>,
    lease_timeout_ms: i64,
}

impl WorkerRegistry {
    pub const DEFAULT_LEASE_TIMEOUT_MS: i64 = 30_000;

    pub fn new() -> Self {
        Self::with_lease_timeout(Self::DEFAULT_LEASE_TIMEOUT_MS)
    }

    pub fn with_lease_timeout(lease_timeout_ms: i64) -> Self {
        Self {
            workers: Vec::new(),
            lease_timeout_ms: lease_timeout_ms.max(1),
        }
    }

    pub fn register(&mut self, worker: LocalWorker) {
        self.register_with_metadata(worker, WorkerMetadata::default());
    }

    pub fn register_with_metadata(&mut self, worker: LocalWorker, metadata: WorkerMetadata) {
        self.register_at(worker, metadata, now_ms());
    }

    pub fn register_at(
        &mut self,
        worker: LocalWorker,
        metadata: WorkerMetadata,
        heartbeat_ms: i64,
    ) {
        let record = WorkerRecord {
            worker,
            metadata,
            last_heartbeat_ms: heartbeat_ms,
            active_tasks: 0,
            available: true,
        };
        if let Some(existing) = self
            .workers
            .iter_mut()
            .find(|existing| existing.agent_id() == record.agent_id())
        {
            *existing = record;
        } else {
            self.workers.push(record);
        }
    }

    pub fn workers(&self) -> &[WorkerRecord] {
        &self.workers
    }

    pub fn get(&self, agent_id: &str) -> Option<&WorkerRecord> {
        self.workers
            .iter()
            .find(|record| record.agent_id() == agent_id)
    }

    pub fn heartbeat(&mut self, agent_id: &str) -> Result<(), WorkerError> {
        self.heartbeat_at(agent_id, now_ms())
    }

    pub fn heartbeat_at(&mut self, agent_id: &str, heartbeat_ms: i64) -> Result<(), WorkerError> {
        let record = self.get_mut(agent_id)?;
        record.last_heartbeat_ms = heartbeat_ms;
        Ok(())
    }

    pub fn set_available(&mut self, agent_id: &str, available: bool) -> Result<(), WorkerError> {
        self.get_mut(agent_id)?.available = available;
        Ok(())
    }

    pub fn update_load(&mut self, agent_id: &str, active_tasks: usize) -> Result<(), WorkerError> {
        self.get_mut(agent_id)?.active_tasks = active_tasks;
        Ok(())
    }

    pub fn select_agent_id(&self, task: &Task) -> Result<String, WorkerError> {
        self.select_agent_id_at(task, now_ms())
    }

    pub fn select_agent_id_at(&self, task: &Task, now_ms: i64) -> Result<String, WorkerError> {
        self.workers
            .iter()
            .filter(|record| record.is_fresh_at(now_ms, self.lease_timeout_ms))
            .filter(|record| record.has_capacity())
            .filter(|record| record.worker.can_execute(task))
            .min_by(|a, b| {
                extra_capability_count(&a.worker, task)
                    .cmp(&extra_capability_count(&b.worker, task))
                    .then_with(|| a.active_tasks.cmp(&b.active_tasks))
                    .then_with(|| {
                        a.metadata
                            .cost_microusd_per_task
                            .cmp(&b.metadata.cost_microusd_per_task)
                    })
                    .then_with(|| {
                        a.metadata
                            .estimated_latency_ms
                            .cmp(&b.metadata.estimated_latency_ms)
                    })
                    .then_with(|| a.agent_id().cmp(b.agent_id()))
            })
            .map(|record| record.agent_id().to_string())
            .ok_or_else(|| WorkerError::NoEligibleWorker {
                required: task.required_capabilities.clone(),
            })
    }

    pub fn execute_on(&mut self, agent_id: &str, task: &Task) -> Result<Task, WorkerError> {
        let index = self
            .workers
            .iter()
            .position(|record| record.agent_id() == agent_id)
            .ok_or_else(|| WorkerError::UnknownWorker {
                agent_id: agent_id.to_string(),
            })?;

        if !self.workers[index].has_capacity() {
            return Err(WorkerError::WorkerUnavailable {
                agent_id: agent_id.to_string(),
            });
        }

        self.workers[index].active_tasks += 1;
        let result = self.workers[index].worker.execute(task);
        self.workers[index].active_tasks = self.workers[index].active_tasks.saturating_sub(1);
        result
    }

    fn get_mut(&mut self, agent_id: &str) -> Result<&mut WorkerRecord, WorkerError> {
        self.workers
            .iter_mut()
            .find(|record| record.agent_id() == agent_id)
            .ok_or_else(|| WorkerError::UnknownWorker {
                agent_id: agent_id.to_string(),
            })
    }
}

impl Default for WorkerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
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
    #[error("worker is unavailable or at concurrency limit: {agent_id}")]
    WorkerUnavailable { agent_id: String },
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
        let mut registry = WorkerRegistry::with_lease_timeout(1_000);
        registry.register_at(
            LocalWorker::with_capabilities(
                "admin",
                ["code", "test", "repo.write", "deploy.execute", "resource.delete"],
            ),
            WorkerMetadata::default(),
            1_000,
        );
        registry.register_at(
            LocalWorker::with_capabilities("writer", ["code", "test", "repo.write"]),
            WorkerMetadata::default(),
            1_000,
        );

        let task = task_with(&["code", "test", "repo.write"]);
        assert_eq!(registry.select_agent_id_at(&task, 1_500).unwrap(), "writer");
    }

    #[test]
    fn scheduler_excludes_stale_workers() {
        let mut registry = WorkerRegistry::with_lease_timeout(100);
        registry.register_at(
            LocalWorker::new("stale"),
            WorkerMetadata::default(),
            1_000,
        );
        registry.register_at(
            LocalWorker::new("fresh"),
            WorkerMetadata::default(),
            1_950,
        );

        let task = task_with(&["code", "test"]);
        assert_eq!(registry.select_agent_id_at(&task, 2_000).unwrap(), "fresh");
    }

    #[test]
    fn scheduler_respects_concurrency_capacity() {
        let mut registry = WorkerRegistry::with_lease_timeout(1_000);
        registry.register_at(
            LocalWorker::new("busy"),
            WorkerMetadata {
                max_concurrency: 1,
                ..WorkerMetadata::default()
            },
            1_000,
        );
        registry.register_at(
            LocalWorker::new("idle"),
            WorkerMetadata::default(),
            1_000,
        );
        registry.update_load("busy", 1).unwrap();

        let task = task_with(&["code", "test"]);
        assert_eq!(registry.select_agent_id_at(&task, 1_500).unwrap(), "idle");
    }

    #[test]
    fn scheduler_uses_cost_then_latency_after_privilege_and_load() {
        let mut registry = WorkerRegistry::with_lease_timeout(1_000);
        registry.register_at(
            LocalWorker::new("expensive"),
            WorkerMetadata {
                cost_microusd_per_task: 50,
                estimated_latency_ms: 10,
                ..WorkerMetadata::default()
            },
            1_000,
        );
        registry.register_at(
            LocalWorker::new("cheap"),
            WorkerMetadata {
                cost_microusd_per_task: 10,
                estimated_latency_ms: 100,
                ..WorkerMetadata::default()
            },
            1_000,
        );

        let task = task_with(&["code", "test"]);
        assert_eq!(registry.select_agent_id_at(&task, 1_500).unwrap(), "cheap");
    }

    #[test]
    fn registry_rejects_when_no_worker_is_eligible() {
        let mut registry = WorkerRegistry::with_lease_timeout(1_000);
        registry.register_at(
            LocalWorker::new("worker"),
            WorkerMetadata::default(),
            1_000,
        );
        let task = task_with(&["code", "deploy.execute"]);

        assert_eq!(
            registry.select_agent_id_at(&task, 1_500).unwrap_err(),
            WorkerError::NoEligibleWorker {
                required: vec!["code".into(), "deploy.execute".into()],
            }
        );
    }

    #[test]
    fn heartbeat_renews_worker_lease() {
        let mut registry = WorkerRegistry::with_lease_timeout(100);
        registry.register_at(
            LocalWorker::new("worker"),
            WorkerMetadata::default(),
            1_000,
        );
        registry.heartbeat_at("worker", 1_950).unwrap();

        let task = task_with(&["code", "test"]);
        assert_eq!(registry.select_agent_id_at(&task, 2_000).unwrap(), "worker");
    }

    #[test]
    fn registry_replaces_duplicate_agent_registration() {
        let mut registry = WorkerRegistry::with_lease_timeout(1_000);
        registry.register_at(
            LocalWorker::with_capabilities("worker", ["code"]),
            WorkerMetadata::default(),
            1_000,
        );
        registry.register_at(
            LocalWorker::with_capabilities("worker", ["code", "test"]),
            WorkerMetadata::default(),
            1_000,
        );

        assert_eq!(registry.workers().len(), 1);
        assert_eq!(registry.get("worker").unwrap().capabilities().len(), 2);
    }
}
