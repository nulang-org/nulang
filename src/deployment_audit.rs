//! Immutable evidence record for Cloud deployment admission.
//!
//! A hosted control plane should persist this record after bundle validation
//! and admission, before scheduling. It snapshots the derived execution
//! manifest, tenant policy, candidate execution environment, and admission
//! decision, while content-addressing each component independently.

use std::fmt;

use serde::Serialize;

use crate::admission_policy::{
    evaluate_artifact_admission, AdmissionDecision, AdmissionEnvironment, AdmissionPolicy,
};
use crate::deployment_bundle::DeploymentBundle;
use crate::deployment_manifest::CompiledExecutionManifest;

pub const DEPLOYMENT_AUDIT_SCHEMA_VERSION: u32 = 1;

/// Immutable evidence for one admission decision.
///
/// Fields are private and the type is serialize-only so callers cannot bypass
/// [`DeploymentAdmissionRecord::new`] via struct literals or generic serde
/// deserialization. Timestamps intentionally do not live in this value: the
/// persistence layer may attach receipt/commit time while this record remains
/// deterministic and content-addressable from the security inputs themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentAdmissionRecord {
    schema_version: u32,
    deployment_id: String,
    package_name: String,
    artifact_path: String,
    artifact_blake3: String,
    manifest_blake3: String,
    policy_blake3: String,
    environment_blake3: String,
    decision_blake3: String,
    admitted: bool,
    execution_manifest: CompiledExecutionManifest,
    policy: AdmissionPolicy,
    environment: AdmissionEnvironment,
    decision: AdmissionDecision,
}

impl DeploymentAdmissionRecord {
    /// Build and cross-check a record from already validated bundle/admission
    /// inputs. Any mismatch is rejected rather than recorded as authoritative.
    ///
    /// Admission is re-evaluated from the exact artifact, exact policy contents,
    /// and exact candidate environment. This prevents a decision made under
    /// different policy contents from being relabeled with the same policy
    /// id/version in the audit ledger.
    pub fn new(
        deployment_id: impl Into<String>,
        bundle: &DeploymentBundle,
        policy: &AdmissionPolicy,
        environment: &AdmissionEnvironment,
        decision: &AdmissionDecision,
    ) -> Result<Self, DeploymentAuditError> {
        let deployment_id = deployment_id.into();
        if deployment_id.trim().is_empty() {
            return Err(DeploymentAuditError::EmptyDeploymentId);
        }

        let artifact_blake3 = blake3::hash(bundle.artifact_bytes()).to_hex().to_string();
        if artifact_blake3 != bundle.execution_manifest().artifact_blake3 {
            return Err(DeploymentAuditError::ArtifactManifestMismatch {
                artifact: artifact_blake3,
                manifest_artifact: bundle.execution_manifest().artifact_blake3.clone(),
            });
        }
        if decision.artifact_blake3 != bundle.execution_manifest().artifact_blake3 {
            return Err(DeploymentAuditError::DecisionArtifactMismatch {
                expected: bundle.execution_manifest().artifact_blake3.clone(),
                actual: decision.artifact_blake3.clone(),
            });
        }
        if decision.policy_id != policy.policy_id || decision.policy_version != policy.policy_version {
            return Err(DeploymentAuditError::DecisionPolicyMismatch {
                expected_id: policy.policy_id.clone(),
                expected_version: policy.policy_version,
                actual_id: decision.policy_id.clone(),
                actual_version: decision.policy_version,
            });
        }
        if decision.actual_isolation_level != environment.isolation_level {
            return Err(DeploymentAuditError::DecisionEnvironmentMismatch {
                expected_isolation_level: environment.isolation_level,
                actual_isolation_level: decision.actual_isolation_level,
            });
        }
        if decision.admitted != decision.reasons.is_empty() {
            return Err(DeploymentAuditError::InconsistentDecision {
                admitted: decision.admitted,
                reason_count: decision.reasons.len(),
            });
        }

        let manifest_blake3 = json_blake3(bundle.execution_manifest())?;
        if decision.derived_manifest_blake3.as_deref() != Some(manifest_blake3.as_str()) {
            return Err(DeploymentAuditError::DecisionManifestMismatch {
                expected: manifest_blake3,
                actual: decision.derived_manifest_blake3.clone(),
            });
        }

        // Re-evaluate against the exact security inputs rather than trusting
        // policy id/version labels or caller-provided decision contents.
        let recomputed = evaluate_artifact_admission(bundle.artifact_bytes(), policy, environment);
        if &recomputed != decision {
            return Err(DeploymentAuditError::DecisionReevaluationMismatch {
                expected_blake3: json_blake3(&recomputed)?,
                actual_blake3: json_blake3(decision)?,
            });
        }

        let policy_blake3 = json_blake3(policy)?;
        let environment_blake3 = json_blake3(environment)?;
        let decision_blake3 = json_blake3(decision)?;

        Ok(Self {
            schema_version: DEPLOYMENT_AUDIT_SCHEMA_VERSION,
            deployment_id,
            package_name: bundle.package_name().to_string(),
            artifact_path: bundle.artifact_path().to_string(),
            artifact_blake3: bundle.execution_manifest().artifact_blake3.clone(),
            manifest_blake3,
            policy_blake3,
            environment_blake3,
            decision_blake3,
            admitted: decision.admitted,
            execution_manifest: bundle.execution_manifest().clone(),
            policy: policy.clone(),
            environment: environment.clone(),
            decision: decision.clone(),
        })
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn deployment_id(&self) -> &str {
        &self.deployment_id
    }

    pub fn package_name(&self) -> &str {
        &self.package_name
    }

    pub fn artifact_path(&self) -> &str {
        &self.artifact_path
    }

    pub fn artifact_blake3(&self) -> &str {
        &self.artifact_blake3
    }

    pub fn manifest_blake3(&self) -> &str {
        &self.manifest_blake3
    }

    pub fn policy_blake3(&self) -> &str {
        &self.policy_blake3
    }

    pub fn environment_blake3(&self) -> &str {
        &self.environment_blake3
    }

    pub fn decision_blake3(&self) -> &str {
        &self.decision_blake3
    }

    pub fn admitted(&self) -> bool {
        self.admitted
    }

    pub fn execution_manifest(&self) -> &CompiledExecutionManifest {
        &self.execution_manifest
    }

    pub fn policy(&self) -> &AdmissionPolicy {
        &self.policy
    }

    pub fn environment(&self) -> &AdmissionEnvironment {
        &self.environment
    }

    pub fn decision(&self) -> &AdmissionDecision {
        &self.decision
    }

    /// Content address the complete immutable record.
    pub fn record_blake3(&self) -> Result<String, serde_json::Error> {
        json_blake3(self)
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[derive(Debug)]
pub enum DeploymentAuditError {
    EmptyDeploymentId,
    ArtifactManifestMismatch {
        artifact: String,
        manifest_artifact: String,
    },
    DecisionArtifactMismatch {
        expected: String,
        actual: String,
    },
    DecisionPolicyMismatch {
        expected_id: String,
        expected_version: u64,
        actual_id: String,
        actual_version: u64,
    },
    DecisionEnvironmentMismatch {
        expected_isolation_level: u8,
        actual_isolation_level: u8,
    },
    DecisionManifestMismatch {
        expected: String,
        actual: Option<String>,
    },
    DecisionReevaluationMismatch {
        expected_blake3: String,
        actual_blake3: String,
    },
    InconsistentDecision {
        admitted: bool,
        reason_count: usize,
    },
    Serialization(serde_json::Error),
}

impl fmt::Display for DeploymentAuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDeploymentId => write!(f, "deployment id must not be empty"),
            Self::ArtifactManifestMismatch {
                artifact,
                manifest_artifact,
            } => write!(
                f,
                "artifact digest {artifact} does not match artifact digest recorded by derived manifest {manifest_artifact}"
            ),
            Self::DecisionArtifactMismatch { expected, actual } => write!(
                f,
                "decision artifact digest {actual} does not match admitted artifact {expected}"
            ),
            Self::DecisionPolicyMismatch {
                expected_id,
                expected_version,
                actual_id,
                actual_version,
            } => write!(
                f,
                "decision policy {actual_id}@{actual_version} does not match evaluated policy {expected_id}@{expected_version}"
            ),
            Self::DecisionEnvironmentMismatch {
                expected_isolation_level,
                actual_isolation_level,
            } => write!(
                f,
                "decision isolation level {actual_isolation_level} does not match candidate environment {expected_isolation_level}"
            ),
            Self::DecisionManifestMismatch { expected, actual } => write!(
                f,
                "decision derived-manifest digest {:?} does not match validated manifest {expected}",
                actual
            ),
            Self::DecisionReevaluationMismatch {
                expected_blake3,
                actual_blake3,
            } => write!(
                f,
                "admission decision digest {actual_blake3} does not match decision recomputed from exact policy/environment inputs {expected_blake3}"
            ),
            Self::InconsistentDecision {
                admitted,
                reason_count,
            } => write!(
                f,
                "admission decision is internally inconsistent: admitted={admitted}, denial reasons={reason_count}"
            ),
            Self::Serialization(error) => {
                write!(f, "cannot serialize deployment audit evidence: {error}")
            }
        }
    }
}

impl std::error::Error for DeploymentAuditError {}

impl From<serde_json::Error> for DeploymentAuditError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serialization(value)
    }
}

fn json_blake3<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(value)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
    use flate2::write::GzEncoder;
    use flate2::Compression;

    fn bundle_from_module(module: CodeModule) -> DeploymentBundle {
        let artifact = module.to_nbc(None).expect("encode nbc");
        let package: &[u8] = b"[package]\nname = \"app\"\nversion = \"0.1.0\"\n";
        let mut output = Vec::new();
        {
            let gzip = GzEncoder::new(&mut output, Compression::default());
            let mut builder = tar::Builder::new(gzip);
            for (path, data) in [
                ("Nulang.toml", package),
                (".nula/dist/app.nbc", artifact.as_slice()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, data)
                    .expect("append bundle entry");
            }
            let gzip = builder.into_inner().expect("finish tar");
            gzip.finish().expect("finish gzip");
        }
        DeploymentBundle::parse(&output).expect("validate bundle")
    }

    fn fixture_bundle() -> DeploymentBundle {
        bundle_from_module(CodeModule::new("audit-test"))
    }

    fn effect_bundle(effect: &str) -> DeploymentBundle {
        let mut module = CodeModule::new("audit-effect-test");
        let idx = module.add_constant(Constant::String(effect.to_string()));
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((idx >> 8) & 0xff) as u8,
            (idx & 0xff) as u8,
            0,
        ));
        bundle_from_module(module)
    }

    #[test]
    fn records_complete_immutable_admission_evidence() {
        let bundle = fixture_bundle();
        let policy = AdmissionPolicy::new("tenant/default", 7);
        let environment = AdmissionEnvironment::default();
        let decision = bundle.evaluate_admission(&policy, &environment);

        let record = DeploymentAdmissionRecord::new(
            "dep_123",
            &bundle,
            &policy,
            &environment,
            &decision,
        )
        .expect("build audit record");

        assert!(record.admitted());
        assert_eq!(record.schema_version(), DEPLOYMENT_AUDIT_SCHEMA_VERSION);
        assert_eq!(record.deployment_id(), "dep_123");
        assert_eq!(record.package_name(), "app");
        assert_eq!(record.artifact_path(), ".nula/dist/app.nbc");
        assert_eq!(record.policy().policy_id, "tenant/default");
        assert_eq!(
            record.artifact_blake3(),
            bundle.execution_manifest().artifact_blake3
        );
        assert_eq!(
            record.manifest_blake3(),
            json_blake3(bundle.execution_manifest()).unwrap()
        );
        assert_eq!(record.policy_blake3(), json_blake3(&policy).unwrap());
        assert_eq!(
            record.environment_blake3(),
            json_blake3(&environment).unwrap()
        );
        assert_eq!(record.decision_blake3(), json_blake3(&decision).unwrap());
        assert_eq!(record.execution_manifest(), bundle.execution_manifest());
        assert_eq!(record.environment(), &environment);
        assert_eq!(record.decision(), &decision);
        assert_eq!(record.record_blake3().unwrap(), record.record_blake3().unwrap());
    }

    #[test]
    fn policy_content_changes_are_visible_even_when_version_is_reused() {
        let bundle = fixture_bundle();
        let environment = AdmissionEnvironment::default();
        let policy_a = AdmissionPolicy::new("tenant/default", 1);
        let mut policy_b = policy_a.clone();
        policy_b
            .allowed_external_effect_roots
            .insert("Net".to_string());

        assert_ne!(json_blake3(&policy_a).unwrap(), json_blake3(&policy_b).unwrap());

        let decision = bundle.evaluate_admission(&policy_a, &environment);
        let record = DeploymentAdmissionRecord::new(
            "dep_policy",
            &bundle,
            &policy_a,
            &environment,
            &decision,
        )
        .unwrap();
        assert_ne!(record.policy_blake3(), json_blake3(&policy_b).unwrap());
    }

    #[test]
    fn rejects_decision_from_different_policy_contents_with_same_identity() {
        let bundle = effect_bundle("Net.fetch");
        let mut policy_allowed = AdmissionPolicy::new("tenant/shared", 1);
        policy_allowed
            .allowed_runtime_features
            .insert("effects".to_string());
        policy_allowed
            .allowed_external_effect_roots
            .insert("Net".to_string());
        let mut policy_denied = policy_allowed.clone();
        policy_denied.allowed_external_effect_roots.clear();
        let environment = AdmissionEnvironment {
            available_runtime_features: ["effects".to_string()].into_iter().collect(),
            isolation_level: 0,
        };

        let allowed_decision = bundle.evaluate_admission(&policy_allowed, &environment);
        assert!(allowed_decision.admitted);

        assert!(matches!(
            DeploymentAdmissionRecord::new(
                "dep_relabel",
                &bundle,
                &policy_denied,
                &environment,
                &allowed_decision,
            ),
            Err(DeploymentAuditError::DecisionReevaluationMismatch { .. })
        ));
    }

    #[test]
    fn rejects_a_decision_for_a_different_artifact() {
        let bundle = fixture_bundle();
        let policy = AdmissionPolicy::new("tenant/default", 1);
        let environment = AdmissionEnvironment::default();
        let mut decision = bundle.evaluate_admission(&policy, &environment);
        decision.artifact_blake3 = "0".repeat(64);

        assert!(matches!(
            DeploymentAdmissionRecord::new(
                "dep_forged",
                &bundle,
                &policy,
                &environment,
                &decision,
            ),
            Err(DeploymentAuditError::DecisionArtifactMismatch { .. })
        ));
    }

    #[test]
    fn rejects_a_decision_for_a_different_policy_identity() {
        let bundle = fixture_bundle();
        let policy = AdmissionPolicy::new("tenant/default", 1);
        let environment = AdmissionEnvironment::default();
        let mut decision = bundle.evaluate_admission(&policy, &environment);
        decision.policy_version = 2;

        assert!(matches!(
            DeploymentAdmissionRecord::new(
                "dep_policy_mismatch",
                &bundle,
                &policy,
                &environment,
                &decision,
            ),
            Err(DeploymentAuditError::DecisionPolicyMismatch { .. })
        ));
    }
}
