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

/// Non-serialized proof that classification was applied to this exact semantic
/// payload. Public classification fields remain useful for inspection and
/// transport, but they are not authoritative for execution.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedClassification {
    text: String,
    safety: IntentSafety,
    risk: ExecutionRisk,
    requested_capabilities: Vec<String>,
    requires_confirmation: bool,
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
    /// Classification authority is process-local and cannot be forged by
    /// deserializing an Intent IR supplied by an external caller.
    #[serde(skip)]
    classification: Option<AppliedClassification>,
    /// Confirmation authority is likewise private. The public boolean is only
    /// an observable mirror and cannot grant execution on its own.
    #[serde(skip)]
    confirmation_granted: bool,
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
            classification: None,
            confirmation_granted: false,
        }
    }

    pub fn provisional(modality: IntentModality, text: impl Into<String>) -> Self {
        Self::new(modality, IntentPhase::Provisional, text)
    }

    pub fn confirmed(modality: IntentModality, text: impl Into<String>) -> Self {
        Self::new(modality, IntentPhase::Confirmed, text)
    }

    pub fn is_executable(&self) -> bool {
        if self.phase != IntentPhase::Confirmed {
            return false;
        }
        let Ok(classification) = self.validated_classification() else {
            return false;
        };
        classification.safety != IntentSafety::Unknown
            && (!classification.requires_confirmation || self.confirmation_granted)
    }

    pub fn apply_classification(&mut self, classification: IntentClassification) {
        let snapshot = AppliedClassification {
            text: self.text.clone(),
            safety: classification.safety,
            risk: classification.risk,
            requested_capabilities: classification.required_capabilities.clone(),
            requires_confirmation: classification.requires_confirmation,
        };

        self.safety = snapshot.safety;
        self.execution_risk = Some(snapshot.risk);
        self.requested_capabilities = snapshot.requested_capabilities.clone();
        self.requires_confirmation = snapshot.requires_confirmation;
        self.confirmation_granted = !snapshot.requires_confirmation;
        self.execution_confirmed = self.confirmation_granted;
        self.metadata["classification_rationale"] =
            serde_json::Value::String(classification.rationale);
        self.classification = Some(snapshot);
    }

    pub fn confirm_execution(&mut self) -> Result<(), IntentExecutionError> {
        if self.phase != IntentPhase::Confirmed {
            return Err(IntentExecutionError::ProvisionalIntent);
        }
        let classification = self.validated_classification()?;
        if classification.safety == IntentSafety::Unknown {
            return Err(IntentExecutionError::UnclassifiedIntent);
        }
        self.confirmation_granted = true;
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
        let classification = self.validated_classification()?.clone();
        if classification.safety == IntentSafety::Unknown {
            return Err(IntentExecutionError::UnclassifiedIntent);
        }
        if classification.requires_confirmation && !self.confirmation_granted {
            return Err(IntentExecutionError::ExplicitConfirmationRequired);
        }

        let mut goal = Goal::new(project_id, self.text, budget_usd);
        goal.conversation_id = self.conversation_id;
        goal.constraints = serde_json::json!({
            "intent_id": self.id,
            "modality": self.modality,
            "safety": classification.safety,
            "execution_risk": classification.risk,
            "requested_capabilities": classification.requested_capabilities,
            "execution_confirmed": self.confirmation_granted,
        });
        Ok(goal)
    }

    fn validated_classification(&self) -> Result<&AppliedClassification, IntentExecutionError> {
        let Some(classification) = self.classification.as_ref() else {
            return Err(IntentExecutionError::UnclassifiedIntent);
        };

        let mirrors_match = classification.text == self.text
            && classification.safety == self.safety
            && Some(classification.risk) == self.execution_risk
            && classification.requested_capabilities == self.requested_capabilities
            && classification.requires_confirmation == self.requires_confirmation
            && self.execution_confirmed == self.confirmation_granted;

        if !mirrors_match {
            return Err(IntentExecutionError::ClassificationStale);
        }
        Ok(classification)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentExecutionError {
    ProvisionalIntent,
    UnclassifiedIntent,
    ClassificationStale,
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

    #[test]
    fn mutating_text_after_classification_invalidates_authority() {
        let mut intent = IntentIr::confirmed(IntentModality::Text, "review the auth module");
        let classification = RuleBasedIntentClassifier.classify(&intent).unwrap();
        intent.apply_classification(classification);
        intent.text = "delete production".into();

        assert_eq!(
            intent.into_goal("nulang", 2.0),
            Err(IntentExecutionError::ClassificationStale)
        );
    }

    #[test]
    fn public_confirmation_flag_cannot_forge_approval() {
        let mut intent = IntentIr::confirmed(IntentModality::Text, "deploy to production");
        let classification = RuleBasedIntentClassifier.classify(&intent).unwrap();
        intent.apply_classification(classification);
        intent.execution_confirmed = true;

        assert_eq!(
            intent.into_goal("nulang", 2.0),
            Err(IntentExecutionError::ClassificationStale)
        );
    }

    #[test]
    fn deserialization_does_not_restore_execution_authority() {
        let mut intent = IntentIr::confirmed(IntentModality::Api, "review the auth module");
        let classification = RuleBasedIntentClassifier.classify(&intent).unwrap();
        intent.apply_classification(classification);
        let json = serde_json::to_string(&intent).unwrap();
        let restored: IntentIr = serde_json::from_str(&json).unwrap();

        assert!(!restored.is_executable());
        assert_eq!(
            restored.into_goal("nulang", 2.0),
            Err(IntentExecutionError::UnclassifiedIntent)
        );
    }
}
