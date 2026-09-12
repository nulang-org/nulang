//! Director agent: turns user intent into durable goals.

use chrono::Utc;
use nulang_ai_core::{
    intent::{IntentExecutionError, IntentIr},
    Goal, GoalStatus,
};
use uuid::Uuid;

pub trait Director: Send + Sync {
    fn create_goal(
        &self,
        project_id: &str,
        conversation_id: Uuid,
        intent: &str,
        budget_usd: f64,
    ) -> Goal;

    /// Preferred modality-neutral entry point. Intent IR enforces that the
    /// request is confirmed, classified, and explicitly approved when the
    /// risk policy requires confirmation before a durable goal is created.
    fn create_goal_from_intent(
        &self,
        project_id: &str,
        intent: IntentIr,
        budget_usd: f64,
    ) -> Result<Goal, IntentExecutionError> {
        intent.into_goal(project_id, budget_usd)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_core::{
        capability::{IntentCapabilityClassifier, RuleBasedIntentClassifier},
        intent::IntentModality,
    };

    #[test]
    fn classified_read_only_intent_creates_goal() {
        let director = LocalDirector::new("local");
        let mut intent = IntentIr::confirmed(IntentModality::Voice, "review the auth module");
        let classification = RuleBasedIntentClassifier.classify(&intent).unwrap();
        intent.apply_classification(classification);

        let goal = director
            .create_goal_from_intent("nulang", intent, 2.0)
            .unwrap();
        assert_eq!(goal.constraints["execution_risk"], "low");
        assert_eq!(goal.constraints["requested_capabilities"][0], "repo.read");
    }

    #[test]
    fn deployment_is_blocked_until_explicitly_confirmed() {
        let director = LocalDirector::new("local");
        let mut intent = IntentIr::confirmed(IntentModality::Voice, "deploy to production");
        let classification = RuleBasedIntentClassifier.classify(&intent).unwrap();
        intent.apply_classification(classification);

        assert_eq!(
            director.create_goal_from_intent("nulang", intent, 2.0),
            Err(IntentExecutionError::ExplicitConfirmationRequired)
        );
    }
}
