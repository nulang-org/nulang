//! Durable orchestration helpers over the canonical NLAP goal/task graph.
//!
//! `GoalGraph` remains the single task ledger. This module adds fail-closed
//! dependency validation, deterministic readiness queries, and a checkpointable
//! progress ledger for stall detection and replanning.

use crate::{GoalGraph, TaskStatus};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

impl GoalGraph {
    /// Validate task identity, goal ownership, dependency references, and cycles.
    pub fn validate_task_graph(&self) -> Result<(), String> {
        let mut tasks = HashMap::with_capacity(self.tasks.len());
        for task in &self.tasks {
            if task.goal_id != self.goal.id {
                return Err(format!(
                    "Task {} belongs to goal {}, expected {}",
                    task.id, task.goal_id, self.goal.id
                ));
            }
            if tasks.insert(task.id, task).is_some() {
                return Err(format!("Duplicate task id {}", task.id));
            }
        }

        for task in &self.tasks {
            for dependency in &task.dependencies {
                if *dependency == task.id {
                    return Err(format!("Task {} depends on itself", task.id));
                }
                if !tasks.contains_key(dependency) {
                    return Err(format!(
                        "Task {} references unknown dependency {}",
                        task.id, dependency
                    ));
                }
            }
        }

        fn visit(
            id: Uuid,
            tasks: &HashMap<Uuid, &crate::Task>,
            visiting: &mut HashSet<Uuid>,
            visited: &mut HashSet<Uuid>,
        ) -> Result<(), String> {
            if visited.contains(&id) {
                return Ok(());
            }
            if !visiting.insert(id) {
                return Err(format!("Task dependency cycle detected at {}", id));
            }

            let task = tasks
                .get(&id)
                .ok_or_else(|| format!("Task {} not found during validation", id))?;
            for dependency in &task.dependencies {
                visit(*dependency, tasks, visiting, visited)?;
            }

            visiting.remove(&id);
            visited.insert(id);
            Ok(())
        }

        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        for task in &self.tasks {
            visit(task.id, &tasks, &mut visiting, &mut visited)?;
        }
        Ok(())
    }

    /// Return runnable task ids in stable graph order.
    ///
    /// `Created` tasks become runnable when every dependency completed. Tasks
    /// already marked `Ready` remain runnable, but dependencies are still
    /// checked so corrupted state fails closed.
    pub fn ready_task_ids(&self) -> Result<Vec<Uuid>, String> {
        self.validate_task_graph()?;
        let statuses: HashMap<Uuid, TaskStatus> = self
            .tasks
            .iter()
            .map(|task| (task.id, task.status))
            .collect();

        Ok(self
            .tasks
            .iter()
            .filter(|task| matches!(task.status, TaskStatus::Created | TaskStatus::Ready))
            .filter(|task| {
                task.dependencies.iter().all(|dependency| {
                    statuses
                        .get(dependency)
                        .is_some_and(|status| *status == TaskStatus::Completed)
                })
            })
            .map(|task| task.id)
            .collect())
    }

    pub fn completed_task_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .count()
    }

    pub fn all_tasks_completed(&self) -> bool {
        !self.tasks.is_empty()
            && self
                .tasks
                .iter()
                .all(|task| task.status == TaskStatus::Completed)
    }
}

/// Outcome from one coordinator progress check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEvent {
    Advanced,
    Stalled { consecutive: u32 },
    ReplanRequired { consecutive: u32 },
    Complete,
}

/// Checkpointable coordinator progress state bound to one NLAP goal.
///
/// Meaningful progress is intentionally defined as another completed task,
/// rather than another model turn or tool call. This makes repeated reasoning
/// loops visible to the scheduler instead of allowing prompt activity to mask a
/// stalled plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressLedger {
    goal_id: Uuid,
    stall_threshold: u32,
    stall_count: u32,
    replan_count: u32,
    last_completed_count: usize,
    replan_required: bool,
}

impl ProgressLedger {
    pub fn new(goal_id: Uuid, stall_threshold: u32) -> Self {
        Self {
            goal_id,
            stall_threshold: stall_threshold.max(1),
            stall_count: 0,
            replan_count: 0,
            last_completed_count: 0,
            replan_required: false,
        }
    }

    pub fn record_round(&mut self, graph: &GoalGraph) -> Result<ProgressEvent, String> {
        if graph.goal.id != self.goal_id {
            return Err(format!(
                "Progress ledger is bound to goal {}, received {}",
                self.goal_id, graph.goal.id
            ));
        }
        graph.validate_task_graph()?;

        let completed = graph.completed_task_count();
        if graph.all_tasks_completed() {
            self.last_completed_count = completed;
            self.stall_count = 0;
            self.replan_required = false;
            return Ok(ProgressEvent::Complete);
        }

        if completed > self.last_completed_count {
            self.last_completed_count = completed;
            self.stall_count = 0;
            self.replan_required = false;
            return Ok(ProgressEvent::Advanced);
        }

        self.stall_count = self.stall_count.saturating_add(1);
        if self.stall_count >= self.stall_threshold {
            self.replan_required = true;
            Ok(ProgressEvent::ReplanRequired {
                consecutive: self.stall_count,
            })
        } else {
            Ok(ProgressEvent::Stalled {
                consecutive: self.stall_count,
            })
        }
    }

    /// Record that the coordinator consumed the replan signal and revised its
    /// plan. Task mutations remain explicit on the canonical `GoalGraph`.
    pub fn acknowledge_replan(&mut self) {
        if self.replan_required {
            self.replan_count = self.replan_count.saturating_add(1);
        }
        self.stall_count = 0;
        self.replan_required = false;
    }

    pub fn goal_id(&self) -> Uuid {
        self.goal_id
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
