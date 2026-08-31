//! Director agent: turns user intent into durable goals.

use chrono::Utc;
use nulang_ai_core::{Goal, GoalStatus};
use uuid::Uuid;

pub trait Director: Send + Sync {
    fn create_goal(
        &self,
        project_id: &str,
        conversation_id: Uuid,
        intent: &str,
        budget_usd: f64,
    ) -> Goal;
}

pub struct LocalDirector {
    pub id: String,
}

impl LocalDirector {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

impl Director for LocalDirector {
    fn create_goal(
        &self,
        project_id: &str,
        conversation_id: Uuid,
        intent: &str,
        budget_usd: f64,
    ) -> Goal {
        let now = Utc::now();
        Goal {
            id: Uuid::new_v4(),
            project_id: project_id.to_string(),
            conversation_id: Some(conversation_id),
            intent: intent.to_string(),
            desired_state: serde_json::json!({"summary": intent}),
            constraints: serde_json::json!({}),
            success_criteria: vec![format!("Deliver outcome for: {}", intent)],
            budget_usd,
            deadline: None,
            status: GoalStatus::Created,
            created_at: now,
            updated_at: now,
        }
    }
}
