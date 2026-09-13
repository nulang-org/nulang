//! Control-plane bridge between validated Nulang deployment admission and
//! provider-neutral capacity placement.
//!
//! This crate intentionally depends on both `nulang` and `nulang-capacity` so
//! neither core crate has to depend on the other. Hosted control-plane code can
//! use these contracts directly while keeping artifact authority, capacity
//! economics, and runtime eligibility as separate decisions.

pub mod acquisition;

use std::fmt;

use nulang::admission_policy::{AdmissionDecision, AdmissionEnvironment, AdmissionPolicy};
use nulang::deployment_bundle::DeploymentBundle;
use nulang::deployment_manifest::CompiledExecutionManifest;
use nulang_capacity::execution::{
    best_eligible_execution_target, ExecutionRequirements, ExecutionTarget, RuntimeEnvelope,
};

/// Runtime features represented by the compiled manifest but not loadable
/// scheduler subsystems. They remain part of authority analysis, not worker
/// pool capability matching.
const NON_LOADABLE_RUNTIME_MARKERS: &[&str] = &["external_authority"];

/// Candidate selection result after both capacity eligibility and final
/// authoritative artifact admission succeed.
#[derive(Debug)]
pub struct SelectedExecutionTarget<'a> {
    target: &'a ExecutionTarget,
    requirements: ExecutionRequirements,
    decision: AdmissionDecision,
}

impl<'a> SelectedExecutionTarget<'a> {
    pub fn target(&self) -> &'a ExecutionTarget {
        self.target
    }

    pub fn requirements(&self) -> &ExecutionRequirements {
        &self.requirements
    }

    pub fn decision(&self) -> &AdmissionDecision {
        &self.decision
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlPlaneSelectionError {
    /// Artifact/policy combination is denied even when runtime availability and
    /// isolation are treated as satisfiable during preflight.
    PolicyDenied(AdmissionDecision),
    /// Capacity exists, but none of the bound runtime targets can satisfy the
    /// required runtime subsystems and isolation strength.
    NoEligibleExecutionTarget,
    /// Defensive final gate: the selected concrete environment failed the same
    /// authoritative admission logic used during preflight.
    FinalAdmissionDenied(AdmissionDecision),
}

impl fmt::Display for ControlPlaneSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PolicyDenied(decision) => write!(
                f,
                "deployment denied by artifact/tenant policy preflight ({} reason(s))",
                decision.reasons.len()
            ),
            Self::NoEligibleExecutionTarget => {
                write!(f, "no execution target satisfies runtime/isolation requirements")
            }
            Self::FinalAdmissionDenied(decision) => write!(
                f,
                "selected execution target failed final admission ({} reason(s))",
                decision.reasons.len()
            ),
        }
    }
}

impl std::error::Error for ControlPlaneSelectionError {}

/// Convert a concrete worker-pool/runtime envelope into the admission engine's
/// candidate-environment model.
pub fn admission_environment(runtime: &RuntimeEnvelope) -> AdmissionEnvironment {
    AdmissionEnvironment {
        available_runtime_features: runtime.runtime_features.clone(),
        isolation_level: runtime.isolation_level,
    }
}

/// Build a synthetic environment used only to separate policy/authority denial
/// from runtime availability during preflight.
///
/// Every loadable feature required by the compiled artifact is marked present
/// and isolation is maximized. Tenant authority checks remain fully active.
pub fn preflight_environment(manifest: &CompiledExecutionManifest) -> AdmissionEnvironment {
    AdmissionEnvironment {
        available_runtime_features: loadable_runtime_features(manifest),
        isolation_level: u8::MAX,
    }
}

/// Derive scheduler requirements from the validated artifact and exact tenant
/// policy. A denied policy never reaches capacity selection.
pub fn derive_execution_requirements(
    bundle: &DeploymentBundle,
    policy: &AdmissionPolicy,
) -> Result<(ExecutionRequirements, AdmissionDecision), AdmissionDecision> {
    let environment = preflight_environment(bundle.execution_manifest());
    let decision = bundle.evaluate_admission(policy, &environment);
    if !decision.admitted {
        return Err(decision);
    }

    Ok((
        ExecutionRequirements {
            required_runtime_features: loadable_runtime_features(bundle.execution_manifest()),
            minimum_isolation_level: decision.required_isolation_level,
        },
        decision,
    ))
}

/// Select the highest-ranked runtime-compatible capacity target, then run final
/// authoritative admission against that target's exact runtime envelope.
///
/// `targets` must already be ordered by the capacity broker. This function never
/// changes economic ordering; it only applies hard compatibility constraints.
pub fn select_execution_target<'a>(
    bundle: &DeploymentBundle,
    policy: &AdmissionPolicy,
    targets: &'a [ExecutionTarget],
) -> Result<SelectedExecutionTarget<'a>, ControlPlaneSelectionError> {
    let (requirements, _preflight) = derive_execution_requirements(bundle, policy)
        .map_err(ControlPlaneSelectionError::PolicyDenied)?;

    let target = best_eligible_execution_target(&requirements, targets)
        .ok_or(ControlPlaneSelectionError::NoEligibleExecutionTarget)?;
    let environment = admission_environment(&target.runtime);
    let decision = bundle.evaluate_admission(policy, &environment);
    if !decision.admitted {
        return Err(ControlPlaneSelectionError::FinalAdmissionDenied(decision));
    }

    Ok(SelectedExecutionTarget {
        target,
        requirements,
        decision,
    })
}

fn loadable_runtime_features(
    manifest: &CompiledExecutionManifest,
) -> std::collections::BTreeSet<String> {
    manifest
        .runtime_features
        .iter()
        .filter(|feature| !NON_LOADABLE_RUNTIME_MARKERS.contains(&feature.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use nulang::bytecode::{CodeModule, Constant, Instruction, OpCode};
    use nulang_capacity::broker::PlacementCandidate;
    use nulang_capacity::{Architecture, CapacityOffer, Lifecycle, TrustTier};

    fn emit_effect(module: &mut CodeModule, effect: &str) {
        let idx = module.add_constant(Constant::String(effect.to_string()));
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((idx >> 8) & 0xff) as u8,
            (idx & 0xff) as u8,
            0,
        ));
    }

    fn deployment_bundle(module: CodeModule) -> DeploymentBundle {
        let nbc = module.to_nbc(None).expect("encode nbc");
        let manifest = b"[package]\nname = \"app\"\nversion = \"0.1.0\"\n";
        let mut bytes = Vec::new();
        {
            let gzip = GzEncoder::new(&mut bytes, Compression::default());
            let mut tar = tar::Builder::new(gzip);
            append(&mut tar, "Nulang.toml", manifest);
            append(&mut tar, ".nula/dist/app.nbc", &nbc);
            let gzip = tar.into_inner().expect("finish tar");
            gzip.finish().expect("finish gzip");
        }
        DeploymentBundle::parse(&bytes).expect("validated bundle")
    }

    fn append<W: std::io::Write>(tar: &mut tar::Builder<W>, path: &str, data: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, path, data)
            .expect("append bundle entry");
    }

    fn target(id: &str, cost: f64, features: &[&str], isolation: u8) -> ExecutionTarget {
        ExecutionTarget {
            target_id: id.to_string(),
            placement: PlacementCandidate {
                offer: CapacityOffer {
                    provider: "test".into(),
                    region: "us-east".into(),
                    zone: None,
                    offer_id: id.into(),
                    lifecycle: Lifecycle::OnDemand,
                    architecture: Architecture::X86_64,
                    vcpus: 4.0,
                    memory_gib: 8.0,
                    accelerator: None,
                    hourly_usd: cost,
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
                effective_cost_usd: cost,
                expected_runtime_seconds: 60.0,
                expected_recovery_seconds: 0.0,
            },
            runtime: RuntimeEnvelope {
                runtime_features: features.iter().map(|value| (*value).to_string()).collect(),
                isolation_level: isolation,
            },
        }
    }

    fn allowed_net_policy() -> AdmissionPolicy {
        let mut policy = AdmissionPolicy::new("tenant/default", 1);
        policy.allowed_runtime_features.insert("effects".into());
        policy.allowed_external_effect_roots.insert("Net".into());
        policy.minimum_host_boundary_isolation_level = 2;
        policy
    }

    #[test]
    fn policy_denial_happens_before_capacity_selection() {
        let mut module = CodeModule::new("denied");
        emit_effect(&mut module, "Net.fetch");
        let bundle = deployment_bundle(module);
        let policy = AdmissionPolicy::new("tenant/default", 1);
        let targets = vec![target("pool", 0.01, &["effects"], 10)];

        let error = select_execution_target(&bundle, &policy, &targets).unwrap_err();
        assert!(matches!(error, ControlPlaneSelectionError::PolicyDenied(_)));
    }

    #[test]
    fn cheapest_incompatible_target_is_skipped_without_reordering() {
        let mut module = CodeModule::new("net");
        emit_effect(&mut module, "Net.fetch");
        let bundle = deployment_bundle(module);
        let policy = allowed_net_policy();
        let targets = vec![
            target("cheap-isolation-1", 0.01, &["effects"], 1),
            target("next-isolation-2", 0.02, &["effects"], 2),
            target("expensive", 1.00, &["effects"], 5),
        ];

        let selected = select_execution_target(&bundle, &policy, &targets).expect("select target");
        assert_eq!(selected.target().target_id, "next-isolation-2");
        assert_eq!(selected.requirements().minimum_isolation_level, 2);
        assert!(selected.decision().admitted);
    }

    #[test]
    fn missing_runtime_subsystem_yields_no_eligible_target() {
        let mut module = CodeModule::new("net");
        emit_effect(&mut module, "Net.fetch");
        let bundle = deployment_bundle(module);
        let policy = allowed_net_policy();
        let targets = vec![target("pool", 0.01, &[], 5)];

        let error = select_execution_target(&bundle, &policy, &targets).unwrap_err();
        assert_eq!(error, ControlPlaneSelectionError::NoEligibleExecutionTarget);
    }

    #[test]
    fn preflight_excludes_non_loadable_authority_marker() {
        let mut module = CodeModule::new("net");
        emit_effect(&mut module, "Net.fetch");
        let bundle = deployment_bundle(module);
        let environment = preflight_environment(bundle.execution_manifest());

        assert!(environment.available_runtime_features.contains("effects"));
        assert!(!environment
            .available_runtime_features
            .contains("external_authority"));
    }
}
