//! Director agent: turns classified user intent into durable goals.

use nulang_ai_core::{
    intent::{IntentExecutionError, IntentIr},
    Goal,
};

pub trait Director: Send + Sync {
    /// Modality-neutral goal creation boundary.
    ///
    /// Intent IR enforces that the request is confirmed, classified, and
    /// explicitly approved when the risk policy requires confirmation before a
    /// durable goal can exist. There is intentionally no raw-text goal creation
    /// method on this trait: callers must cross the semantic safety boundary.
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

impl Director for LocalDirector {}

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
