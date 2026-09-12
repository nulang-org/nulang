//! Runtime-aware scheduling candidates for Nulang Cloud.
//!
//! The capacity broker ranks raw infrastructure offers economically. This
//! module adds the second scheduling dimension: whether a concrete runtime pool
//! can actually satisfy the runtime-feature and isolation requirements derived
//! by deployment admission.
//!
//! It intentionally does not depend on the compiler/runtime crate. The hosted
//! control plane can translate a compiled deployment manifest/admission result
//! into [`ExecutionRequirements`] and translate a configured worker pool into
//! [`RuntimeEnvelope`] without creating a package dependency cycle.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::broker::PlacementCandidate;

/// Runtime/security facts for a concrete worker pool or execution target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEnvelope {
    /// Loadable runtime subsystems available on this target.
    #[serde(default)]
    pub runtime_features: BTreeSet<String>,
    /// Control-plane-defined isolation strength. Higher values represent a
    /// target considered at least as isolated as lower values.
    pub isolation_level: u8,
}

/// Scheduler requirements derived from authoritative deployment admission.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRequirements {
    /// Runtime subsystems that must be present on the target.
    #[serde(default)]
    pub required_runtime_features: BTreeSet<String>,
    pub minimum_isolation_level: u8,
}

/// Economically ranked capacity bound to a concrete runtime pool.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionTarget {
    /// Stable control-plane identity for the worker pool/image/runtime target.
    pub target_id: String,
    /// Provider/region/cost/reliability candidate from the capacity broker.
    pub placement: PlacementCandidate,
    /// Runtime features and isolation actually supplied by this target.
    pub runtime: RuntimeEnvelope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ExecutionTargetReason {
    MissingRuntimeFeature { feature: String },
    IsolationLevelInsufficient { required: u8, actual: u8 },
}

/// Deterministic pre-scheduling eligibility result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionTargetDecision {
    pub eligible: bool,
    pub reasons: Vec<ExecutionTargetReason>,
}

/// Check whether a concrete execution target can satisfy admitted runtime
/// requirements. Reasons are deterministic because feature sets are ordered.
pub fn evaluate_execution_target(
    requirements: &ExecutionRequirements,
    target: &ExecutionTarget,
) -> ExecutionTargetDecision {
    let mut reasons = Vec::new();

    for feature in &requirements.required_runtime_features {
        if !target.runtime.runtime_features.contains(feature) {
            reasons.push(ExecutionTargetReason::MissingRuntimeFeature {
                feature: feature.clone(),
            });
        }
    }

    if target.runtime.isolation_level < requirements.minimum_isolation_level {
        reasons.push(ExecutionTargetReason::IsolationLevelInsufficient {
            required: requirements.minimum_isolation_level,
            actual: target.runtime.isolation_level,
        });
    }

    ExecutionTargetDecision {
        eligible: reasons.is_empty(),
        reasons,
    }
}

/// Filter an already economically ranked target list without reordering it.
///
/// This lets the broker retain ownership of price/reliability ranking while
/// runtime admission acts as a hard eligibility gate before scheduling.
pub fn eligible_execution_targets<'a>(
    requirements: &ExecutionRequirements,
    targets: &'a [ExecutionTarget],
) -> Vec<&'a ExecutionTarget> {
    targets
        .iter()
        .filter(|target| evaluate_execution_target(requirements, target).eligible)
        .collect()
}

/// Return the cheapest/highest-ranked target that also satisfies execution
/// requirements, assuming `targets` are already in broker ranking order.
pub fn best_eligible_execution_target<'a>(
    requirements: &ExecutionRequirements,
    targets: &'a [ExecutionTarget],
) -> Option<&'a ExecutionTarget> {
    targets
        .iter()
        .find(|target| evaluate_execution_target(requirements, target).eligible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Architecture, CapacityOffer, Lifecycle, TrustTier};

    fn placement(id: &str, effective_cost_usd: f64) -> PlacementCandidate {
        PlacementCandidate {
            offer: CapacityOffer {
                provider: "test".into(),
                region: "us-east".into(),
                zone: None,
                offer_id: id.into(),
                lifecycle: Lifecycle::OnDemand,
                architecture: Architecture::X86_64,
                vcpus: 8.0,
                memory_gib: 32.0,
                accelerator: None,
                hourly_usd: effective_cost_usd,
                storage_usd: 0.0,
                egress_usd_per_gib: 0.0,
                startup_p50_seconds: 1.0,
                startup_p95_seconds: 2.0,
                interruption_rate_per_hour: 0.0,
                interruption_notice_seconds: 0,
                capacity_confidence: 1.0,
                throughput_score: 1.0,
                trust_tier: TrustTier::CloudProvider,
            },
            effective_cost_usd,
            expected_runtime_seconds: 60.0,
            expected_recovery_seconds: 0.0,
        }
    }

    fn target(
        id: &str,
        cost: f64,
        runtime_features: &[&str],
        isolation_level: u8,
    ) -> ExecutionTarget {
        ExecutionTarget {
            target_id: id.into(),
            placement: placement(id, cost),
            runtime: RuntimeEnvelope {
                runtime_features: runtime_features
                    .iter()
                    .map(|feature| (*feature).to_string())
                    .collect(),
                isolation_level,
            },
        }
    }

    #[test]
    fn reports_missing_features_and_isolation_deterministically() {
        let requirements = ExecutionRequirements {
            required_runtime_features: ["effects", "ffi"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            minimum_isolation_level: 3,
        };
        let target = target("pool-a", 0.10, &["effects"], 1);

        let decision = evaluate_execution_target(&requirements, &target);
        assert!(!decision.eligible);
        assert_eq!(
            decision.reasons,
            vec![
                ExecutionTargetReason::MissingRuntimeFeature {
                    feature: "ffi".into(),
                },
                ExecutionTargetReason::IsolationLevelInsufficient {
                    required: 3,
                    actual: 1,
                },
            ]
        );
    }

    #[test]
    fn filtering_preserves_capacity_broker_order() {
        let requirements = ExecutionRequirements {
            required_runtime_features: ["effects".to_string()].into_iter().collect(),
            minimum_isolation_level: 2,
        };
        let targets = vec![
            target("cheap-but-ineligible", 0.05, &["effects"], 1),
            target("next-best", 0.10, &["effects"], 2),
            target("expensive", 0.50, &["effects", "ffi"], 4),
        ];

        let eligible = eligible_execution_targets(&requirements, &targets);
        assert_eq!(eligible.len(), 2);
        assert_eq!(eligible[0].target_id, "next-best");
        assert_eq!(eligible[1].target_id, "expensive");
        assert_eq!(
            best_eligible_execution_target(&requirements, &targets)
                .expect("eligible target")
                .target_id,
            "next-best"
        );
    }

    #[test]
    fn exact_runtime_feature_set_is_not_interpreted_as_authority_policy() {
        let requirements = ExecutionRequirements {
            required_runtime_features: ["effects".to_string()].into_iter().collect(),
            minimum_isolation_level: 0,
        };
        let target = target("pool-a", 0.10, &["effects", "ffi", "python"], 0);

        let decision = evaluate_execution_target(&requirements, &target);
        assert!(decision.eligible);
        assert!(decision.reasons.is_empty());
    }
}
