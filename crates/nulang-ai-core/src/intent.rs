//! Provider-neutral Intent IR shared by text, voice, API, IDE, and agent inputs.
//!
//! Intent IR is the semantic boundary between user-facing modalities and
//! capability-checked execution. Provisional intents may drive read-only
//! speculation, but only confirmed intents may become executable goals.

use crate::Goal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentModality {
    Text,
    Voice,
    Api,
    Ide,
    Agent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentPhase {
    Provisional,
    Confirmed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentSafety {
    ReadOnly,
    Reversible,
    Irreversible,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntentIr {
    pub id: Uuid,
    pub conversation_id: Option<Uuid>,
    pub source_session_id: Option<Uuid>,
    pub modality: IntentModality,
    pub phase: IntentPhase,
    pub text: String,
    pub language: Option<String>,
    pub confidence: Option<f32>,
    pub safety: IntentSafety,
    pub requested_capabilities: Vec<String>,
    pub metadata: serde_json::Value,
}

impl IntentIr {
    pub fn new(modality: IntentModality, phase: IntentPhase, text: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            conversation_id: None,
            source_session_id: None,
            modality,
            phase,
            text: text.into(),
            language: None,
            confidence: None,
            safety: IntentSafety::Unknown,
            requested_capabilities: Vec::new(),
            metadata: serde_json::json!({}),
        }
    }

    pub fn provisional(modality: IntentModality, text: impl Into<String>) -> Self {
        Self::new(modality, IntentPhase::Provisional, text)
    }

    pub fn confirmed(modality: IntentModality, text: impl Into<String>) -> Self {
        Self::new(modality, IntentPhase::Confirmed, text)
    }

    pub fn is_executable(&self) -> bool {
        self.phase == IntentPhase::Confirmed
    }

    pub fn into_goal(
        self,
        project_id: impl Into<String>,
        budget_usd: f64,
    ) -> Result<Goal, IntentExecutionError> {
        if !self.is_executable() {
            return Err(IntentExecutionError::ProvisionalIntent);
        }

        let mut goal = Goal::new(project_id, self.text, budget_usd);
        goal.conversation_id = self.conversation_id;
        goal.constraints = serde_json::json!({
            "intent_id": self.id,
            "modality": self.modality,
            "safety": self.safety,
            "requested_capabilities": self.requested_capabilities,
        });
        Ok(goal)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentExecutionError {
    ProvisionalIntent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisional_intent_cannot_become_goal() {
        let intent = IntentIr::provisional(IntentModality::Voice, "search the repository");
        assert_eq!(
            intent.into_goal("nulang", 1.0),
            Err(IntentExecutionError::ProvisionalIntent)
        );
    }

    #[test]
    fn confirmed_intent_becomes_goal_and_preserves_execution_metadata() {
        let conversation_id = Uuid::new_v4();
        let mut intent = IntentIr::confirmed(IntentModality::Voice, "review the auth module");
        intent.conversation_id = Some(conversation_id);
        intent.safety = IntentSafety::ReadOnly;
        intent.requested_capabilities = vec!["repo.read".into()];

        let goal = intent.into_goal("nulang", 2.0).unwrap();
        assert_eq!(goal.conversation_id, Some(conversation_id));
        assert_eq!(goal.intent, "review the auth module");
        assert_eq!(goal.constraints["safety"], "read_only");
    }
}
