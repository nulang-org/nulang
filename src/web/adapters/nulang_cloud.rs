//! Nulang Cloud deployment admission for compiler-emitted Behavior Manifests.
//!
//! The compiler owns program semantics. Cloud admission verifies the immutable
//! compiler contract and applies platform policy; it must never infer missing
//! authority/effect semantics from deployment configuration.

use crate::behavior_manifest::{
    ArtifactKind, AuthorityEntry, AuthorityKind, BehaviorManifest, ReplayClass,
    BEHAVIOR_SCHEMA_V0ALPHA1,
};
use std::collections::BTreeSet;
use std::fmt;

pub const NAME: &str = "nulang_cloud";

/// Platform policy applied *after* the compiler contract has been verified.
///
/// This policy can only reject compiler-declared behavior. It cannot add an
/// authority, weaken replay requirements, or replace the manifest's artifact
/// identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudAdmissionPolicy {
    pub allowed_authority_kinds: BTreeSet<AuthorityKind>,
    pub allow_nonreplayable_external_effects: bool,
}

impl CloudAdmissionPolicy {
    /// Contract-only verification useful in the CLI before upload. The remote
    /// Cloud control plane must still apply its tenant/platform policy.
    pub fn contract_verification_only() -> Self {
        Self {
            allowed_authority_kinds: all_authority_kinds(),
            allow_nonreplayable_external_effects: true,
        }
    }

    /// Conservative managed-runtime baseline. Custom authority and
    /// non-replayable external effects require an explicit policy decision.
    pub fn managed_default() -> Self {
        let mut allowed = all_authority_kinds();
        allowed.remove(&AuthorityKind::Custom);
        Self {
            allowed_authority_kinds: allowed,
            allow_nonreplayable_external_effects: false,
        }
    }
}

impl Default for CloudAdmissionPolicy {
    fn default() -> Self {
        Self::managed_default()
    }
}

fn all_authority_kinds() -> BTreeSet<AuthorityKind> {
    [
        AuthorityKind::Filesystem,
        AuthorityKind::Network,
        AuthorityKind::Secret,
        AuthorityKind::Inference,
        AuthorityKind::Payment,
        AuthorityKind::State,
        AuthorityKind::ActorDelegation,
        AuthorityKind::Custom,
    ]
    .into_iter()
    .collect()
}

/// Verified deployment facts suitable for scheduling/admission telemetry.
///
/// Values here are copied only from the compiler manifest after artifact
/// binding succeeds; no deployment config can widen them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudAdmissionPlan {
    pub package_name: String,
    pub artifact_kind: ArtifactKind,
    pub required_authority: Vec<AuthorityEntry>,
    pub durable_actors: Vec<String>,
    pub idempotency_key_effects: Vec<String>,
    pub nonreplayable_effects: Vec<String>,
}

/// Validate a Behavior Manifest against the exact artifact bytes and platform
/// policy.
///
/// Order is intentional: schema/language/artifact binding is checked before
/// policy. An attacker cannot use policy errors as a way to get an unbound or
/// unknown-version manifest interpreted.
pub fn admit_behavior_manifest(
    manifest: &BehaviorManifest,
    expected_artifact_kind: ArtifactKind,
    artifact_bytes: &[u8],
    policy: &CloudAdmissionPolicy,
) -> Result<CloudAdmissionPlan, CloudAdmissionError> {
    if manifest.schema != BEHAVIOR_SCHEMA_V0ALPHA1 {
        return Err(CloudAdmissionError::UnsupportedSchema {
            actual: manifest.schema.clone(),
        });
    }

    let expected_language = crate::format::constants::LANGUAGE_VERSION_STR;
    if manifest.package.language_version != expected_language {
        return Err(CloudAdmissionError::LanguageVersionMismatch {
            expected: expected_language.to_string(),
            actual: manifest.package.language_version.clone(),
        });
    }

    if manifest.artifact.kind != expected_artifact_kind {
        return Err(CloudAdmissionError::ArtifactKindMismatch {
            expected: expected_artifact_kind,
            actual: manifest.artifact.kind.clone(),
        });
    }

    let actual_digest = format!("blake3:{}", blake3::hash(artifact_bytes).to_hex());
    if manifest.artifact.digest != actual_digest {
        return Err(CloudAdmissionError::ArtifactDigestMismatch {
            expected: manifest.artifact.digest.clone(),
            actual: actual_digest,
        });
    }

    // Compiler output should contain exactly one replay contract per effect.
    // Refuse duplicate/incomplete metadata rather than guessing from the first
    // matching entry.
    let effect_names: BTreeSet<_> = manifest.effects.iter().map(|e| e.effect.as_str()).collect();
    if effect_names.len() != manifest.effects.len() {
        return Err(CloudAdmissionError::DuplicateEffectInventory);
    }
    let replay_names: BTreeSet<_> = manifest.replay.iter().map(|e| e.effect.as_str()).collect();
    if replay_names.len() != manifest.replay.len() {
        return Err(CloudAdmissionError::DuplicateReplayInventory);
    }
    if effect_names != replay_names {
        let missing = effect_names
            .difference(&replay_names)
            .map(|s| (*s).to_string())
            .collect();
        let unknown = replay_names
            .difference(&effect_names)
            .map(|s| (*s).to_string())
            .collect();
        return Err(CloudAdmissionError::ReplayInventoryMismatch { missing, unknown });
    }

    let mut required_authority = Vec::new();
    for authority in manifest.authority.iter().filter(|entry| entry.required) {
        if !policy.allowed_authority_kinds.contains(&authority.kind) {
            return Err(CloudAdmissionError::AuthorityDenied {
                kind: authority.kind,
                resource: authority.resource.clone(),
            });
        }
        required_authority.push(authority.clone());
    }

    let mut idempotency_key_effects = Vec::new();
    let mut nonreplayable_effects = Vec::new();
    for replay in &manifest.replay {
        match replay.class {
            ReplayClass::ExternalRequiresIdempotencyKey => {
                idempotency_key_effects.push(replay.effect.clone());
            }
            ReplayClass::ExternalNonreplayable => {
                nonreplayable_effects.push(replay.effect.clone());
            }
            _ => {}
        }
    }
    if !policy.allow_nonreplayable_external_effects && !nonreplayable_effects.is_empty() {
        return Err(CloudAdmissionError::NonreplayableExternalEffects {
            effects: nonreplayable_effects,
        });
    }

    let mut durable_actors: Vec<_> = manifest
        .actors
        .iter()
        .filter(|actor| {
            matches!(
                actor.durability,
                crate::behavior_manifest::PersistenceClass::Durable
            )
        })
        .map(|actor| actor.name.clone())
        .collect();
    durable_actors.sort();

    required_authority.sort_by(|left, right| {
        (
            left.kind,
            left.resource.as_deref().unwrap_or(""),
            left.operations.as_slice(),
        )
            .cmp(&(
                right.kind,
                right.resource.as_deref().unwrap_or(""),
                right.operations.as_slice(),
            ))
    });
    idempotency_key_effects.sort();
    nonreplayable_effects.sort();

    Ok(CloudAdmissionPlan {
        package_name: manifest.package.name.clone(),
        artifact_kind: manifest.artifact.kind.clone(),
        required_authority,
        durable_actors,
        idempotency_key_effects,
        nonreplayable_effects,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudAdmissionError {
    UnsupportedSchema {
        actual: String,
    },
    LanguageVersionMismatch {
        expected: String,
        actual: String,
    },
    ArtifactKindMismatch {
        expected: ArtifactKind,
        actual: ArtifactKind,
    },
    ArtifactDigestMismatch {
        expected: String,
        actual: String,
    },
    DuplicateEffectInventory,
    DuplicateReplayInventory,
    ReplayInventoryMismatch {
        missing: Vec<String>,
        unknown: Vec<String>,
    },
    AuthorityDenied {
        kind: AuthorityKind,
        resource: Option<String>,
    },
    NonreplayableExternalEffects {
        effects: Vec<String>,
    },
}

impl fmt::Display for CloudAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema { actual } => {
                write!(f, "unsupported Behavior Manifest schema '{actual}'")
            }
            Self::LanguageVersionMismatch { expected, actual } => write!(
                f,
                "Behavior Manifest language version mismatch: expected {expected}, got {actual}"
            ),
            Self::ArtifactKindMismatch { expected, actual } => write!(
                f,
                "Behavior Manifest artifact kind mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::ArtifactDigestMismatch { expected, actual } => write!(
                f,
                "Behavior Manifest artifact digest mismatch: manifest {expected}, artifact {actual}"
            ),
            Self::DuplicateEffectInventory => {
                write!(f, "Behavior Manifest contains duplicate effect entries")
            }
            Self::DuplicateReplayInventory => {
                write!(f, "Behavior Manifest contains duplicate replay entries")
            }
            Self::ReplayInventoryMismatch { missing, unknown } => write!(
                f,
                "Behavior Manifest replay inventory mismatch: missing={missing:?}, unknown={unknown:?}"
            ),
            Self::AuthorityDenied { kind, resource } => write!(
                f,
                "Nulang Cloud policy denies required authority {kind:?}{}",
                resource
                    .as_deref()
                    .map(|value| format!(" ({value})"))
                    .unwrap_or_default()
            ),
            Self::NonreplayableExternalEffects { effects } => write!(
                f,
                "Nulang Cloud policy denies non-replayable external effects: {}",
                effects.join(", ")
            ),
        }
    }
}

impl std::error::Error for CloudAdmissionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::behavior_manifest::{
        ActorEntry, ArtifactIdentity, CompilerIdentity, Determinism, DurabilityEntry, EffectClass,
        EffectEntry, EffectReplay, PackageIdentity, PersistenceClass, Provenance, ReplayEntry,
        Resources,
    };

    fn manifest_for(bytes: &[u8]) -> BehaviorManifest {
        BehaviorManifest {
            schema: BEHAVIOR_SCHEMA_V0ALPHA1.to_string(),
            package: PackageIdentity {
                name: "demo".to_string(),
                version: "0.1.0".to_string(),
                language_version: crate::format::constants::LANGUAGE_VERSION_STR.to_string(),
            },
            artifact: ArtifactIdentity {
                kind: ArtifactKind::Bytecode,
                digest: format!("blake3:{}", blake3::hash(bytes).to_hex()),
            },
            compiler: CompilerIdentity {
                implementation: "nulang-rust".to_string(),
                version: "test".to_string(),
                digest: format!("blake3:{}", blake3::hash(b"compiler").to_hex()),
            },
            interfaces: Vec::new(),
            actors: vec![ActorEntry {
                name: "Counter".to_string(),
                protocol: None,
                durability: PersistenceClass::Durable,
                state_schema: Some("blake3:state".to_string()),
            }],
            effects: vec![EffectEntry {
                effect: "Comms.send".to_string(),
                class: EffectClass::External,
                determinism: Determinism::Nondeterministic,
                replay: EffectReplay::RequiresIdempotencyKey,
                cost_class: None,
            }],
            authority: vec![AuthorityEntry {
                kind: AuthorityKind::Network,
                resource: Some("host-effect:Comms".to_string()),
                operations: Vec::new(),
                required: true,
            }],
            durability: vec![DurabilityEntry {
                owner: "Counter".to_string(),
                persistence: PersistenceClass::Durable,
                schema: Some("blake3:state".to_string()),
                migration_contract: None,
            }],
            replay: vec![ReplayEntry {
                effect: "Comms.send".to_string(),
                class: ReplayClass::ExternalRequiresIdempotencyKey,
            }],
            resources: Resources::default(),
            provenance: Provenance {
                source_digest: format!("blake3:{}", blake3::hash(b"source").to_hex()),
                dependency_digest: format!("blake3:{}", blake3::hash(b"deps").to_hex()),
                interface_digests: Vec::new(),
                state_schema_digests: Vec::new(),
            },
        }
    }

    #[test]
    fn exact_bytecode_manifest_is_admitted() {
        let bytes = b"exact-nbc";
        let manifest = manifest_for(bytes);
        let plan = admit_behavior_manifest(
            &manifest,
            ArtifactKind::Bytecode,
            bytes,
            &CloudAdmissionPolicy::managed_default(),
        )
        .unwrap();
        assert_eq!(plan.package_name, "demo");
        assert_eq!(plan.durable_actors, vec!["Counter"]);
        assert_eq!(plan.idempotency_key_effects, vec!["Comms.send"]);
    }

    #[test]
    fn artifact_substitution_is_rejected_before_policy() {
        let manifest = manifest_for(b"original");
        let err = admit_behavior_manifest(
            &manifest,
            ArtifactKind::Bytecode,
            b"substituted",
            &CloudAdmissionPolicy::contract_verification_only(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            CloudAdmissionError::ArtifactDigestMismatch { .. }
        ));
    }

    #[test]
    fn managed_policy_rejects_custom_authority() {
        let bytes = b"artifact";
        let mut manifest = manifest_for(bytes);
        manifest.authority[0].kind = AuthorityKind::Custom;
        let err = admit_behavior_manifest(
            &manifest,
            ArtifactKind::Bytecode,
            bytes,
            &CloudAdmissionPolicy::managed_default(),
        )
        .unwrap_err();
        assert!(matches!(err, CloudAdmissionError::AuthorityDenied { .. }));
    }

    #[test]
    fn nonreplayable_effect_requires_explicit_policy() {
        let bytes = b"artifact";
        let mut manifest = manifest_for(bytes);
        manifest.effects[0].replay = EffectReplay::Nonreplayable;
        manifest.replay[0].class = ReplayClass::ExternalNonreplayable;

        let err = admit_behavior_manifest(
            &manifest,
            ArtifactKind::Bytecode,
            bytes,
            &CloudAdmissionPolicy::managed_default(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            CloudAdmissionError::NonreplayableExternalEffects { .. }
        ));

        assert!(admit_behavior_manifest(
            &manifest,
            ArtifactKind::Bytecode,
            bytes,
            &CloudAdmissionPolicy::contract_verification_only(),
        )
        .is_ok());
    }

    #[test]
    fn duplicate_replay_contract_is_rejected() {
        let bytes = b"artifact";
        let mut manifest = manifest_for(bytes);
        manifest.replay.push(manifest.replay[0].clone());
        let err = admit_behavior_manifest(
            &manifest,
            ArtifactKind::Bytecode,
            bytes,
            &CloudAdmissionPolicy::contract_verification_only(),
        )
        .unwrap_err();
        assert_eq!(err, CloudAdmissionError::DuplicateReplayInventory);
    }

    #[test]
    fn incomplete_replay_inventory_is_rejected() {
        let bytes = b"artifact";
        let mut manifest = manifest_for(bytes);
        manifest.replay.clear();
        let err = admit_behavior_manifest(
            &manifest,
            ArtifactKind::Bytecode,
            bytes,
            &CloudAdmissionPolicy::contract_verification_only(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            CloudAdmissionError::ReplayInventoryMismatch { .. }
        ));
    }
}
