//! Content-addressed retention for compiled Nulang artifacts.
//!
//! Frozen NBC v1 intentionally omits semantic/artifact sidecars. Durable
//! execution nevertheless needs to retain historical executable bytes together
//! with the compiler-owned identities required to interpret them safely.
//! This module provides a backend-neutral retention trait and a local
//! filesystem implementation. A future package registry / Nulang Cloud backend
//! can implement the same contract without changing recovery semantics.
//!
//! The local store is a trusted persistence boundary. It validates the
//! ArtifactIdentityManifest, exact ArtifactId directory key, NBC byte integrity,
//! and actor-sidecar shape before returning an artifact.

use crate::artifact_identity::{ArtifactIdentityError, ArtifactIdentityManifest};
use crate::bytecode::CodeModule;
use crate::content_identity::{ArtifactId, SemanticId};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

pub const RETAINED_ARTIFACT_METADATA_VERSION: u16 = 1;

const IDENTITY_FILE: &str = "identity.json";
const METADATA_FILE: &str = "retention.json";
const NBC_FILE: &str = "artifact.nbc";
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Fully rehydrated historical artifact.
///
/// `module` has the semantic/artifact sidecars restored even though the NBC
/// payload itself remains frozen v1.
#[derive(Debug, Clone)]
pub struct RetainedArtifact {
    pub manifest: ArtifactIdentityManifest,
    pub module: CodeModule,
    pub nbc_bytes: Vec<u8>,
}

/// Retention backend used by durable execution and package tooling.
pub trait ArtifactStore: Send + Sync {
    /// Retain one exact compiled artifact. Repeating the identical write is
    /// idempotent; attempting to reuse an ArtifactId for different bytes or
    /// sidecars fails closed.
    fn retain(
        &self,
        manifest: &ArtifactIdentityManifest,
        module: &CodeModule,
        source_hash: Option<[u8; 32]>,
    ) -> Result<ArtifactId, ArtifactStoreError>;

    /// Load an exact artifact by canonical ArtifactId.
    fn load(&self, artifact_id: ArtifactId)
        -> Result<Option<RetainedArtifact>, ArtifactStoreError>;
}

/// Local content-addressed artifact cache.
///
/// Layout:
///
/// ```text
/// <root>/<artifact-id>/
///   identity.json   # ArtifactIdentityManifest
///   retention.json  # NBC digest + actor semantic sidecars
///   artifact.nbc    # exact frozen NBC bytes
/// ```
#[derive(Debug, Clone)]
pub struct LocalArtifactStore {
    root: PathBuf,
}

impl LocalArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn artifact_dir(&self, artifact_id: ArtifactId) -> PathBuf {
        self.root.join(artifact_id.to_string())
    }

    fn load_required(&self, artifact_id: ArtifactId) -> Result<RetainedArtifact, ArtifactStoreError> {
        self.load(artifact_id)?
            .ok_or(ArtifactStoreError::MissingArtifact { artifact_id })
    }

    fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), ArtifactStoreError> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(ArtifactStoreError::from)?;
        file.write_all(bytes).map_err(ArtifactStoreError::from)?;
        file.sync_all().map_err(ArtifactStoreError::from)
    }

    fn verify_module_for_manifest(
        manifest: &ArtifactIdentityManifest,
        module: &CodeModule,
    ) -> Result<(), ArtifactStoreError> {
        let module_semantic = module.semantic_id.ok_or(
            ArtifactStoreError::MissingModuleSemanticIdentity,
        )?;
        if module_semantic != manifest.semantic_id() {
            return Err(ArtifactStoreError::SemanticIdentityMismatch {
                manifest: manifest.semantic_id(),
                module: module_semantic,
            });
        }
        if let Some(module_artifact) = module.artifact_id {
            if module_artifact != manifest.artifact_id() {
                return Err(ArtifactStoreError::ArtifactIdentityMismatch {
                    expected: manifest.artifact_id(),
                    actual: module_artifact,
                });
            }
        }
        if module.actor_semantic_ids.len() != module.actor_metadata.len() {
            return Err(ArtifactStoreError::ActorSemanticCountMismatch {
                actors: module.actor_metadata.len(),
                semantic_ids: module.actor_semantic_ids.len(),
            });
        }
        Ok(())
    }
}

impl ArtifactStore for LocalArtifactStore {
    fn retain(
        &self,
        manifest: &ArtifactIdentityManifest,
        module: &CodeModule,
        source_hash: Option<[u8; 32]>,
    ) -> Result<ArtifactId, ArtifactStoreError> {
        Self::verify_module_for_manifest(manifest, module)?;

        let artifact_id = manifest.artifact_id();
        let nbc_bytes = module
            .to_nbc(source_hash)
            .map_err(|error| ArtifactStoreError::Bytecode(error.to_string()))?;
        let identity_json = manifest
            .to_json()
            .map_err(ArtifactStoreError::ArtifactIdentity)?;
        let metadata = RetainedArtifactMetadataV1 {
            version: RETAINED_ARTIFACT_METADATA_VERSION,
            artifact_id: artifact_id.to_string(),
            nbc_blake3: blake3::hash(&nbc_bytes).to_hex().to_string(),
            actor_semantic_ids: module
                .actor_semantic_ids
                .iter()
                .map(ToString::to_string)
                .collect(),
        };
        let metadata_json = serde_json::to_vec(&metadata)
            .map_err(|error| ArtifactStoreError::Metadata(error.to_string()))?;

        fs::create_dir_all(&self.root).map_err(ArtifactStoreError::from)?;
        let target = self.artifact_dir(artifact_id);
        if target.exists() {
            let existing = self.load_required(artifact_id)?;
            if existing.nbc_bytes == nbc_bytes
                && existing.manifest == *manifest
                && existing.module.actor_semantic_ids == module.actor_semantic_ids
            {
                return Ok(artifact_id);
            }
            return Err(ArtifactStoreError::ArtifactCollision { artifact_id });
        }

        let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = self.root.join(format!(
            ".{}.{}.{}.tmp",
            artifact_id,
            std::process::id(),
            seq
        ));
        if temp.exists() {
            fs::remove_dir_all(&temp).map_err(ArtifactStoreError::from)?;
        }
        fs::create_dir(&temp).map_err(ArtifactStoreError::from)?;

        let write_result = (|| {
            Self::write_new_file(&temp.join(IDENTITY_FILE), &identity_json)?;
            Self::write_new_file(&temp.join(METADATA_FILE), &metadata_json)?;
            Self::write_new_file(&temp.join(NBC_FILE), &nbc_bytes)?;
            fs::rename(&temp, &target).map_err(ArtifactStoreError::from)?;
            Ok::<(), ArtifactStoreError>(())
        })();

        if let Err(error) = write_result {
            let _ = fs::remove_dir_all(&temp);
            // A concurrent writer may have won the race. Accept only if it
            // retained byte-for-byte equivalent content.
            if target.exists() {
                let existing = self.load_required(artifact_id)?;
                if existing.nbc_bytes == nbc_bytes
                    && existing.manifest == *manifest
                    && existing.module.actor_semantic_ids == module.actor_semantic_ids
                {
                    return Ok(artifact_id);
                }
                return Err(ArtifactStoreError::ArtifactCollision { artifact_id });
            }
            return Err(error);
        }

        Ok(artifact_id)
    }

    fn load(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<Option<RetainedArtifact>, ArtifactStoreError> {
        let dir = self.artifact_dir(artifact_id);
        if !dir.exists() {
            return Ok(None);
        }
        if !dir.is_dir() {
            return Err(ArtifactStoreError::InvalidLayout {
                artifact_id,
                message: "artifact path is not a directory".to_string(),
            });
        }

        let identity_json = fs::read(dir.join(IDENTITY_FILE)).map_err(ArtifactStoreError::from)?;
        let manifest = ArtifactIdentityManifest::from_json(&identity_json)
            .map_err(ArtifactStoreError::ArtifactIdentity)?;
        if manifest.artifact_id() != artifact_id {
            return Err(ArtifactStoreError::ArtifactIdentityMismatch {
                expected: artifact_id,
                actual: manifest.artifact_id(),
            });
        }

        let metadata_json = fs::read(dir.join(METADATA_FILE)).map_err(ArtifactStoreError::from)?;
        let metadata: RetainedArtifactMetadataV1 = serde_json::from_slice(&metadata_json)
            .map_err(|error| ArtifactStoreError::Metadata(error.to_string()))?;
        if metadata.version != RETAINED_ARTIFACT_METADATA_VERSION {
            return Err(ArtifactStoreError::UnsupportedMetadataVersion {
                actual: metadata.version,
            });
        }
        let metadata_artifact_id = ArtifactId::from_str(&metadata.artifact_id)
            .map_err(|error| ArtifactStoreError::InvalidIdentity {
                field: "artifact_id",
                message: error.to_string(),
            })?;
        if metadata_artifact_id != artifact_id {
            return Err(ArtifactStoreError::ArtifactIdentityMismatch {
                expected: artifact_id,
                actual: metadata_artifact_id,
            });
        }

        let nbc_bytes = fs::read(dir.join(NBC_FILE)).map_err(ArtifactStoreError::from)?;
        let actual_digest = blake3::hash(&nbc_bytes).to_hex().to_string();
        if actual_digest != metadata.nbc_blake3 {
            return Err(ArtifactStoreError::ByteIntegrityMismatch {
                artifact_id,
                expected: metadata.nbc_blake3,
                actual: actual_digest,
            });
        }

        let artifact = CodeModule::from_nbc(&nbc_bytes)
            .map_err(|error| ArtifactStoreError::Bytecode(error.to_string()))?;
        let mut module = artifact.module;

        let mut actor_semantic_ids = Vec::with_capacity(metadata.actor_semantic_ids.len());
        for value in metadata.actor_semantic_ids {
            let semantic_id = SemanticId::from_str(&value).map_err(|error| {
                ArtifactStoreError::InvalidIdentity {
                    field: "actor_semantic_ids",
                    message: error.to_string(),
                }
            })?;
            actor_semantic_ids.push(semantic_id);
        }
        if actor_semantic_ids.len() != module.actor_metadata.len() {
            return Err(ArtifactStoreError::ActorSemanticCountMismatch {
                actors: module.actor_metadata.len(),
                semantic_ids: actor_semantic_ids.len(),
            });
        }

        module.semantic_id = Some(manifest.semantic_id());
        module.artifact_id = Some(artifact_id);
        module.actor_semantic_ids = actor_semantic_ids;

        Ok(Some(RetainedArtifact {
            manifest,
            module,
            nbc_bytes,
        }))
    }
}

#[derive(Debug)]
pub enum ArtifactStoreError {
    Io(String),
    ArtifactIdentity(ArtifactIdentityError),
    Metadata(String),
    Bytecode(String),
    MissingArtifact {
        artifact_id: ArtifactId,
    },
    MissingModuleSemanticIdentity,
    SemanticIdentityMismatch {
        manifest: SemanticId,
        module: SemanticId,
    },
    ArtifactIdentityMismatch {
        expected: ArtifactId,
        actual: ArtifactId,
    },
    InvalidIdentity {
        field: &'static str,
        message: String,
    },
    UnsupportedMetadataVersion {
        actual: u16,
    },
    ActorSemanticCountMismatch {
        actors: usize,
        semantic_ids: usize,
    },
    ByteIntegrityMismatch {
        artifact_id: ArtifactId,
        expected: String,
        actual: String,
    },
    ArtifactCollision {
        artifact_id: ArtifactId,
    },
    InvalidLayout {
        artifact_id: ArtifactId,
        message: String,
    },
}

impl fmt::Display for ArtifactStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "artifact store I/O error: {message}"),
            Self::ArtifactIdentity(error) => error.fmt(f),
            Self::Metadata(message) => write!(f, "invalid artifact retention metadata: {message}"),
            Self::Bytecode(message) => write!(f, "invalid retained NBC artifact: {message}"),
            Self::MissingArtifact { artifact_id } => {
                write!(f, "historical artifact {artifact_id} is not retained")
            }
            Self::MissingModuleSemanticIdentity => {
                write!(f, "cannot retain module without compiler-proven semantic identity")
            }
            Self::SemanticIdentityMismatch { manifest, module } => write!(
                f,
                "artifact manifest semantic identity {manifest} does not match module semantic identity {module}"
            ),
            Self::ArtifactIdentityMismatch { expected, actual } => write!(
                f,
                "artifact identity mismatch: expected {expected}, got {actual}"
            ),
            Self::InvalidIdentity { field, message } => {
                write!(f, "invalid {field} in artifact retention metadata: {message}")
            }
            Self::UnsupportedMetadataVersion { actual } => write!(
                f,
                "unsupported artifact retention metadata version {actual}; runtime supports {RETAINED_ARTIFACT_METADATA_VERSION}"
            ),
            Self::ActorSemanticCountMismatch {
                actors,
                semantic_ids,
            } => write!(
                f,
                "retained actor semantic sidecar count {semantic_ids} does not match actor metadata count {actors}"
            ),
            Self::ByteIntegrityMismatch {
                artifact_id,
                expected,
                actual,
            } => write!(
                f,
                "retained artifact {artifact_id} failed byte integrity check: expected {expected}, got {actual}"
            ),
            Self::ArtifactCollision { artifact_id } => write!(
                f,
                "artifact store already contains different content for {artifact_id}"
            ),
            Self::InvalidLayout {
                artifact_id,
                message,
            } => write!(f, "invalid retained artifact {artifact_id}: {message}"),
        }
    }
}

impl std::error::Error for ArtifactStoreError {}

impl From<io::Error> for ArtifactStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RetainedArtifactMetadataV1 {
    version: u16,
    artifact_id: String,
    nbc_blake3: String,
    #[serde(default)]
    actor_semantic_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_identity::ArtifactIdentityManifest;
    use crate::bytecode::{ActorMeta, Instruction, OpCode};
    use crate::content_identity::{SemanticId, SourceId};

    fn temp_root(name: &str) -> PathBuf {
        let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "nulang-artifact-store-{name}-{}-{seq}",
            std::process::id()
        ))
    }

    fn fixture() -> (CodeModule, ArtifactIdentityManifest) {
        let program_semantic = SemanticId::from_canonical_bytes(b"program", []);
        let actor_semantic = SemanticId::from_canonical_bytes(b"Counter", []);
        let mut module = CodeModule::new("artifact-test");
        module.semantic_id = Some(program_semantic);
        module.actor_semantic_ids.push(actor_semantic);
        module.actor_metadata.push(ActorMeta::new("Counter"));
        module.emit(Instruction::new0(OpCode::Halt));

        let manifest = ArtifactIdentityManifest::new(
            Some(SourceId::from_bytes(b"actor Counter {}")),
            program_semantic,
            "nulangc-test",
            "nulang-vm-v1",
            "nbc-v1",
            "bytecode",
            ["opt=0"],
        );
        module.artifact_id = Some(manifest.artifact_id());
        (module, manifest)
    }

    #[test]
    fn local_store_round_trips_frozen_nbc_with_identity_sidecars() {
        let root = temp_root("roundtrip");
        let _ = fs::remove_dir_all(&root);
        let store = LocalArtifactStore::new(&root);
        let (module, manifest) = fixture();

        let artifact_id = store.retain(&manifest, &module, None).unwrap();
        let restored = store.load(artifact_id).unwrap().unwrap();

        assert_eq!(restored.manifest, manifest);
        assert_eq!(restored.module.semantic_id, module.semantic_id);
        assert_eq!(restored.module.artifact_id, Some(artifact_id));
        assert_eq!(restored.module.actor_semantic_ids, module.actor_semantic_ids);
        assert_eq!(restored.module.actor_metadata, module.actor_metadata);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn identical_retention_is_idempotent() {
        let root = temp_root("idempotent");
        let _ = fs::remove_dir_all(&root);
        let store = LocalArtifactStore::new(&root);
        let (module, manifest) = fixture();

        let first = store.retain(&manifest, &module, None).unwrap();
        let second = store.retain(&manifest, &module, None).unwrap();
        assert_eq!(first, second);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_artifact_is_distinct_from_corruption() {
        let root = temp_root("missing");
        let _ = fs::remove_dir_all(&root);
        let store = LocalArtifactStore::new(&root);
        let missing = ArtifactId::from_semantic(
            SemanticId::from_canonical_bytes(b"missing", []),
            "compiler",
            "target",
            "abi",
            "backend",
            std::iter::empty::<&str>(),
        );
        assert!(store.load(missing).unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tampered_nbc_bytes_fail_integrity_check() {
        let root = temp_root("tamper");
        let _ = fs::remove_dir_all(&root);
        let store = LocalArtifactStore::new(&root);
        let (module, manifest) = fixture();
        let artifact_id = store.retain(&manifest, &module, None).unwrap();

        fs::write(store.artifact_dir(artifact_id).join(NBC_FILE), b"tampered").unwrap();
        assert!(matches!(
            store.load(artifact_id),
            Err(ArtifactStoreError::ByteIntegrityMismatch { .. })
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn module_manifest_semantic_mismatch_fails_closed() {
        let root = temp_root("semantic-mismatch");
        let _ = fs::remove_dir_all(&root);
        let store = LocalArtifactStore::new(&root);
        let (mut module, manifest) = fixture();
        module.semantic_id = Some(SemanticId::from_canonical_bytes(b"different", []));

        assert!(matches!(
            store.retain(&manifest, &module, None),
            Err(ArtifactStoreError::SemanticIdentityMismatch { .. })
        ));
        let _ = fs::remove_dir_all(root);
    }
}
