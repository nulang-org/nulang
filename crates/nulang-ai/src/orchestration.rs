//! Durable, model-agnostic orchestration state for agent systems.
//!
//! These types intentionally contain no runtime or provider dependencies. A
//! caller can checkpoint them through any persistence layer, restore them, and
//! continue orchestration without reconstructing state from an LLM transcript.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Lifecycle of a unit of agent work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Blocked,
    Failed,
    Cancelled,
}

/// One durable unit of work in an orchestration run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: u64,
    pub title: String,
    pub dependencies: Vec<u64>,
    pub status: TaskStatus,
    pub assigned_agent: Option<u64>,
    pub result: Option<String>,
    pub error: Option<String>,
}

/// Deterministic task ledger suitable for checkpointing and replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLedger {
    next_id: u64,
    tasks: BTreeMap<u64, TaskRecord>,
}

impl Default for TaskLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskLedger {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            tasks: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn task(&self, id: u64) -> Option<&TaskRecord> {
        self.tasks.get(&id)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &TaskRecord> {
        self.tasks.values()
    }

    /// Add work whose dependencies must already exist in this ledger.
    ///
    /// Requiring existing dependencies prevents dangling task graphs from being
    /// checkpointed and later failing during scheduling.
    pub fn add_task(
        &mut self,
        title: impl Into<String>,
        mut dependencies: Vec<u64>,
    ) -> Result<u64, String> {
        dependencies.sort_unstable();
        dependencies.dedup();
        for dependency in &dependencies {
            if !self.tasks.contains_key(dependency) {
                return Err(format!("Task dependency {} not found", dependency));
            }
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        self.tasks.insert(
            id,
            TaskRecord {
                id,
                title: title.into(),
                dependencies,
                status: TaskStatus::Pending,
                assigned_agent: None,
                result: None,
                error: None,
            },
        );
        Ok(id)
    }

    /// Pending tasks for which every dependency completed successfully.
    pub fn ready_task_ids(&self) -> Vec<u64> {
        self.tasks
            .values()
            .filter(|task| {
                task.status == TaskStatus::Pending
                    && task.dependencies.iter().all(|dependency| {
                        self.tasks
                            .get(dependency)
                            .is_some_and(|task| task.status == TaskStatus::Completed)
                    })
            })
            .map(|task| task.id)
            .collect()
    }

    pub fn start(&mut self, id: u64, agent_id: u64) -> Result<(), String> {
        let dependencies_ready = {
            let task = self
                .tasks
                .get(&id)
                .ok_or_else(|| format!("Task {} not found", id))?;
            if task.status != TaskStatus::Pending {
                return Err(format!(
                    "Task {} cannot start from {:?}",
                    id, task.status
                ));
            }
            task.dependencies.iter().all(|dependency| {
                self.tasks
                    .get(dependency)
                    .is_some_and(|task| task.status == TaskStatus::Completed)
            })
        };

        if !dependencies_ready {
            return Err(format!("Task {} has incomplete dependencies", id));
        }

        let task = self.tasks.get_mut(&id).expect("task checked above");
        task.status = TaskStatus::Running;
        task.assigned_agent = Some(agent_id);
        task.error = None;
        Ok(())
    }

    pub fn complete(&mut self, id: u64, result: impl Into<String>) -> Result<(), String> {
        let task = self
            .tasks
            .get_mut(&id)
            .ok_or_else(|| format!("Task {} not found", id))?;
        if task.status != TaskStatus::Running {
            return Err(format!(
                "Task {} cannot complete from {:?}",
                id, task.status
            ));
        }
        task.status = TaskStatus::Completed;
        task.result = Some(result.into());
        task.error = None;
        Ok(())
    }

    pub fn block(&mut self, id: u64, reason: impl Into<String>) -> Result<(), String> {
        self.finish_non_success(id, TaskStatus::Blocked, reason.into())
    }

    pub fn fail(&mut self, id: u64, error: impl Into<String>) -> Result<(), String> {
        self.finish_non_success(id, TaskStatus::Failed, error.into())
    }

    pub fn cancel(&mut self, id: u64, reason: impl Into<String>) -> Result<(), String> {
        self.finish_non_success(id, TaskStatus::Cancelled, reason.into())
    }

    fn finish_non_success(
        &mut self,
        id: u64,
        status: TaskStatus,
        reason: String,
    ) -> Result<(), String> {
        let task = self
            .tasks
            .get_mut(&id)
            .ok_or_else(|| format!("Task {} not found", id))?;
        if matches!(
            task.status,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        ) {
            return Err(format!(
                "Task {} cannot transition from {:?} to {:?}",
                id, task.status, status
            ));
        }
        task.status = status;
        task.error = Some(reason);
        Ok(())
    }

    pub fn completed_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|task| task.status == TaskStatus::Completed)
            .count()
    }

    pub fn all_completed(&self) -> bool {
        !self.tasks.is_empty()
            && self
                .tasks
                .values()
                .all(|task| task.status == TaskStatus::Completed)
    }
}

/// Outcome from one coordinator progress check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProgressEvent {
    Advanced,
    Stalled { consecutive: u32 },
    ReplanRequired { consecutive: u32 },
    Complete,
}

/// Durable coordinator progress state.
///
/// Progress is intentionally based on completed work rather than prompt turns or
/// tool calls. Repeated rounds without another completed task are treated as a
/// stall and eventually require an explicit replan acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressLedger {
    stall_threshold: u32,
    stall_count: u32,
    replan_count: u32,
    last_completed_count: usize,
    replan_required: bool,
}

impl Default for ProgressLedger {
    fn default() -> Self {
        Self::new(3)
    }
}

impl ProgressLedger {
    pub fn new(stall_threshold: u32) -> Self {
        Self {
            stall_threshold: stall_threshold.max(1),
            stall_count: 0,
            replan_count: 0,
            last_completed_count: 0,
            replan_required: false,
        }
    }

    pub fn record_round(&mut self, tasks: &TaskLedger) -> ProgressEvent {
        let completed = tasks.completed_count();

        if tasks.all_completed() {
            self.last_completed_count = completed;
            self.stall_count = 0;
            self.replan_required = false;
            return ProgressEvent::Complete;
        }

        if completed > self.last_completed_count {
            self.last_completed_count = completed;
            self.stall_count = 0;
            self.replan_required = false;
            return ProgressEvent::Advanced;
        }

        self.stall_count = self.stall_count.saturating_add(1);
        if self.stall_count >= self.stall_threshold {
            self.replan_required = true;
            ProgressEvent::ReplanRequired {
                consecutive: self.stall_count,
            }
        } else {
            ProgressEvent::Stalled {
                consecutive: self.stall_count,
            }
        }
    }

    /// Record that the coordinator consumed the replan signal and replaced or
    /// revised its plan. This does not mutate the task ledger itself.
    pub fn acknowledge_replan(&mut self) {
        if self.replan_required {
            self.replan_count = self.replan_count.saturating_add(1);
        }
        self.stall_count = 0;
        self.replan_required = false;
    }

    pub fn stall_count(&self) -> u32 {
        self.stall_count
    }

    pub fn replan_count(&self) -> u32 {
        self.replan_count
    }

    pub fn replan_required(&self) -> bool {
        self.replan_required
    }
}
