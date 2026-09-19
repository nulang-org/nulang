//! Typed model for RFC 0020 Behavior Manifests.
//!
//! This module is deliberately independent from HIR/MIR and backend layout.
//! It is the compiler-owned deployment contract for artifact identity,
//! externally relevant semantics, and host ABI admission metadata.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::host_effect_abi::{lookup_host_operation_by_identity, HOST_EFFECT_ABI_SCHEMA};

pub const BEHAVIOR_MANIFEST_SCHEMA: &str = "nulang.behavior/v0alpha1";

const HOST_EFFECT_DESCRIPTOR_BYTES: &[u8] =
    include_bytes!("../spec/host-effects/v0alpha1.json");

pub fn blake3_digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

pub fn host_effect_descriptor_digest() -> String {
    blake3_digest(HOST_EFFECT_DESCRIPTOR_BYTES)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorManifest {
    pub schema: String,
    pub package: PackageIdentity,
    pub artifact: ArtifactIdentity,
    pub compiler: CompilerIdentity,
    pub interfaces: Vec<InterfaceEntry>,
    pub actors: Vec<ActorEntry>,
    pub effects: Vec<EffectEntry>,
    pub authority: Vec<AuthorityEntry>,
    pub durability: Vec<DurabilityEntry>,
    pub replay: Vec<ReplayEntry>,
    pub resources: Resources,
    pub provenance: Provenance,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_abi: Option<HostAbiBinding>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl BehaviorManifest {
    pub fn new(
        package: PackageIdentity,
        artifact: ArtifactIdentity,
        compiler: CompilerIdentity,
        provenance: Provenance,
    ) -> Self {
        Self {
            schema: BEHAVIOR_MANIFEST_SCHEMA.to_string(),
            package,
            artifact,
            compiler,
            interfaces: Vec::new(),
            actors: Vec::new(),
            effects: Vec::new(),
            authority: Vec::new(),
            durability: Vec::new(),
            replay: Vec::new(),
            resources: Resources::default(),
            provenance,
            host_abi: None,
            extensions: BTreeMap::new(),
        }
    }

    /// Bind the exact canonical host operations required by this artifact.
    ///
    /// Callers pass compiler-owned canonical identities, never dotted source
    /// spellings. Unknown identities fail closed.
    pub fn bind_current_host_operations<'a, I>(&mut self, operations: I) -> Result<(), String>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut required = Vec::new();
        for (effect_id, operation_id) in operations {
            if lookup_host_operation_by_identity(effect_id, operation_id).is_none() {
                return Err(format!(
                    "unknown host operation identity: {effect_id}#{operation_id}"
                ));
            }
            required.push(HostOperationIdentity {
                effect_id: effect_id.to_string(),
                operation_id: operation_id.to_string(),
            });
        }

        required.sort();
        required.dedup();
        self.host_abi = if required.is_empty() {
            None
        } else {
            Some(HostAbiBinding {
                schema: HOST_EFFECT_ABI_SCHEMA.to_string(),
                descriptor_digest: host_effect_descriptor_digest(),
                operations: required,
            })
        };
        Ok(())
    }

    /// Validate security-relevant host ABI metadata against this compiler.
    ///
    /// Unknown ABI versions, descriptor bytes, or operation identities are
    /// rejected rather than interpreted heuristically by a deployment host.
    pub fn validate_host_abi(&self) -> Result<(), String> {
        let Some(binding) = &self.host_abi else {
            return Ok(());
        };

        if binding.schema != HOST_EFFECT_ABI_SCHEMA {
            return Err(format!("unsupported host ABI schema: {}", binding.schema));
        }

        let expected_digest = host_effect_descriptor_digest();
        if binding.descriptor_digest != expected_digest {
            return Err(format!(
                "host ABI descriptor digest mismatch: expected {expected_digest}, got {}",
                binding.descriptor_digest
            ));
        }

        if binding.operations.is_empty() {
            return Err("host ABI binding must contain at least one operation".to_string());
        }

        for operation in &binding.operations {
            if lookup_host_operation_by_identity(&operation.effect_id, &operation.operation_id)
                .is_none()
            {
                return Err(format!(
                    "unknown required host operation: {}#{}",
                    operation.effect_id, operation.operation_id
                ));
            }
        }

        Ok(())
    }

    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageIdentity {
    pub name: String,
    pub version: String,
    pub language_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactIdentity {
    pub kind: ArtifactKind,
    pub digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Bytecode,
    Native,
    WasmModule,
    WasmComponent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompilerIdentity {
    pub implementation: String,
    pub version: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceEntry {
    pub name: String,
    pub input: String,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub durability: DurabilityClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_schema: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DurabilityClass {
    Transient,
    Durable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectEntry {
    pub effect: String,
    pub class: EffectClass,
    pub determinism: Determinism,
    pub replay: EffectReplay,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_class: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectClass {
    Pure,
    Local,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Determinism {
    Deterministic,
    Nondeterministic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectReplay {
    None,
    Safe,
    RequiresJournal,
    RequiresIdempotencyKey,
    Nonreplayable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityEntry {
    pub kind: AuthorityKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_required() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthorityKind {
    Filesystem,
    Network,
    Secret,
    Inference,
    Payment,
    State,
    ActorDelegation,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurabilityEntry {
    pub owner: String,
    pub persistence: DurabilityClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_contract: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayEntry {
    pub effect: String,
    pub class: ReplayClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReplayClass {
    Pure,
    LocalReplaySafe,
    JournalResult,
    ExternalIdempotent,
    ExternalRequiresIdempotencyKey,
    ExternalNonreplayable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_min_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_weight: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accelerator_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_budget_microusd: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub residency: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_egress_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source_digest: String,
    pub dependency_digest: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interface_digests: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state_schema_digests: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostAbiBinding {
    pub schema: String,
    pub descriptor_digest: String,
    pub operations: Vec<HostOperationIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HostOperationIdentity {
    pub effect_id: String,
    pub operation_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> BehaviorManifest {
        BehaviorManifest::new(
            PackageIdentity {
                name: "demo".to_string(),
                version: "0.1.0".to_string(),
                language_version: "1.0.0-frozen".to_string(),
            },
            ArtifactIdentity {
                kind: ArtifactKind::WasmModule,
                digest: blake3_digest(b"wasm"),
            },
            CompilerIdentity {
                implementation: "nulang-rust".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                digest: blake3_digest(b"compiler"),
            },
            Provenance {
                source_digest: blake3_digest(b"source"),
                dependency_digest: blake3_digest(b"deps"),
                interface_digests: Vec::new(),
                state_schema_digests: Vec::new(),
            },
        )
    }

    #[test]
    fn digest_uses_schema_compatible_blake3_shape() {
        let digest = blake3_digest(b"nulang");
        assert!(digest.starts_with("blake3:"));
        assert_eq!(digest.len(), "blake3:".len() + 64);
    }

    #[test]
    fn host_binding_contains_only_canonical_runtime_identity() {
        let mut manifest = manifest();
        manifest
            .bind_current_host_operations([("nulang:storage/string", "Write")])
            .unwrap();

        manifest.validate_host_abi().unwrap();
        let json = manifest.to_pretty_json().unwrap();
        assert!(json.contains("nulang.host-effects/v0alpha1"));
        assert!(json.contains("nulang:storage/string"));
        assert!(json.contains("\"Write\""));
        assert!(!json.contains("Storage.write"));
        assert!(!json.contains("source_effect"));
        assert!(!json.contains("source_operation"));
    }

    #[test]
    fn unknown_host_abi_schema_fails_closed() {
        let mut manifest = manifest();
        manifest
            .bind_current_host_operations([("nulang:storage/string", "Read")])
            .unwrap();
        manifest.host_abi.as_mut().unwrap().schema = "nulang.host-effects/v999".to_string();

        assert!(manifest.validate_host_abi().is_err());
    }

    #[test]
    fn unknown_host_operation_fails_closed() {
        let mut manifest = manifest();
        manifest.host_abi = Some(HostAbiBinding {
            schema: HOST_EFFECT_ABI_SCHEMA.to_string(),
            descriptor_digest: host_effect_descriptor_digest(),
            operations: vec![HostOperationIdentity {
                effect_id: "nulang:unknown/unknown".to_string(),
                operation_id: "Run".to_string(),
            }],
        });

        assert!(manifest.validate_host_abi().is_err());
    }
}
