//! Durable, content-verified retention for compiled Nulang artifacts.
//!
//! `ArtifactId` identifies canonical code-generation inputs; it is deliberately
//! distinct from a raw hash of emitted bytes. Historical execution therefore
//! needs both identities:
//!
//! - `ArtifactId`: which semantic/codegen artifact was requested;
//! - BLAKE3(bytes): proof that the retained immutable bytes did not change.
//!
//! This store keeps the frozen artifact bytes together with the versioned
//! `ArtifactIdentityManifest` in one atomic record. Existing records are never
//! overwritten. If the same `ArtifactId` is presented with different bytes,
//! retention fails closed as a nondeterministic/corrupt artifact collision.

use crate::artifact_identity::{ArtifactIdentityError, ArtifactIdentityManifest};
use crate::content_identity::ArtifactId;
use crate::runtime_artifact_manifest::{
    RuntimeArtifactManifest, RuntimeArtifactManifestError,
};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const RECORD_MAGIC: &[u8; 4] = b"NART";
const RECORD_VERSION: u16 = 1;
const RECORD_HEADER_LEN: usize = 4 + 2 + 4 + 8 + 32;
const RUNTIME_MANIFEST_MAGIC: &[u8; 4] = b"NARM";
const RUNTIME_MANIFEST_RECORD_VERSION: u16 = 1;
const RUNTIME_MANIFEST_HEADER_LEN: usize = 4 + 2 + 4 + 32;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// One verified historical artifact loaded from durable retention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedArtifact {
    pub manifest: ArtifactIdentityManifest,
    pub bytes: Vec<u8>,
    pub bytes_blake3: [u8; 32],
}

/// One fully identified historical runtime artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedRuntimeArtifact {
    pub artifact_manifest: ArtifactIdentityManifest,
    pub runtime_manifest: RuntimeArtifactManifest,
    pub bytes: Vec<u8>,
    pub bytes_blake3: [u8; 32],
}

/// Result of retaining an artifact plus its additive runtime sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeRetainOutcome {
    pub artifact: RetainOutcome,
    pub runtime_manifest: RetainOutcome,
}

/// Result of retaining an immutable artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainOutcome {
    Stored,
    AlreadyPresent,
}

/// Filesystem-backed immutable artifact retention.
///
/// Records are sharded by the first two hex characters of the `ArtifactId`:
///
/// `<root>/<shard>/<artifact-id>.nart`
///
/// A temporary record is fully written and fsynced before an atomic hard-link
/// publishes the final immutable path. Hard-link publication is intentionally
/// no-clobber: concurrent writers can never replace an already retained
/// artifact.
#[derive(Debug, Clone)]
pub struct FileArtifactStore {
    root: PathBuf,
}

impl FileArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Retain one compiled artifact immutably under its manifest's
    /// `ArtifactId`.
    ///
    /// Exact repeated retention is idempotent. Presenting different bytes for
    /// an existing `ArtifactId` fails closed and leaves the original record
    /// untouched.
    pub fn retain(
        &self,
        manifest: &ArtifactIdentityManifest,
        bytes: &[u8],
    ) -> Result<RetainOutcome, ArtifactStoreError> {
        let artifact_id = manifest.artifact_id();

        match self.load(artifact_id) {
            Ok(existing) => {
                return if existing.bytes == bytes {
                    Ok(RetainOutcome::AlreadyPresent)
                } else {
                    Err(ArtifactStoreError::ArtifactCollision { artifact_id })
                };
            }
            Err(ArtifactStoreError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }

        let record = encode_record(manifest, bytes)?;
        let final_path = self.artifact_path(artifact_id);
        let parent = final_path
            .parent()
            .ok_or_else(|| ArtifactStoreError::Corrupt("artifact path has no parent".into()))?;
        fs::create_dir_all(parent)?;

        let temp_id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{}.{}.{}.tmp",
            artifact_id,
            std::process::id(),
            temp_id
        ));

        let write_result = (|| -> Result<(), ArtifactStoreError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(&record)?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }

        match fs::hard_link(&temp_path, &final_path) {
            Ok(()) => {
                fs::remove_file(&temp_path)?;
                sync_directory(parent)?;
                Ok(RetainOutcome::Stored)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temp_path);
                let existing = self.load(artifact_id)?;
                if existing.bytes == bytes {
                    Ok(RetainOutcome::AlreadyPresent)
                } else {
                    Err(ArtifactStoreError::ArtifactCollision { artifact_id })
                }
            }
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                Err(error.into())
            }
        }
    }

    /// Load and verify a retained artifact by immutable `ArtifactId`.
    pub fn load(&self, artifact_id: ArtifactId) -> Result<RetainedArtifact, ArtifactStoreError> {
        let path = self.artifact_path(artifact_id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ArtifactStoreError::NotFound(artifact_id));
            }
            Err(error) => return Err(error.into()),
        };
        decode_record(&bytes, artifact_id)
    }

    /// True only when the retained record exists and passes all identity and
    /// byte-integrity verification.
    pub fn contains_verified(&self, artifact_id: ArtifactId) -> Result<bool, ArtifactStoreError> {
        match self.load(artifact_id) {
            Ok(_) => Ok(true),
            Err(ArtifactStoreError::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Retain executable bytes plus the immutable runtime identity sidecar.
    ///
    /// The executable record is published first. If sidecar publication fails,
    /// the partial state is safe and retryable: runtime loading refuses an
    /// artifact that lacks the sidecar rather than guessing identities.
    pub fn retain_runtime(
        &self,
        artifact_manifest: &ArtifactIdentityManifest,
        runtime_manifest: &RuntimeArtifactManifest,
        bytes: &[u8],
    ) -> Result<RuntimeRetainOutcome, ArtifactStoreError> {
        if artifact_manifest.artifact_id() != runtime_manifest.artifact_id() {
            return Err(ArtifactStoreError::RuntimeManifestArtifactMismatch {
                artifact: artifact_manifest.artifact_id(),
                runtime_manifest: runtime_manifest.artifact_id(),
            });
        }

        let artifact = self.retain(artifact_manifest, bytes)?;
        let runtime_manifest = self.retain_runtime_manifest(runtime_manifest)?;
        Ok(RuntimeRetainOutcome {
            artifact,
            runtime_manifest,
        })
    }

    /// Retain only the additive runtime manifest for an already verified
    /// executable artifact.
    pub fn retain_runtime_manifest(
        &self,
        manifest: &RuntimeArtifactManifest,
    ) -> Result<RetainOutcome, ArtifactStoreError> {
        let artifact_id = manifest.artifact_id();
        self.load(artifact_id)?;

        match self.load_runtime_manifest(artifact_id) {
            Ok(existing) => {
                return if existing == *manifest {
                    Ok(RetainOutcome::AlreadyPresent)
                } else {
                    Err(ArtifactStoreError::RuntimeManifestCollision { artifact_id })
                };
            }
            Err(ArtifactStoreError::RuntimeManifestNotFound(_)) => {}
            Err(error) => return Err(error),
        }

        let record = encode_runtime_manifest_record(manifest)?;
        let final_path = self.runtime_manifest_path(artifact_id);
        let parent = final_path.parent().ok_or_else(|| {
            ArtifactStoreError::Corrupt("runtime manifest path has no parent".into())
        })?;
        fs::create_dir_all(parent)?;

        let temp_id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{}.runtime.{}.{}.tmp",
            artifact_id,
            std::process::id(),
            temp_id
        ));

        let write_result = (|| -> Result<(), ArtifactStoreError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(&record)?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }

        match fs::hard_link(&temp_path, &final_path) {
            Ok(()) => {
                fs::remove_file(&temp_path)?;
                sync_directory(parent)?;
                Ok(RetainOutcome::Stored)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temp_path);
                let existing = self.load_runtime_manifest(artifact_id)?;
                if existing == *manifest {
                    Ok(RetainOutcome::AlreadyPresent)
                } else {
                    Err(ArtifactStoreError::RuntimeManifestCollision { artifact_id })
                }
            }
            Err(error) => {
                let _ = fs::remove_file(&temp_path);
                Err(error.into())
            }
        }
    }

    /// Load and verify the runtime sidecar for an already retained artifact.
    pub fn load_runtime_manifest(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<RuntimeArtifactManifest, ArtifactStoreError> {
        self.load(artifact_id)?;
        let path = self.runtime_manifest_path(artifact_id);
        let record = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ArtifactStoreError::RuntimeManifestNotFound(artifact_id));
            }
            Err(error) => return Err(error.into()),
        };
        decode_runtime_manifest_record(&record, artifact_id)
    }

    /// Load exact executable bytes plus all semantic sidecars required by the
    /// runtime to resume historical durable state.
    pub fn load_runtime_artifact(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<RetainedRuntimeArtifact, ArtifactStoreError> {
        let artifact = self.load(artifact_id)?;
        let runtime_manifest = self.load_runtime_manifest(artifact_id)?;
        Ok(RetainedRuntimeArtifact {
            artifact_manifest: artifact.manifest,
            runtime_manifest,
            bytes: artifact.bytes,
            bytes_blake3: artifact.bytes_blake3,
        })
    }

    /// Decode frozen NBC bytes and restore the compiler identity sidecars from
    /// the independently versioned runtime manifest.
    pub fn load_identified_module(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<crate::bytecode::CodeModule, ArtifactStoreError> {
        let retained = self.load_runtime_artifact(artifact_id)?;
        let mut module = crate::bytecode::CodeModule::from_nbc(&retained.bytes)
            .map_err(|error| ArtifactStoreError::BytecodeFormat(error.to_string()))?
            .module;
        retained.runtime_manifest.bind_module(&mut module)?;
        Ok(module)
    }

    fn artifact_path(&self, artifact_id: ArtifactId) -> PathBuf {
        let hex = artifact_id.to_hex();
        self.root.join(&hex[..2]).join(format!("{hex}.nart"))
    }

    fn runtime_manifest_path(&self, artifact_id: ArtifactId) -> PathBuf {
        let hex = artifact_id.to_hex();
        self.root
            .join(&hex[..2])
            .join(format!("{hex}.runtime.narm"))
    }
}

fn encode_record(
    manifest: &ArtifactIdentityManifest,
    artifact_bytes: &[u8],
) -> Result<Vec<u8>, ArtifactStoreError> {
    let manifest_json = manifest.to_json()?;
    let manifest_len = u32::try_from(manifest_json.len())
        .map_err(|_| ArtifactStoreError::Corrupt("artifact manifest is too large".into()))?;
    let artifact_len = u64::try_from(artifact_bytes.len())
        .map_err(|_| ArtifactStoreError::Corrupt("artifact is too large".into()))?;
    let digest = blake3::hash(artifact_bytes);

    let capacity = RECORD_HEADER_LEN
        .checked_add(manifest_json.len())
        .and_then(|size| size.checked_add(artifact_bytes.len()))
        .ok_or_else(|| ArtifactStoreError::Corrupt("artifact record length overflow".into()))?;
    let mut record = Vec::with_capacity(capacity);
    record.extend_from_slice(RECORD_MAGIC);
    record.extend_from_slice(&RECORD_VERSION.to_le_bytes());
    record.extend_from_slice(&manifest_len.to_le_bytes());
    record.extend_from_slice(&artifact_len.to_le_bytes());
    record.extend_from_slice(digest.as_bytes());
    record.extend_from_slice(&manifest_json);
    record.extend_from_slice(artifact_bytes);
    Ok(record)
}

fn decode_record(
    record: &[u8],
    requested_id: ArtifactId,
) -> Result<RetainedArtifact, ArtifactStoreError> {
    if record.len() < RECORD_HEADER_LEN {
        return Err(ArtifactStoreError::Corrupt(
            "artifact record is truncated".into(),
        ));
    }
    if &record[..4] != RECORD_MAGIC {
        return Err(ArtifactStoreError::Corrupt(
            "artifact record has invalid magic".into(),
        ));
    }

    let version = u16::from_le_bytes([record[4], record[5]]);
    if version != RECORD_VERSION {
        return Err(ArtifactStoreError::UnsupportedVersion(version));
    }

    let manifest_len = u32::from_le_bytes(record[6..10].try_into().expect("fixed header")) as usize;
    let artifact_len_u64 = u64::from_le_bytes(record[10..18].try_into().expect("fixed header"));
    let artifact_len = usize::try_from(artifact_len_u64)
        .map_err(|_| ArtifactStoreError::Corrupt("artifact length exceeds platform size".into()))?;

    let manifest_start = RECORD_HEADER_LEN;
    let manifest_end = manifest_start
        .checked_add(manifest_len)
        .ok_or_else(|| ArtifactStoreError::Corrupt("manifest length overflow".into()))?;
    let artifact_end = manifest_end
        .checked_add(artifact_len)
        .ok_or_else(|| ArtifactStoreError::Corrupt("artifact length overflow".into()))?;
    if artifact_end != record.len() {
        return Err(ArtifactStoreError::Corrupt(
            "artifact record length does not match header".into(),
        ));
    }

    let mut recorded_digest = [0u8; 32];
    recorded_digest.copy_from_slice(&record[18..50]);
    let manifest = ArtifactIdentityManifest::from_json(&record[manifest_start..manifest_end])?;
    if manifest.artifact_id() != requested_id {
        return Err(ArtifactStoreError::ArtifactIdMismatch {
            requested: requested_id,
            recorded: manifest.artifact_id(),
        });
    }

    let artifact_bytes = &record[manifest_end..artifact_end];
    let actual_digest = *blake3::hash(artifact_bytes).as_bytes();
    if actual_digest != recorded_digest {
        return Err(ArtifactStoreError::ByteDigestMismatch {
            artifact_id: requested_id,
        });
    }

    Ok(RetainedArtifact {
        manifest,
        bytes: artifact_bytes.to_vec(),
        bytes_blake3: recorded_digest,
    })
}

fn encode_runtime_manifest_record(
    manifest: &RuntimeArtifactManifest,
) -> Result<Vec<u8>, ArtifactStoreError> {
    let json = manifest.to_json()?;
    let manifest_len = u32::try_from(json.len())
        .map_err(|_| ArtifactStoreError::Corrupt("runtime manifest is too large".into()))?;
    let digest = blake3::hash(&json);

    let capacity = RUNTIME_MANIFEST_HEADER_LEN
        .checked_add(json.len())
        .ok_or_else(|| ArtifactStoreError::Corrupt("runtime manifest length overflow".into()))?;
    let mut record = Vec::with_capacity(capacity);
    record.extend_from_slice(RUNTIME_MANIFEST_MAGIC);
    record.extend_from_slice(&RUNTIME_MANIFEST_RECORD_VERSION.to_le_bytes());
    record.extend_from_slice(&manifest_len.to_le_bytes());
    record.extend_from_slice(digest.as_bytes());
    record.extend_from_slice(&json);
    Ok(record)
}

fn decode_runtime_manifest_record(
    record: &[u8],
    requested_id: ArtifactId,
) -> Result<RuntimeArtifactManifest, ArtifactStoreError> {
    if record.len() < RUNTIME_MANIFEST_HEADER_LEN {
        return Err(ArtifactStoreError::Corrupt(
            "runtime manifest record is truncated".into(),
        ));
    }
    if &record[..4] != RUNTIME_MANIFEST_MAGIC {
        return Err(ArtifactStoreError::Corrupt(
            "runtime manifest record has invalid magic".into(),
        ));
    }

    let version = u16::from_le_bytes([record[4], record[5]]);
    if version != RUNTIME_MANIFEST_RECORD_VERSION {
        return Err(ArtifactStoreError::UnsupportedRuntimeManifestRecordVersion(
            version,
        ));
    }

    let manifest_len = u32::from_le_bytes(record[6..10].try_into().expect("fixed header")) as usize;
    let manifest_end = RUNTIME_MANIFEST_HEADER_LEN
        .checked_add(manifest_len)
        .ok_or_else(|| ArtifactStoreError::Corrupt("runtime manifest length overflow".into()))?;
    if manifest_end != record.len() {
        return Err(ArtifactStoreError::Corrupt(
            "runtime manifest record length does not match header".into(),
        ));
    }

    let mut recorded_digest = [0u8; 32];
    recorded_digest.copy_from_slice(&record[10..42]);
    let json = &record[RUNTIME_MANIFEST_HEADER_LEN..manifest_end];
    let actual_digest = *blake3::hash(json).as_bytes();
    if actual_digest != recorded_digest {
        return Err(ArtifactStoreError::RuntimeManifestDigestMismatch {
            artifact_id: requested_id,
        });
    }

    let manifest = RuntimeArtifactManifest::from_json(json)?;
    if manifest.artifact_id() != requested_id {
        return Err(ArtifactStoreError::RuntimeManifestArtifactMismatch {
            artifact: requested_id,
            runtime_manifest: manifest.artifact_id(),
        });
    }
    Ok(manifest)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ArtifactStoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ArtifactStoreError> {
    Ok(())
}

#[derive(Debug)]
pub enum ArtifactStoreError {
    NotFound(ArtifactId),
    RuntimeManifestNotFound(ArtifactId),
    Io(std::io::Error),
    Manifest(ArtifactIdentityError),
    RuntimeManifest(RuntimeArtifactManifestError),
    BytecodeFormat(String),
    UnsupportedVersion(u16),
    UnsupportedRuntimeManifestRecordVersion(u16),
    ArtifactIdMismatch {
        requested: ArtifactId,
        recorded: ArtifactId,
    },
    ByteDigestMismatch {
        artifact_id: ArtifactId,
    },
    ArtifactCollision {
        artifact_id: ArtifactId,
    },
    RuntimeManifestCollision {
        artifact_id: ArtifactId,
    },
    RuntimeManifestDigestMismatch {
        artifact_id: ArtifactId,
    },
    RuntimeManifestArtifactMismatch {
        artifact: ArtifactId,
        runtime_manifest: ArtifactId,
    },
    Corrupt(String),
}

impl fmt::Display for ArtifactStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "artifact {id} is not retained"),
            Self::RuntimeManifestNotFound(id) => {
                write!(f, "runtime manifest for artifact {id} is not retained")
            }
            Self::Io(error) => write!(f, "artifact store I/O error: {error}"),
            Self::Manifest(error) => write!(f, "invalid retained artifact manifest: {error}"),
            Self::RuntimeManifest(error) => write!(f, "invalid runtime artifact manifest: {error}"),
            Self::BytecodeFormat(error) => write!(f, "invalid retained NBC artifact: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported retained artifact record version {version}")
            }
            Self::UnsupportedRuntimeManifestRecordVersion(version) => write!(
                f,
                "unsupported retained runtime manifest record version {version}"
            ),
            Self::ArtifactIdMismatch {
                requested,
                recorded,
            } => write!(
                f,
                "retained artifact identity mismatch: requested {requested}, record contains {recorded}"
            ),
            Self::ByteDigestMismatch { artifact_id } => write!(
                f,
                "retained artifact {artifact_id} failed BLAKE3 byte verification"
            ),
            Self::ArtifactCollision { artifact_id } => write!(
                f,
                "artifact {artifact_id} already exists with different bytes"
            ),
            Self::RuntimeManifestCollision { artifact_id } => write!(
                f,
                "runtime manifest for artifact {artifact_id} already exists with different provenance"
            ),
            Self::RuntimeManifestDigestMismatch { artifact_id } => write!(
                f,
                "runtime manifest for artifact {artifact_id} failed BLAKE3 verification"
            ),
            Self::RuntimeManifestArtifactMismatch {
                artifact,
                runtime_manifest,
            } => write!(
                f,
                "runtime manifest ArtifactId {runtime_manifest} does not match retained artifact {artifact}"
            ),
            Self::Corrupt(message) => write!(f, "corrupt retained artifact: {message}"),
        }
    }
}

impl std::error::Error for ArtifactStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Manifest(error) => Some(error),
            Self::RuntimeManifest(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ArtifactStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ArtifactIdentityError> for ArtifactStoreError {
    fn from(error: ArtifactIdentityError) -> Self {
        Self::Manifest(error)
    }
}

impl From<RuntimeArtifactManifestError> for ArtifactStoreError {
    fn from(error: RuntimeArtifactManifestError) -> Self {
        Self::RuntimeManifest(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{ActorMeta, CodeModule};
    use crate::content_identity::{SemanticId, SourceId};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(1);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "nulang-artifact-store-{}-{}",
                std::process::id(),
                id
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn manifest(source: &[u8]) -> ArtifactIdentityManifest {
        ArtifactIdentityManifest::new(
            Some(SourceId::from_bytes(source)),
            SemanticId::from_canonical_bytes(b"stable-semantics", []),
            "nulangc-test",
            "portable",
            "nulang-abi-v1",
            "bytecode",
            ["opt=0"],
        )
    }

    fn runtime_module(
        identity: &ArtifactIdentityManifest,
        definition: SemanticId,
    ) -> CodeModule {
        let mut module = CodeModule::new("retained-runtime");
        module.semantic_id = Some(identity.semantic_id());
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
        module
    }

    #[test]
    fn retain_and_load_exact_artifact() {
        let dir = TestDir::new();
        let store = FileArtifactStore::new(&dir.0);
        let manifest = manifest(b"source-a");
        let bytes = b"frozen-nbc-bytes";

        assert_eq!(
            store.retain(&manifest, bytes).unwrap(),
            RetainOutcome::Stored
        );

        let loaded = store.load(manifest.artifact_id()).unwrap();
        assert_eq!(loaded.manifest, manifest);
        assert_eq!(loaded.bytes, bytes);
        assert_eq!(loaded.bytes_blake3, *blake3::hash(bytes).as_bytes());
        assert!(store.contains_verified(manifest.artifact_id()).unwrap());
    }

    #[test]
    fn repeated_same_artifact_is_idempotent_even_when_source_id_differs() {
        let dir = TestDir::new();
        let store = FileArtifactStore::new(&dir.0);
        let first = manifest(b"formatting-a");
        let second = manifest(b"formatting-b");
        assert_eq!(first.artifact_id(), second.artifact_id());
        let bytes = b"same-emitted-artifact";

        assert_eq!(store.retain(&first, bytes).unwrap(), RetainOutcome::Stored);
        assert_eq!(
            store.retain(&second, bytes).unwrap(),
            RetainOutcome::AlreadyPresent
        );
    }

    #[test]
    fn different_bytes_under_same_artifact_id_fail_closed() {
        let dir = TestDir::new();
        let store = FileArtifactStore::new(&dir.0);
        let manifest = manifest(b"source");

        store.retain(&manifest, b"artifact-a").unwrap();
        let error = store.retain(&manifest, b"artifact-b").unwrap_err();
        assert!(matches!(
            error,
            ArtifactStoreError::ArtifactCollision { artifact_id }
                if artifact_id == manifest.artifact_id()
        ));
        assert_eq!(
            store.load(manifest.artifact_id()).unwrap().bytes,
            b"artifact-a"
        );
    }

    #[test]
    fn tampered_retained_bytes_fail_verification() {
        let dir = TestDir::new();
        let store = FileArtifactStore::new(&dir.0);
        let manifest = manifest(b"source");
        store.retain(&manifest, b"artifact").unwrap();

        let path = store.artifact_path(manifest.artifact_id());
        let mut record = fs::read(&path).unwrap();
        let last = record.last_mut().unwrap();
        *last ^= 0x01;
        fs::write(path, record).unwrap();

        assert!(matches!(
            store.load(manifest.artifact_id()),
            Err(ArtifactStoreError::ByteDigestMismatch { artifact_id })
                if artifact_id == manifest.artifact_id()
        ));
    }

    #[test]
    fn missing_artifact_is_explicit() {
        let dir = TestDir::new();
        let store = FileArtifactStore::new(&dir.0);
        let id = manifest(b"source").artifact_id();

        assert!(matches!(
            store.load(id),
            Err(ArtifactStoreError::NotFound(missing)) if missing == id
        ));
        assert!(!store.contains_verified(id).unwrap());
    }

    #[test]
    fn runtime_sidecar_rehydrates_frozen_nbc_identity() {
        let dir = TestDir::new();
        let store = FileArtifactStore::new(&dir.0);
        let identity = manifest(b"source");
        let definition = SemanticId::from_canonical_bytes(b"counter-definition", []);
        let module = runtime_module(&identity, definition);
        let runtime_manifest =
            RuntimeArtifactManifest::from_module(&module, &identity).unwrap();
        let bytes = module.to_nbc(None).unwrap();

        let outcome = store
            .retain_runtime(&identity, &runtime_manifest, &bytes)
            .unwrap();
        assert_eq!(outcome.artifact, RetainOutcome::Stored);
        assert_eq!(outcome.runtime_manifest, RetainOutcome::Stored);

        let restored = store.load_identified_module(identity.artifact_id()).unwrap();
        assert_eq!(restored.semantic_id, Some(identity.semantic_id()));
        assert_eq!(restored.artifact_id, Some(identity.artifact_id()));
        assert_eq!(restored.actor_semantic_ids, vec![definition]);
    }

    #[test]
    fn runtime_sidecar_is_idempotent_across_source_only_changes() {
        let dir = TestDir::new();
        let store = FileArtifactStore::new(&dir.0);
        let first = manifest(b"format-a");
        let reformatted = manifest(b"format-b");
        assert_eq!(first.artifact_id(), reformatted.artifact_id());

        let definition = SemanticId::from_canonical_bytes(b"counter-definition", []);
        let module = runtime_module(&first, definition);
        let runtime_manifest =
            RuntimeArtifactManifest::from_module(&module, &first).unwrap();
        let bytes = module.to_nbc(None).unwrap();

        store
            .retain_runtime(&first, &runtime_manifest, &bytes)
            .unwrap();
        let repeated = store
            .retain_runtime(&reformatted, &runtime_manifest, &bytes)
            .unwrap();
        assert_eq!(repeated.artifact, RetainOutcome::AlreadyPresent);
        assert_eq!(repeated.runtime_manifest, RetainOutcome::AlreadyPresent);
    }

    #[test]
    fn conflicting_runtime_sidecar_fails_closed() {
        let dir = TestDir::new();
        let store = FileArtifactStore::new(&dir.0);
        let identity = manifest(b"source");
        let first_definition = SemanticId::from_canonical_bytes(b"definition-a", []);
        let second_definition = SemanticId::from_canonical_bytes(b"definition-b", []);

        let first_module = runtime_module(&identity, first_definition);
        let first_manifest =
            RuntimeArtifactManifest::from_module(&first_module, &identity).unwrap();
        let bytes = first_module.to_nbc(None).unwrap();
        store
            .retain_runtime(&identity, &first_manifest, &bytes)
            .unwrap();

        let second_module = runtime_module(&identity, second_definition);
        let second_manifest =
            RuntimeArtifactManifest::from_module(&second_module, &identity).unwrap();
        assert!(matches!(
            store.retain_runtime_manifest(&second_manifest),
            Err(ArtifactStoreError::RuntimeManifestCollision { artifact_id })
                if artifact_id == identity.artifact_id()
        ));
    }

    #[test]
    fn tampered_runtime_sidecar_fails_verification() {
        let dir = TestDir::new();
        let store = FileArtifactStore::new(&dir.0);
        let identity = manifest(b"source");
        let definition = SemanticId::from_canonical_bytes(b"counter-definition", []);
        let module = runtime_module(&identity, definition);
        let runtime_manifest =
            RuntimeArtifactManifest::from_module(&module, &identity).unwrap();
        store
            .retain_runtime(&identity, &runtime_manifest, &module.to_nbc(None).unwrap())
            .unwrap();

        let path = store.runtime_manifest_path(identity.artifact_id());
        let mut record = fs::read(&path).unwrap();
        *record.last_mut().unwrap() ^= 0x01;
        fs::write(path, record).unwrap();

        assert!(matches!(
            store.load_runtime_manifest(identity.artifact_id()),
            Err(ArtifactStoreError::RuntimeManifestDigestMismatch { artifact_id })
                if artifact_id == identity.artifact_id()
        ));
    }

    #[test]
    fn runtime_loading_requires_sidecar_instead_of_guessing() {
        let dir = TestDir::new();
        let store = FileArtifactStore::new(&dir.0);
        let identity = manifest(b"source");
        let definition = SemanticId::from_canonical_bytes(b"counter-definition", []);
        let module = runtime_module(&identity, definition);
        store.retain(&identity, &module.to_nbc(None).unwrap()).unwrap();

        assert!(matches!(
            store.load_identified_module(identity.artifact_id()),
            Err(ArtifactStoreError::RuntimeManifestNotFound(artifact_id))
                if artifact_id == identity.artifact_id()
        ));
    }
}
