//! Capability and execution-risk classification for confirmed Intent IR.
//!
//! The classifier is deliberately deterministic and conservative. It provides
//! a safe baseline before any model-assisted classifier is introduced: risky
//! verbs map to explicit capabilities and higher execution risk, while unknown
//! intents remain non-executable until classified by a stronger policy layer.

use crate::intent::{IntentIr, IntentPhase, IntentSafety};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRisk {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntentClassification {
    pub safety: IntentSafety,
    pub risk: ExecutionRisk,
    pub required_capabilities: Vec<String>,
    pub requires_confirmation: bool,
    pub rationale: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentClassificationError {
    ProvisionalIntent,
    EmptyIntent,
}

pub trait IntentCapabilityClassifier: Send + Sync {
    fn classify(
        &self,
        intent: &IntentIr,
    ) -> Result<IntentClassification, IntentClassificationError>;
}

/// Conservative lexical baseline for common engineering actions.
///
/// This is not intended to be the final semantic classifier. Its purpose is to
/// make dangerous actions structurally distinct before tool invocation and to
/// provide a deterministic fallback when a model classifier is unavailable.
#[derive(Debug, Default, Clone, Copy)]
pub struct RuleBasedIntentClassifier;

impl RuleBasedIntentClassifier {
    fn contains_any(text: &str, terms: &[&str]) -> bool {
        terms.iter().any(|term| text.contains(term))
    }

    fn classification(
        safety: IntentSafety,
        risk: ExecutionRisk,
        capabilities: &[&str],
        requires_confirmation: bool,
        rationale: &str,
    ) -> IntentClassification {
        IntentClassification {
            safety,
            risk,
            required_capabilities: capabilities
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            requires_confirmation,
            rationale: rationale.to_string(),
        }
    }
}

impl IntentCapabilityClassifier for RuleBasedIntentClassifier {
    fn classify(
        &self,
        intent: &IntentIr,
    ) -> Result<IntentClassification, IntentClassificationError> {
        if intent.phase != IntentPhase::Confirmed {
            return Err(IntentClassificationError::ProvisionalIntent);
        }

        let text = intent.text.trim().to_ascii_lowercase();
        if text.is_empty() {
            return Err(IntentClassificationError::EmptyIntent);
        }

        if Self::contains_any(
            &text,
            &[
                "delete",
                "destroy",
                "drop database",
                "wipe",
                "purge",
                "terminate environment",
            ],
        ) {
            return Ok(Self::classification(
                IntentSafety::Irreversible,
                ExecutionRisk::Critical,
                &["resource.delete"],
                true,
                "destructive resource mutation",
            ));
        }

        if Self::contains_any(
            &text,
            &[
                "deploy",
                "release",
                "publish",
                "promote to production",
                "roll out",
            ],
        ) {
            return Ok(Self::classification(
                IntentSafety::Reversible,
                ExecutionRisk::High,
                &["deploy.execute"],
                true,
                "externally visible deployment or release",
            ));
        }

        if Self::contains_any(
            &text,
            &[
                "merge pull request",
                "merge pr",
                "merge this pr",
                "merge the pr",
            ],
        ) {
            return Ok(Self::classification(
                IntentSafety::Reversible,
                ExecutionRisk::High,
                &["repo.read", "repo.pull_request.merge"],
                true,
                "repository history mutation",
            ));
        }

        if Self::contains_any(
            &text,
            &[
                "open a pr",
                "open pr",
                "create a pr",
                "create pr",
                "pull request",
            ],
        ) {
            return Ok(Self::classification(
                IntentSafety::Reversible,
                ExecutionRisk::Medium,
                &["repo.read", "repo.pull_request.write"],
                false,
                "reviewable repository mutation",
            ));
        }

        if Self::contains_any(
            &text,
            &[
                "commit",
                "push",
                "edit",
                "modify",
                "change",
                "implement",
                "fix",
                "refactor",
            ],
        ) {
            return Ok(Self::classification(
                IntentSafety::Reversible,
                ExecutionRisk::Medium,
                &["repo.read", "repo.write"],
                false,
                "repository content mutation",
            ));
        }

        if Self::contains_any(
            &text,
            &[
                "inspect",
                "review",
                "search",
                "find",
                "list",
                "read",
                "summarize",
                "explain",
                "analyze",
                "check",
            ],
        ) {
            return Ok(Self::classification(
                IntentSafety::ReadOnly,
                ExecutionRisk::Low,
                &["repo.read"],
                false,
                "read-only repository operation",
            ));
        }

        Ok(Self::classification(
            IntentSafety::Unknown,
            ExecutionRisk::High,
            &[],
            true,
            "intent did not match a known safe execution class",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::{IntentIr, IntentModality};

    fn classify(text: &str) -> IntentClassification {
        RuleBasedIntentClassifier
            .classify(&IntentIr::confirmed(IntentModality::Voice, text))
            .unwrap()
    }

    #[test]
    fn review_is_read_only_low_risk() {
        let result = classify("review the auth module");
        assert_eq!(result.safety, IntentSafety::ReadOnly);
        assert_eq!(result.risk, ExecutionRisk::Low);
        assert_eq!(result.required_capabilities, vec!["repo.read"]);
        assert!(!result.requires_confirmation);
    }

    #[test]
    fn pull_request_write_is_medium_risk() {
        let result = classify("open a PR with the fix");
        assert_eq!(result.safety, IntentSafety::Reversible);
        assert_eq!(result.risk, ExecutionRisk::Medium);
        assert!(result
            .required_capabilities
            .contains(&"repo.pull_request.write".into()));
    }

    #[test]
    fn deploy_requires_confirmation() {
        let result = classify("deploy this service to production");
        assert_eq!(result.risk, ExecutionRisk::High);
        assert!(result.requires_confirmation);
        assert_eq!(result.required_capabilities, vec!["deploy.execute"]);
    }

    #[test]
    fn destructive_action_is_critical_and_irreversible() {
        let result = classify("delete the production environment");
        assert_eq!(result.safety, IntentSafety::Irreversible);
        assert_eq!(result.risk, ExecutionRisk::Critical);
        assert!(result.requires_confirmation);
    }

    #[test]
    fn provisional_intents_are_never_classified_for_execution() {
        let intent = IntentIr::provisional(IntentModality::Voice, "deploy to production");
        assert_eq!(
            RuleBasedIntentClassifier.classify(&intent),
            Err(IntentClassificationError::ProvisionalIntent)
        );
    }
}
