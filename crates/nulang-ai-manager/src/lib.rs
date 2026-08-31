//! Manager agents decompose goals into executable tasks.

use chrono::Utc;
use nulang_ai_core::{ManagerKind, Task, TaskStatus};
use std::time::Duration;
use uuid::Uuid;

pub trait Manager: Send + Sync {
    fn manager_kind(&self) -> ManagerKind;
    fn plan_tasks(&self, goal_id: Uuid, intent: &str, budget_usd: f64) -> Vec<Task>;
}

pub struct EngineeringManager;

impl Manager for EngineeringManager {
    fn manager_kind(&self) -> ManagerKind {
        ManagerKind::Engineering
    }

    fn plan_tasks(&self, goal_id: Uuid, intent: &str, budget_usd: f64) -> Vec<Task> {
        let now = Utc::now();
        vec![Task {
            id: Uuid::new_v4(),
            goal_id,
            parent_task_id: None,
            manager: ManagerKind::Engineering,
            description: format!("Engineering plan for: {}", intent),
            dependencies: Vec::new(),
            required_capabilities: vec!["code".into(), "test".into()],
            acceptance_criteria: vec!["Tests pass".into()],
            budget_usd: budget_usd * 0.5,
            timeout: Duration::from_secs(3600),
            status: TaskStatus::Created,
            assigned_agent_id: Some("worker-local".into()),
            created_at: now,
            updated_at: now,
        }]
    }
}
