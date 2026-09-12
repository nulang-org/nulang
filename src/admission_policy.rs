//! Deterministic deployment admission for compiled Nulang artifacts.
//!
//! Admission is derived from the exact `.nbc` bytes that will execute. Cloud
//! can evaluate an artifact directly, or verify an optional client-produced
//! [`CompiledExecutionManifest`] by re-deriving the manifest server-side before
//! policy evaluation.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::deployment_manifest::{
    CompiledExecutionManifest, DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
};

/// Current admission policy schema.
pub const ADMISSION_POLICY_SCHEMA_VERSION: u32 = 1;
/// Current admission decision/audit schema.
pub const ADMISSION_DECISION_SCHEMA_VERSION: u32 = 1;

/// Tenant or organization policy applied before a compiled artifact may run.
///
/// Authority allow-lists are fail-closed. Runtime features are explicit: every
/// required feature except the aggregate `external_authority` marker must be
/// present. External effects may be granted exactly or by root namespace. FFI
/// may be granted exactly (`library::symbol`) or, deliberately more broadly, by
/// whole library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionPolicy {
    pub schema_version: u32,
    pub policy_id: String,
    pub policy_version: u64,
    #[serde(default)]
    pub allowed_runtime_features: BTreeSet<String>,
    #[serde(default)]
    pub allowed_external_effects: BTreeSet<String>,
    #[serde(default)]
    pub allowed_external_effect_roots: BTreeSet<String>,
    #[serde(default)]
    pub allowed_spawn_capabilities: BTreeSet<String>,
    /// Broad FFI grants. Prefer `allowed_ffi_functions` for least authority.
    #[serde(default)]
    pub allowed_ffi_libraries: BTreeSet<String>,
    /// Exact canonical `library::symbol` grants.
    #[serde(default)]
    pub allowed_ffi_functions: BTreeSet<String>,
    /// Base isolation level required for every deployment under this policy.
    /// The numeric scale is defined by the control plane; admission only
    /// compares levels and therefore does not hard-code container/VM semantics.
    #[serde(default)]
    pub minimum_isolation_level: u8,
    /// Additional minimum when the artifact reaches any host/authority boundary.
    #[serde(default)]
    pub minimum_host_boundary_isolation_level: u8,
    /// Additional minimum when the artifact contains native FFI declarations.
    #[serde(default)]
    pub minimum_ffi_isolation_level: u8,
}

impl AdmissionPolicy {
    pub fn new(policy_id: impl Into<String>, policy_version: u64) -> Self {
        Self {
            schema_version: ADMISSION_POLICY_SCHEMA_VERSION,
            policy_id: policy_id.into(),
            policy_version,
            allowed_runtime_features: BTreeSet::new(),
            allowed_external_effects: BTreeSet::new(),
            allowed_external_effect_roots: BTreeSet::new(),
            allowed_spawn_capabilities: BTreeSet::new(),
            allowed_ffi_libraries: BTreeSet::new(),
            allowed_ffi_functions: BTreeSet::new(),
            minimum_isolation_level: 0,
            minimum_host_boundary_isolation_level: 0,
            minimum_ffi_isolation_level: 0,
        }
    }
}

/// Scheduler/runtime facts for a candidate execution target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionEnvironment {
    /// Runtime feature groups installed/enabled on the target.
    #[serde(default)]
    pub available_runtime_features: BTreeSet<String>,
    /// Control-plane-defined isolation strength. Higher values represent a
    /// target considered at least as isolated as lower values.
    #[serde(default)]
    pub isolation_level: u8,
}

/// Structured reasons for a denied admission decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum AdmissionReason {
    UnsupportedPolicySchema {
        expected: u32,
        actual: u32,
    },
    UnsupportedManifestSchema {
        expected: u32,
        actual: u32,
    },
    ArtifactDecodeFailed {
        message: String,
    },
    ArtifactDigestMismatch {
        expected: String,
        actual: String,
    },
    /// The provided manifest differs from the one re-derived from the exact
    /// artifact bytes. This detects deleted/altered authority requirements even
    /// when the caller leaves the artifact digest unchanged.
    ManifestMismatch {
        provided_manifest_blake3: Option<String>,
        derived_manifest_blake3: Option<String>,
    },
    RuntimeFeatureDenied {
        feature: String,
    },
    RuntimeFeatureUnavailable {
        feature: String,
    },
    ExternalEffectDenied {
        effect: String,
    },
    SpawnCapabilityDenied {
        capability: String,
    },
    FfiFunctionDenied {
        function: String,
    },
    IsolationLevelInsufficient {
        required: u8,
        actual: u8,
    },
}

/// Deterministic, serializable result suitable for an append-only audit ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionDecision {
    pub schema_version: u32,
    pub admitted: bool,
    /// Digest calculated directly from the uploaded artifact bytes.
    pub artifact_blake3: String,
    /// Digest of a caller-provided sidecar, when one was supplied.
    pub provided_manifest_blake3: Option<String>,
    /// Digest of the manifest re-derived from the uploaded artifact.
    pub derived_manifest_blake3: Option<String>,
    pub policy_id: String,
    pub policy_version: u64,
    pub required_isolation_level: u8,
    pub actual_isolation_level: u8,
    pub reasons: Vec<AdmissionReason>,
}

impl AdmissionDecision {
    /// Content-address this decision for append-only audit storage.
    pub fn audit_blake3(&self) -> Result<String, serde_json::Error> {
        let bytes = serde_json::to_vec(self)?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Evaluate an uploaded `.nbc` artifact directly.
///
/// This is the preferred Cloud admission path: policy inputs are derived on the
/// server from the exact artifact being scheduled, so no client manifest is a
/// trust dependency.
pub fn evaluate_artifact_admission(
    nbc_bytes: &[u8],
    policy: &AdmissionPolicy,
    environment: &AdmissionEnvironment,
) -> AdmissionDecision {
    let artifact_blake3 = blake3::hash(nbc_bytes).to_hex().to_string();
    let reasons = policy_schema_reasons(policy);
    let derived_manifest = match CompiledExecutionManifest::from_nbc_bytes(nbc_bytes) {
        Ok(manifest) => manifest,
        Err(message) => {
            return decode_failure_decision(
                artifact_blake3,
                None,
                policy,
                environment,
                reasons,
                message,
            )
        }
    };
    let derived_manifest_blake3 = manifest_blake3(&derived_manifest).ok();
    evaluate_derived_manifest(
        artifact_blake3,
        None,
        derived_manifest_blake3,
        &derived_manifest,
        policy,
        environment,
        reasons,
    )
}

/// Verify an uploaded artifact plus optional preflight/sidecar manifest, then
/// evaluate policy using the manifest re-derived from the artifact.
///
/// The provided manifest is never trusted as authority input; any discrepancy
/// is itself a denial reason.
pub fn evaluate_admission(
    nbc_bytes: &[u8],
    provided_manifest: &CompiledExecutionManifest,
    policy: &AdmissionPolicy,
    environment: &AdmissionEnvironment,
) -> AdmissionDecision {
    let artifact_blake3 = blake3::hash(nbc_bytes).to_hex().to_string();
    let provided_manifest_blake3 = manifest_blake3(provided_manifest).ok();
    let mut reasons = policy_schema_reasons(policy);

    if provided_manifest.schema_version != DEPLOYMENT_MANIFEST_SCHEMA_VERSION {
        reasons.push(AdmissionReason::UnsupportedManifestSchema {
            expected: DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
            actual: provided_manifest.schema_version,
        });
    }
    if provided_manifest.artifact_blake3 != artifact_blake3 {
        reasons.push(AdmissionReason::ArtifactDigestMismatch {
            expected: provided_manifest.artifact_blake3.clone(),
            actual: artifact_blake3.clone(),
        });
    }

    let derived_manifest = match CompiledExecutionManifest::from_nbc_bytes(nbc_bytes) {
        Ok(manifest) => manifest,
        Err(message) => {
            return decode_failure_decision(
                artifact_blake3,
                provided_manifest_blake3,
                policy,
                environment,
                reasons,
                message,
            )
        }
    };
    let derived_manifest_blake3 = manifest_blake3(&derived_manifest).ok();
    if provided_manifest != &derived_manifest {
        reasons.push(AdmissionReason::ManifestMismatch {
            provided_manifest_blake3: provided_manifest_blake3.clone(),
            derived_manifest_blake3: derived_manifest_blake3.clone(),
        });
    }

    evaluate_derived_manifest(
        artifact_blake3,
        provided_manifest_blake3,
        derived_manifest_blake3,
        &derived_manifest,
        policy,
        environment,
        reasons,
    )
}

fn evaluate_derived_manifest(
    artifact_blake3: String,
    provided_manifest_blake3: Option<String>,
    derived_manifest_blake3: Option<String>,
    manifest: &CompiledExecutionManifest,
    policy: &AdmissionPolicy,
    environment: &AdmissionEnvironment,
    mut reasons: Vec<AdmissionReason>,
) -> AdmissionDecision {
    // `external_authority` is an aggregate marker, not a loadable runtime
    // subsystem. Concrete authorities are evaluated below.
    for feature in &manifest.runtime_features {
        if feature == "external_authority" {
            continue;
        }
        if !policy.allowed_runtime_features.contains(feature) {
            reasons.push(AdmissionReason::RuntimeFeatureDenied {
                feature: feature.clone(),
            });
        }
        if !environment.available_runtime_features.contains(feature) {
            reasons.push(AdmissionReason::RuntimeFeatureUnavailable {
                feature: feature.clone(),
            });
        }
    }

    for effect in &manifest.external_effects {
        let root = external_effect_root(effect);
        if !policy.allowed_external_effects.contains(effect)
            && !policy.allowed_external_effect_roots.contains(root)
        {
            reasons.push(AdmissionReason::ExternalEffectDenied {
                effect: effect.clone(),
            });
        }
    }

    for capability in &manifest.spawn_capabilities {
        if !policy.allowed_spawn_capabilities.contains(capability) {
            reasons.push(AdmissionReason::SpawnCapabilityDenied {
                capability: capability.clone(),
            });
        }
    }

    for function in &manifest.ffi_functions {
        let library = ffi_library(function);
        if !policy.allowed_ffi_functions.contains(function)
            && !policy.allowed_ffi_libraries.contains(library)
        {
            reasons.push(AdmissionReason::FfiFunctionDenied {
                function: function.clone(),
            });
        }
    }

    let mut required_isolation_level = policy.minimum_isolation_level;
    if manifest.reaches_host_boundary {
        required_isolation_level =
            required_isolation_level.max(policy.minimum_host_boundary_isolation_level);
    }
    if !manifest.ffi_functions.is_empty() {
        required_isolation_level =
            required_isolation_level.max(policy.minimum_ffi_isolation_level);
    }
    if environment.isolation_level < required_isolation_level {
        reasons.push(AdmissionReason::IsolationLevelInsufficient {
            required: required_isolation_level,
            actual: environment.isolation_level,
        });
    }

    AdmissionDecision {
        schema_version: ADMISSION_DECISION_SCHEMA_VERSION,
        admitted: reasons.is_empty(),
        artifact_blake3,
        provided_manifest_blake3,
        derived_manifest_blake3,
        policy_id: policy.policy_id.clone(),
        policy_version: policy.policy_version,
        required_isolation_level,
        actual_isolation_level: environment.isolation_level,
        reasons,
    }
}

fn decode_failure_decision(
    artifact_blake3: String,
    provided_manifest_blake3: Option<String>,
    policy: &AdmissionPolicy,
    environment: &AdmissionEnvironment,
    mut reasons: Vec<AdmissionReason>,
    message: String,
) -> AdmissionDecision {
    reasons.push(AdmissionReason::ArtifactDecodeFailed { message });
    AdmissionDecision {
        schema_version: ADMISSION_DECISION_SCHEMA_VERSION,
        admitted: false,
        artifact_blake3,
        provided_manifest_blake3,
        derived_manifest_blake3: None,
        policy_id: policy.policy_id.clone(),
        policy_version: policy.policy_version,
        required_isolation_level: policy.minimum_isolation_level,
        actual_isolation_level: environment.isolation_level,
        reasons,
    }
}

fn policy_schema_reasons(policy: &AdmissionPolicy) -> Vec<AdmissionReason> {
    if policy.schema_version == ADMISSION_POLICY_SCHEMA_VERSION {
        Vec::new()
    } else {
        vec![AdmissionReason::UnsupportedPolicySchema {
            expected: ADMISSION_POLICY_SCHEMA_VERSION,
            actual: policy.schema_version,
        }]
    }
}

fn external_effect_root(effect: &str) -> &str {
    effect
        .split_once('.')
        .map_or(effect, |(root, _operation)| root)
}

fn ffi_library(function: &str) -> &str {
    function
        .rsplit_once("::")
        .map_or(function, |(library, _symbol)| library)
}

fn manifest_blake3(manifest: &CompiledExecutionManifest) -> Result<String, serde_json::Error> {
    let bytes = serde_json::to_vec(manifest)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{
        CodeModule, Constant, FfiType, ForeignFunctionDef, Instruction, OpCode,
    };

    fn nbc(module: &CodeModule) -> Vec<u8> {
        module.to_nbc(None).expect("encode nbc")
    }

    fn manifest(bytes: &[u8]) -> CompiledExecutionManifest {
        CompiledExecutionManifest::from_nbc_bytes(bytes).expect("derive manifest")
    }

    fn string_set(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn emit_effect(module: &mut CodeModule, name: &str) {
        let idx = module.add_constant(Constant::String(name.to_string()));
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((idx >> 8) & 0xff) as u8,
            (idx & 0xff) as u8,
            0,
        ));
    }

    #[test]
    fn artifact_only_admission_derives_manifest_server_side() {
        let module = CodeModule::new("pure");
        let bytes = nbc(&module);
        let policy = AdmissionPolicy::new("tenant/default", 1);
        let environment = AdmissionEnvironment::default();

        let decision = evaluate_artifact_admission(&bytes, &policy, &environment);
        assert!(decision.admitted, "{:?}", decision.reasons);
        assert!(decision.provided_manifest_blake3.is_none());
        assert!(decision.derived_manifest_blake3.is_some());
    }

    #[test]
    fn pure_compute_sidecar_is_admitted_by_empty_authority_policy() {
        let module = CodeModule::new("pure");
        let bytes = nbc(&module);
        let manifest = manifest(&bytes);
        let policy = AdmissionPolicy::new("tenant/default", 1);
        let environment = AdmissionEnvironment::default();

        let decision = evaluate_admission(&bytes, &manifest, &policy, &environment);
        assert!(decision.admitted, "{:?}", decision.reasons);
        assert!(decision.reasons.is_empty());
        assert_eq!(decision.artifact_blake3, manifest.artifact_blake3);
        assert!(decision.audit_blake3().is_ok());
    }

    #[test]
    fn manifest_authority_tampering_is_detected_even_with_correct_artifact_digest() {
        let mut module = CodeModule::new("tamper");
        emit_effect(&mut module, "DB.query");
        let bytes = nbc(&module);
        let mut provided = manifest(&bytes);
        provided.external_effects.clear();
        provided.external_effect_roots.clear();
        provided.reaches_host_boundary = false;

        let mut policy = AdmissionPolicy::new("tenant/db", 3);
        policy.allowed_runtime_features = string_set(&["effects"]);
        policy.allowed_external_effect_roots = string_set(&["DB"]);
        let environment = AdmissionEnvironment {
            available_runtime_features: string_set(&["effects"]),
            isolation_level: 0,
        };

        let decision = evaluate_admission(&bytes, &provided, &policy, &environment);
        assert!(!decision.admitted);
        assert!(decision
            .reasons
            .iter()
            .any(|reason| matches!(reason, AdmissionReason::ManifestMismatch { .. })));
        assert_eq!(provided.artifact_blake3, decision.artifact_blake3);
    }

    #[test]
    fn external_effect_root_can_be_explicitly_allowed() {
        let mut module = CodeModule::new("db");
        emit_effect(&mut module, "DB.query");
        let bytes = nbc(&module);
        let manifest = manifest(&bytes);

        let mut policy = AdmissionPolicy::new("tenant/db", 1);
        policy.allowed_runtime_features = string_set(&["effects"]);
        policy.allowed_external_effect_roots = string_set(&["DB"]);
        let environment = AdmissionEnvironment {
            available_runtime_features: string_set(&["effects"]),
            isolation_level: 0,
        };

        let decision = evaluate_admission(&bytes, &manifest, &policy, &environment);
        assert!(decision.admitted, "{:?}", decision.reasons);
    }

    #[test]
    fn external_effect_is_denied_by_default() {
        let mut module = CodeModule::new("net");
        emit_effect(&mut module, "Net.fetch");
        let bytes = nbc(&module);
        let manifest = manifest(&bytes);

        let mut policy = AdmissionPolicy::new("tenant/default", 1);
        policy.allowed_runtime_features = string_set(&["effects"]);
        let environment = AdmissionEnvironment {
            available_runtime_features: string_set(&["effects"]),
            isolation_level: 0,
        };

        let decision = evaluate_admission(&bytes, &manifest, &policy, &environment);
        assert!(!decision.admitted);
        assert!(decision.reasons.iter().any(|reason| matches!(
            reason,
            AdmissionReason::ExternalEffectDenied { effect } if effect == "Net.fetch"
        )));
    }

    #[test]
    fn scheduler_must_actually_offer_required_runtime_features() {
        let mut module = CodeModule::new("db");
        emit_effect(&mut module, "DB.query");
        let bytes = nbc(&module);
        let manifest = manifest(&bytes);

        let mut policy = AdmissionPolicy::new("tenant/db", 1);
        policy.allowed_runtime_features = string_set(&["effects"]);
        policy.allowed_external_effect_roots = string_set(&["DB"]);
        let environment = AdmissionEnvironment::default();

        let decision = evaluate_admission(&bytes, &manifest, &policy, &environment);
        assert!(!decision.admitted);
        assert!(decision.reasons.iter().any(|reason| matches!(
            reason,
            AdmissionReason::RuntimeFeatureUnavailable { feature } if feature == "effects"
        )));
    }

    #[test]
    fn host_boundary_can_require_stronger_isolation() {
        let mut module = CodeModule::new("db");
        emit_effect(&mut module, "DB.query");
        let bytes = nbc(&module);
        let manifest = manifest(&bytes);

        let mut policy = AdmissionPolicy::new("tenant/isolated", 4);
        policy.allowed_runtime_features = string_set(&["effects"]);
        policy.allowed_external_effect_roots = string_set(&["DB"]);
        policy.minimum_host_boundary_isolation_level = 2;
        let environment = AdmissionEnvironment {
            available_runtime_features: string_set(&["effects"]),
            isolation_level: 1,
        };

        let decision = evaluate_admission(&bytes, &manifest, &policy, &environment);
        assert!(!decision.admitted);
        assert_eq!(decision.required_isolation_level, 2);
        assert!(decision.reasons.iter().any(|reason| matches!(
            reason,
            AdmissionReason::IsolationLevelInsufficient {
                required: 2,
                actual: 1
            }
        )));
    }

    #[test]
    fn spawn_authority_is_exact_and_fail_closed() {
        let mut module = CodeModule::new("spawn");
        module.spawn_capability_grants.push((
            0,
            vec!["Net::TcpOut(api.example.com:443)".to_string()],
        ));
        let bytes = nbc(&module);
        let manifest = manifest(&bytes);

        let mut policy = AdmissionPolicy::new("tenant/agents", 2);
        policy.allowed_runtime_features = string_set(&["actors", "heap"]);
        let environment = AdmissionEnvironment {
            available_runtime_features: string_set(&["actors", "heap"]),
            isolation_level: 0,
        };

        let denied = evaluate_admission(&bytes, &manifest, &policy, &environment);
        assert!(!denied.admitted);
        assert!(denied.reasons.iter().any(|reason| matches!(
            reason,
            AdmissionReason::SpawnCapabilityDenied { capability }
                if capability == "Net::TcpOut(api.example.com:443)"
        )));

        policy.allowed_spawn_capabilities =
            string_set(&["Net::TcpOut(api.example.com:443)"]);
        let admitted = evaluate_admission(&bytes, &manifest, &policy, &environment);
        assert!(admitted.admitted, "{:?}", admitted.reasons);
    }

    #[test]
    fn ffi_authority_is_exact_unless_library_is_deliberately_granted() {
        let mut module = CodeModule::new("ffi");
        module.foreign_functions.push(ForeignFunctionDef {
            library: "libc.so.6".to_string(),
            symbol: "getpid".to_string(),
            params: vec![],
            ret: FfiType::Int,
        });
        let bytes = nbc(&module);
        let manifest = manifest(&bytes);

        let mut policy = AdmissionPolicy::new("tenant/ffi", 1);
        policy.allowed_runtime_features = string_set(&["ffi"]);
        let environment = AdmissionEnvironment {
            available_runtime_features: string_set(&["ffi"]),
            isolation_level: 0,
        };

        let denied = evaluate_admission(&bytes, &manifest, &policy, &environment);
        assert!(!denied.admitted);
        assert!(denied.reasons.iter().any(|reason| matches!(
            reason,
            AdmissionReason::FfiFunctionDenied { function }
                if function == "libc.so.6::getpid"
        )));

        policy.allowed_ffi_functions = string_set(&["libc.so.6::getpid"]);
        let exact = evaluate_admission(&bytes, &manifest, &policy, &environment);
        assert!(exact.admitted, "{:?}", exact.reasons);

        policy.allowed_ffi_functions.clear();
        policy.allowed_ffi_libraries = string_set(&["libc.so.6"]);
        let broad = evaluate_admission(&bytes, &manifest, &policy, &environment);
        assert!(broad.admitted, "{:?}", broad.reasons);
    }

    #[test]
    fn corrupt_artifact_is_denied_without_panicking() {
        let policy = AdmissionPolicy::new("tenant/default", 1);
        let environment = AdmissionEnvironment::default();

        let decision =
            evaluate_artifact_admission(b"not-an-nbc", &policy, &environment);
        assert!(!decision.admitted);
        assert!(decision
            .reasons
            .iter()
            .any(|reason| matches!(reason, AdmissionReason::ArtifactDecodeFailed { .. })));
    }
}
