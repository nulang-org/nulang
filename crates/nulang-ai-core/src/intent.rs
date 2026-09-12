//! Provider-neutral Intent IR shared by text, voice, API, IDE, and agent inputs.
//!
//! Intent IR is the semantic boundary between user-facing modalities and
//! capability-checked execution. Provisional intents may drive read-only
//! speculation, but only confirmed, classified intents may become executable goals.

use crate::{
    capability::{ExecutionRisk, IntentClassification},
    Goal,
};
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
    pub execution_risk: Option<ExecutionRisk>,
    pub requested_capabilities: Vec<String>,
    pub requires_confirmation: bool,
    pub execution_confirmed: bool,
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
            execution_risk: None,
            requested_capabilities: Vec::new(),
            requires_confirmation: false,
            execution_confirmed: false,
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
            && self.execution_risk.is_some()
            && self.safety != IntentSafety::Unknown
            && (!self.requires_confirmation || self.execution_confirmed)
    }

    pub fn apply_classification(&mut self, classification: IntentClassification) {
        self.safety = classification.safety;
        self.execution_risk = Some(classification.risk);
        self.requested_capabilities = classification.required_capabilities;
        self.requires_confirmation = classification.requires_confirmation;
        self.execution_confirmed = !classification.requires_confirmation;
        self.metadata["classification_rationale"] = serde_json::Value::String(classification.rationale);
    }

    pub fn confirm_execution(&mut self) -> Result<(), IntentExecutionError> {
        if self.phase != IntentPhase::Confirmed {
            return Err(IntentExecutionError::ProvisionalIntent);
        }
        if self.execution_risk.is_none() || self.safety == IntentSafety::Unknown {
            return Err(IntentExecutionError::UnclassifiedIntent);
        }
        self.execution_confirmed = true;
        Ok(())
    }

    pub fn into_goal(
        self,
        project_id: impl Into<String>,
        budget_usd: f64,
    ) -> Result<Goal, IntentExecutionError> {
        if self.phase != IntentPhase::Confirmed {
            return Err(IntentExecutionError::ProvisionalIntent);
        }
        if self.execution_risk.is_none() || self.safety == IntentSafety::Unknown {
            return Err(IntentExecutionError::UnclassifiedIntent);
        }
        if self.requires_confirmation && !self.execution_confirmed {
            return Err(IntentExecutionError::ExplicitConfirmationRequired);
        }

        let mut goal = Goal::new(project_id, self.text, budget_usd);
        goal.conversation_id = self.conversation_id;
        goal.constraints = serde_json::json!({
            "intent_id": self.id,
            "modality": self.modality,
            "safety": self.safety,
            "execution_risk": self.execution_risk,
            "requested_capabilities": self.requested_capabilities,
            "execution_confirmed": self.execution_confirmed,
        });
        Ok(goal)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentExecutionError {
    ProvisionalIntent,
    UnclassifiedIntent,
    ExplicitConfirmationRequired,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{IntentCapabilityClassifier, RuleBasedIntentClassifier};

    #[test]
    fn provisional_intent_cannot_become_goal() {
        let intent = IntentIr::provisional(IntentModality::Voice, "search the repository");
        assert_eq!(
            intent.into_goal("nulang", 1.0),
            Err(IntentExecutionError::ProvisionalIntent)
        );
    }

    #[test]
    fn unclassified_confirmed_intent_cannot_become_goal() {
        let intent = IntentIr::confirmed(IntentModality::Voice, "do something useful");
        assert_eq!(
            intent.into_goal("nulang", 1.0),
            Err(IntentExecutionError::UnclassifiedIntent)
        );
    }

    #[test]
    fn classified_read_only_intent_becomes_goal() {
        let conversation_id = Uuid::new_v4();
        let mut intent = IntentIr::confirmed(IntentModality::Voice, "review the auth module");
        intent.conversation_id = Some(conversation_id);
        let classification = RuleBasedIntentClassifier.classify(&intent).unwrap();
        intent.apply_classification(classification);

        let goal = intent.into_goal("nulang", 2.0).unwrap();
        assert_eq!(goal.conversation_id, Some(conversation_id));
        assert_eq!(goal.intent, "review the auth module");
        assert_eq!(goal.constraints["safety"], "read_only");
        assert_eq!(goal.constraints["execution_risk"], "low");
    }

    #[test]
    fn high_risk_intent_requires_explicit_confirmation() {
        let mut intent = IntentIr::confirmed(IntentModality::Voice, "deploy to production");
        let classification = RuleBasedIntentClassifier.classify(&intent).unwrap();
        intent.apply_classification(classification);

        assert_eq!(
            intent.clone().into_goal("nulang", 2.0),
            Err(IntentExecutionError::ExplicitConfirmationRequired)
        );

        intent.confirm_execution().unwrap();
        assert!(intent.into_goal("nulang", 2.0).is_ok());
    }
}
