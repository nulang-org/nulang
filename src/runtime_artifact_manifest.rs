//! Versioned runtime provenance for retained executable artifacts.
//!
//! Frozen NBC v1 intentionally does not carry compiler semantic sidecars.
//! Historical durable execution therefore needs an additive manifest that
//! binds one exact `ArtifactId` to the whole-program semantic identity and
//! the definition-scoped identities parallel to `CodeModule::actor_metadata`.
//!
//! Source identity is deliberately excluded from this runtime manifest:
//! formatting-only source changes may produce the same semantic/codegen
//! artifact and must remain idempotent for historical execution.

use crate::artifact_identity::ArtifactIdentityManifest;
use crate::bytecode::CodeModule;
use crate::content_identity::{ArtifactId, SemanticId};
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

pub const RUNTIME_ARTIFACT_MANIFEST_VERSION: u16 = 1;

/// Definition-scoped semantic identity at one immutable actor-metadata index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDefinitionIdentity {
    pub metadata_index: u32,
    pub name: String,
    pub semantic_id: SemanticId,
}

/// Runtime-relevant identity required to reconstruct a fully identified module
/// after decoding frozen NBC bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeArtifactManifest {
    artifact_id: ArtifactId,
    semantic_id: SemanticId,
    compiler_version: String,
    target: String,
    abi: String,
    backend: String,
    flags: BTreeSet<String>,
    module_name: String,
    definitions: Vec<RuntimeDefinitionIdentity>,
}

impl RuntimeArtifactManifest {
    /// Build a runtime manifest only from a module that already carries
    /// compiler-proven semantic and exact artifact identity sidecars.
    pub fn from_module(
        module: &CodeModule,
        artifact_identity: &ArtifactIdentityManifest,
    ) -> Result<Self, RuntimeArtifactManifestError> {
        let module_semantic = module
            .semantic_id
            .ok_or(RuntimeArtifactManifestError::MissingModuleSemanticIdentity)?;
        if module_semantic != artifact_identity.semantic_id() {
            return Err(RuntimeArtifactManifestError::ProgramSemanticMismatch {
                module: module_semantic,
                manifest: artifact_identity.semantic_id(),
            });
        }

        let module_artifact = module
            .artifact_id
            .ok_or(RuntimeArtifactManifestError::MissingModuleArtifactIdentity)?;
        if module_artifact != artifact_identity.artifact_id() {
            return Err(RuntimeArtifactManifestError::ArtifactIdentityMismatch {
                module: module_artifact,
                manifest: artifact_identity.artifact_id(),
            });
        }

        if module.actor_metadata.len() != module.actor_semantic_ids.len() {
            return Err(RuntimeArtifactManifestError::DefinitionCountMismatch {
                metadata: module.actor_metadata.len(),
                identities: module.actor_semantic_ids.len(),
            });
        }

        let definitions = module
            .actor_metadata
            .iter()
            .zip(module.actor_semantic_ids.iter().copied())
            .enumerate()
            .map(|(index, (metadata, semantic_id))| RuntimeDefinitionIdentity {
                metadata_index: index as u32,
                name: metadata.name.clone(),
                semantic_id,
            })
            .collect();

        Ok(Self {
            artifact_id: artifact_identity.artifact_id(),
            semantic_id: artifact_identity.semantic_id(),
            compiler_version: artifact_identity.compiler_version().to_string(),
            target: artifact_identity.target().to_string(),
            abi: artifact_identity.abi().to_string(),
            backend: artifact_identity.backend().to_string(),
            flags: artifact_identity.flags().map(str::to_string).collect(),
            module_name: module.name.clone(),
            definitions,
        })
    }

    pub fn artifact_id(&self) -> ArtifactId {
        self.artifact_id
    }

    pub fn semantic_id(&self) -> SemanticId {
        self.semantic_id
    }

    pub fn module_name(&self) -> &str {
        &self.module_name
    }

    pub fn definitions(&self) -> &[RuntimeDefinitionIdentity] {
        &self.definitions
    }

    /// Recreate the runtime-relevant artifact identity manifest. SourceId is
    /// intentionally absent because it does not participate in ArtifactId.
    pub fn artifact_identity(&self) -> ArtifactIdentityManifest {
        ArtifactIdentityManifest::new(
            None,
            self.semantic_id,
            self.compiler_version.clone(),
            self.target.clone(),
            self.abi.clone(),
            self.backend.clone(),
            self.flags.iter(),
        )
    }

    /// Bind identities back onto an NBC-decoded module after verifying the
    /// persisted metadata layout. No existing conflicting sidecar is replaced.
    pub fn bind_module(
        &self,
        module: &mut CodeModule,
    ) -> Result<(), RuntimeArtifactManifestError> {
        if module.name != self.module_name {
            return Err(RuntimeArtifactManifestError::ModuleNameMismatch {
                module: module.name.clone(),
                manifest: self.module_name.clone(),
            });
        }

        if module.actor_metadata.len() != self.definitions.len() {
            return Err(RuntimeArtifactManifestError::DefinitionCountMismatch {
                metadata: module.actor_metadata.len(),
                identities: self.definitions.len(),
            });
        }

        for (expected_index, (metadata, definition)) in module
            .actor_metadata
            .iter()
            .zip(self.definitions.iter())
            .enumerate()
        {
            if definition.metadata_index as usize != expected_index {
                return Err(RuntimeArtifactManifestError::DefinitionIndexMismatch {
                    expected: expected_index,
                    actual: definition.metadata_index as usize,
                });
            }
            if metadata.name != definition.name {
                return Err(RuntimeArtifactManifestError::DefinitionNameMismatch {
                    index: expected_index,
                    module: metadata.name.clone(),
                    manifest: definition.name.clone(),
                });
            }
        }

        if let Some(existing) = module.semantic_id {
            if existing != self.semantic_id {
                return Err(RuntimeArtifactManifestError::ProgramSemanticMismatch {
                    module: existing,
                    manifest: self.semantic_id,
                });
            }
        }
        if let Some(existing) = module.artifact_id {
            if existing != self.artifact_id {
                return Err(RuntimeArtifactManifestError::ArtifactIdentityMismatch {
                    module: existing,
                    manifest: self.artifact_id,
                });
            }
        }

        let definition_ids: Vec<_> = self
            .definitions
            .iter()
            .map(|definition| definition.semantic_id)
            .collect();
        if !module.actor_semantic_ids.is_empty() && module.actor_semantic_ids != definition_ids {
            return Err(RuntimeArtifactManifestError::ExistingDefinitionIdentityMismatch);
        }

        module.semantic_id = Some(self.semantic_id);
        module.artifact_id = Some(self.artifact_id);
        module.actor_semantic_ids = definition_ids;
        Ok(())
    }

    pub fn to_json(&self) -> Result<Vec<u8>, RuntimeArtifactManifestError> {
        let persisted = PersistedRuntimeArtifactManifestV1 {
            version: RUNTIME_ARTIFACT_MANIFEST_VERSION,
            artifact_id: self.artifact_id.to_string(),
            semantic_id: self.semantic_id.to_string(),
            compiler_version: self.compiler_version.clone(),
            target: self.target.clone(),
            abi: self.abi.clone(),
            backend: self.backend.clone(),
            flags: self.flags.iter().cloned().collect(),
            module_name: self.module_name.clone(),
            definitions: self
                .definitions
                .iter()
                .map(|definition| PersistedRuntimeDefinitionIdentityV1 {
                    metadata_index: definition.metadata_index,
                    name: definition.name.clone(),
                    semantic_id: definition.semantic_id.to_string(),
                })
                .collect(),
        };
        serde_json::to_vec(&persisted).map_err(RuntimeArtifactManifestError::from)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, RuntimeArtifactManifestError> {
        let persisted: PersistedRuntimeArtifactManifestV1 =
            serde_json::from_slice(bytes).map_err(RuntimeArtifactManifestError::from)?;
        if persisted.version != RUNTIME_ARTIFACT_MANIFEST_VERSION {
            return Err(RuntimeArtifactManifestError::UnsupportedVersion {
                actual: persisted.version,
            });
        }

        let semantic_id = parse_identity::<SemanticId>("semantic_id", &persisted.semantic_id)?;
        let artifact_id = parse_identity::<ArtifactId>("artifact_id", &persisted.artifact_id)?;
        let flags: BTreeSet<String> = persisted.flags.into_iter().collect();
        let expected_artifact = ArtifactId::from_semantic(
            semantic_id,
            &persisted.compiler_version,
            &persisted.target,
            &persisted.abi,
            &persisted.backend,
            flags.iter(),
        );
        if artifact_id != expected_artifact {
            return Err(RuntimeArtifactManifestError::ArtifactIdentityMismatch {
                module: expected_artifact,
                manifest: artifact_id,
            });
        }

        let mut definitions = Vec::with_capacity(persisted.definitions.len());
        for (expected_index, definition) in persisted.definitions.into_iter().enumerate() {
            if definition.metadata_index as usize != expected_index {
                return Err(RuntimeArtifactManifestError::DefinitionIndexMismatch {
                    expected: expected_index,
                    actual: definition.metadata_index as usize,
                });
            }
            definitions.push(RuntimeDefinitionIdentity {
                metadata_index: definition.metadata_index,
                name: definition.name,
                semantic_id: parse_identity::<SemanticId>(
                    "definition.semantic_id",
                    &definition.semantic_id,
                )?,
            });
        }

        Ok(Self {
            artifact_id,
            semantic_id,
            compiler_version: persisted.compiler_version,
            target: persisted.target,
            abi: persisted.abi,
            backend: persisted.backend,
            flags,
            module_name: persisted.module_name,
            definitions,
        })
    }
}

fn parse_identity<T>(
    field: &'static str,
    value: &str,
) -> Result<T, RuntimeArtifactManifestError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|error| RuntimeArtifactManifestError::InvalidIdentity {
            field,
            message: error.to_string(),
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeArtifactManifestError {
    Json(String),
    UnsupportedVersion {
        actual: u16,
    },
    InvalidIdentity {
        field: &'static str,
        message: String,
    },
    MissingModuleSemanticIdentity,
    MissingModuleArtifactIdentity,
    ProgramSemanticMismatch {
        module: SemanticId,
        manifest: SemanticId,
    },
    ArtifactIdentityMismatch {
        module: ArtifactId,
        manifest: ArtifactId,
    },
    ModuleNameMismatch {
        module: String,
        manifest: String,
    },
    DefinitionCountMismatch {
        metadata: usize,
        identities: usize,
    },
    DefinitionIndexMismatch {
        expected: usize,
        actual: usize,
    },
    DefinitionNameMismatch {
        index: usize,
        module: String,
        manifest: String,
    },
    ExistingDefinitionIdentityMismatch,
}

impl fmt::Display for RuntimeArtifactManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid runtime artifact manifest: {message}"),
            Self::UnsupportedVersion { actual } => write!(
                f,
                "unsupported runtime artifact manifest version {actual}; runtime supports version {RUNTIME_ARTIFACT_MANIFEST_VERSION}"
            ),
            Self::InvalidIdentity { field, message } => {
                write!(f, "invalid {field} in runtime artifact manifest: {message}")
            }
            Self::MissingModuleSemanticIdentity => {
                write!(f, "runtime artifact requires compiler-proven module semantic identity")
            }
            Self::MissingModuleArtifactIdentity => {
                write!(f, "runtime artifact requires exact module ArtifactId")
            }
            Self::ProgramSemanticMismatch { module, manifest } => write!(
                f,
                "module semantic identity {module} does not match runtime manifest semantic identity {manifest}"
            ),
            Self::ArtifactIdentityMismatch { module, manifest } => write!(
                f,
                "module ArtifactId {module} does not match runtime manifest ArtifactId {manifest}"
            ),
            Self::ModuleNameMismatch { module, manifest } => write!(
                f,
                "module name {module:?} does not match runtime manifest module name {manifest:?}"
            ),
            Self::DefinitionCountMismatch {
                metadata,
                identities,
            } => write!(
                f,
                "runtime artifact definition count mismatch: module has {metadata} actor metadata entries but manifest has {identities} identities"
            ),
            Self::DefinitionIndexMismatch { expected, actual } => write!(
                f,
                "runtime artifact definition index mismatch: expected {expected}, got {actual}"
            ),
            Self::DefinitionNameMismatch {
                index,
                module,
                manifest,
            } => write!(
                f,
                "runtime artifact definition {index} name mismatch: module has {module:?}, manifest has {manifest:?}"
            ),
            Self::ExistingDefinitionIdentityMismatch => write!(
                f,
                "module already carries definition identities that conflict with runtime manifest"
            ),
        }
    }
}

impl std::error::Error for RuntimeArtifactManifestError {}

impl From<serde_json::Error> for RuntimeArtifactManifestError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PersistedRuntimeArtifactManifestV1 {
    version: u16,
    artifact_id: String,
    semantic_id: String,
    compiler_version: String,
    target: String,
    abi: String,
    backend: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    flags: Vec<String>,
    module_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    definitions: Vec<PersistedRuntimeDefinitionIdentityV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PersistedRuntimeDefinitionIdentityV1 {
    metadata_index: u32,
    name: String,
    semantic_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{ActorMeta, CodeModule};
    use crate::content_identity::SemanticId;

    fn module_with_identity() -> (CodeModule, ArtifactIdentityManifest) {
        let program = SemanticId::from_canonical_bytes(b"program", []);
        let definition = SemanticId::from_canonical_bytes(b"definition", []);
        let identity = ArtifactIdentityManifest::new(
            None,
            program,
            "nulangc-test",
            "portable",
            "nulang-abi-v1",
            "bytecode",
            ["opt=0"],
        );

        let mut module = CodeModule::new("runtime-manifest-test");
        module.semantic_id = Some(program);
        module.artifact_id = Some(identity.artifact_id());
        module.actor_metadata.push(ActorMeta {
            name: "Counter".to_string(),
            persistent: true,
            state_models: vec![],
            state_defaults: vec![],
            behavior_indices: vec![],
            type_hash: None,
            version: 1,
            migrations: String::new(),
            is_workflow: false,
            is_agent: false,
            is_organization: false,
            is_virtual: false,
            tools: vec![],
            semantic_memory_dimensions: None,
            procedural_memory_namespace: None,
            backend: crate::ast::ActorBackendKind::Native,
            fallback_config: String::new(),
            retry_config: String::new(),
        });
        module.actor_semantic_ids.push(definition);
        (module, identity)
    }

    #[test]
    fn roundtrip_and_bind_restore_frozen_nbc_sidecars() {
        let (module, identity) = module_with_identity();
        let manifest = RuntimeArtifactManifest::from_module(&module, &identity).unwrap();
        let decoded = RuntimeArtifactManifest::from_json(&manifest.to_json().unwrap()).unwrap();
        assert_eq!(decoded, manifest);

        let nbc = module.to_nbc(None).unwrap();
        let mut restored = CodeModule::from_nbc(&nbc).unwrap().module;
        assert_eq!(restored.semantic_id, None);
        assert_eq!(restored.artifact_id, None);
        assert!(restored.actor_semantic_ids.is_empty());

        decoded.bind_module(&mut restored).unwrap();
        assert_eq!(restored.semantic_id, module.semantic_id);
        assert_eq!(restored.artifact_id, module.artifact_id);
        assert_eq!(restored.actor_semantic_ids, module.actor_semantic_ids);
    }

    #[test]
    fn definition_layout_mismatch_fails_closed() {
        let (module, identity) = module_with_identity();
        let manifest = RuntimeArtifactManifest::from_module(&module, &identity).unwrap();
        let mut restored = CodeModule::from_nbc(&module.to_nbc(None).unwrap())
            .unwrap()
            .module;
        restored.actor_metadata[0].name = "Different".to_string();

        assert!(matches!(
            manifest.bind_module(&mut restored),
            Err(RuntimeArtifactManifestError::DefinitionNameMismatch { .. })
        ));
    }

    #[test]
    fn module_requires_exact_artifact_identity() {
        let (mut module, identity) = module_with_identity();
        module.artifact_id = Some(ArtifactId::from_semantic(
            identity.semantic_id(),
            "nulangc-test",
            "portable",
            "nulang-abi-v1",
            "bytecode",
            ["opt=3"],
        ));

        assert!(matches!(
            RuntimeArtifactManifest::from_module(&module, &identity),
            Err(RuntimeArtifactManifestError::ArtifactIdentityMismatch { .. })
        ));
    }
}
