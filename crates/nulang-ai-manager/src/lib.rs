//! Manager agents decompose goals into executable tasks.

use chrono::Utc;
use nulang_ai_core::{ManagerKind, MissionSpec, Task, TaskStatus};
use std::time::Duration;
use uuid::Uuid;

pub trait Manager: Send + Sync {
    fn manager_kind(&self) -> ManagerKind;
    fn plan_tasks(&self, goal_id: Uuid, intent: &str, budget_usd: f64) -> Vec<Task>;

    /// Plan work for a typed mission contract.
    ///
    /// The manager still owns decomposition, while the shared runtime policy
    /// supplies hard ceilings and capabilities. Keeping this as a default
    /// method lets specialized managers opt into mission execution without
    /// duplicating budget/capability plumbing.
    fn plan_mission(&self, mission: &MissionSpec) -> Vec<Task> {
        let effective_budget = mission
            .goal
            .budget_usd
            .min(mission.budget.max_cost_usd)
            .max(0.0);
        let mut tasks = self.plan_tasks(
            mission.goal.id,
            &mission.goal.intent,
            effective_budget,
        );

        tasks.truncate(mission.budget.max_tasks as usize);
        if tasks.is_empty() {
            return tasks;
        }

        // A manager may request more aggregate spend than the mission allows.
        // Scale all task budgets proportionally instead of silently allowing
        // the mission to exceed its contract.
        let requested_total: f64 = tasks.iter().map(|task| task.budget_usd.max(0.0)).sum();
        if requested_total > effective_budget && requested_total > 0.0 {
            let scale = effective_budget / requested_total;
            for task in &mut tasks {
                task.budget_usd = task.budget_usd.max(0.0) * scale;
            }
        }

        for task in &mut tasks {
            if task.timeout > mission.budget.max_duration {
                task.timeout = mission.budget.max_duration;
            }

            for capability in &mission.required_capabilities {
                if !task.required_capabilities.contains(capability) {
                    task.required_capabilities.push(capability.clone());
                }
            }

            if task.acceptance_criteria.is_empty() {
                task.acceptance_criteria
                    .extend(mission.goal.success_criteria.iter().cloned());
            }
        }

        tasks
    }
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
    use nulang_ai_core::{Goal, MissionBudget, MissionSpec};

    #[test]
    fn mission_policy_is_applied_to_planned_tasks() {
        let goal = Goal::new("demo", "ship feature", 10.0);
        let mut mission = MissionSpec::from_goal(goal);
        mission.required_capabilities = vec!["deploy.production".into()];
        mission.budget = MissionBudget {
            max_cost_usd: 4.0,
            max_tasks: 4,
            max_parallelism: 2,
            max_duration: Duration::from_secs(30),
            max_tokens: None,
        };
        mission.goal.budget_usd = 4.0;

        let tasks = EngineeringManager.plan_mission(&mission);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0]
            .required_capabilities
            .contains(&"deploy.production".to_string()));
        assert!(tasks[0].timeout <= Duration::from_secs(30));
        assert!(tasks.iter().map(|task| task.budget_usd).sum::<f64>() <= 4.0);
    }
}