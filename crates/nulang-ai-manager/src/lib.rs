//! Manager agents decompose goals into executable tasks.

use chrono::Utc;
use nulang_ai_core::{Goal, ManagerKind, Task, TaskStatus};
use std::time::Duration;
use uuid::Uuid;

pub trait Manager: Send + Sync {
    fn manager_kind(&self) -> ManagerKind;
    fn plan_tasks(&self, goal_id: Uuid, intent: &str, budget_usd: f64) -> Vec<Task>;

    /// Plan tasks from a durable goal while preserving capability constraints.
    ///
    /// Capability propagation is a defense-in-depth boundary: even if a
    /// planner forgets a requirement, capabilities attached by Intent IR are
    /// inherited by every resulting task before assignment.
    fn plan_goal(&self, goal: &Goal) -> Vec<Task> {
        let mut tasks = self.plan_tasks(goal.id, &goal.intent, goal.budget_usd);
        let inherited = goal_capabilities(goal);
        for task in &mut tasks {
            for capability in &inherited {
                if !task.required_capabilities.contains(capability) {
                    task.required_capabilities.push(capability.clone());
                }
            }
        }
        tasks
    }
}

fn goal_capabilities(goal: &Goal) -> Vec<String> {
    goal.constraints
        .get("requested_capabilities")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_capabilities_are_inherited_by_tasks() {
        let mut goal = Goal::new("nulang", "deploy release", 10.0);
        goal.constraints = serde_json::json!({
            "requested_capabilities": ["repo.read", "deploy.execute"]
        });

        let tasks = EngineeringManager.plan_goal(&goal);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].required_capabilities.contains(&"code".into()));
        assert!(tasks[0].required_capabilities.contains(&"repo.read".into()));
        assert!(tasks[0]
            .required_capabilities
            .contains(&"deploy.execute".into()));
    }

    #[test]
    fn inherited_capabilities_are_deduplicated() {
        let mut goal = Goal::new("nulang", "review code", 1.0);
        goal.constraints = serde_json::json!({
            "requested_capabilities": ["code", "repo.read"]
        });

        let task = EngineeringManager.plan_goal(&goal).remove(0);
        assert_eq!(
            task.required_capabilities
                .iter()
                .filter(|capability| capability.as_str() == "code")
                .count(),
            1
        );
    }
}
