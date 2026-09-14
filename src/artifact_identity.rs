//! Versioned identity metadata for compiled Nulang artifacts.
//!
//! `.nbc` format v1 is frozen by RFC 0001, so semantic-closure identity must
//! not be smuggled into that header without a format migration. This module
//! provides an additive compatibility boundary that can accompany a compiled
//! artifact today and can be embedded by a future `.nbc` v2 without changing
//! the identity contract.

use crate::content_identity::{ArtifactId, SemanticId, SourceId};
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

pub const ARTIFACT_IDENTITY_MANIFEST_VERSION: u16 = 1;

/// Strong identity and code-generation provenance for one compiled artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactIdentityManifest {
    source_id: Option<SourceId>,
    semantic_id: SemanticId,
    artifact_id: ArtifactId,
    compiler_version: String,
    target: String,
    abi: String,
    backend: String,
    flags: BTreeSet<String>,
}

impl ArtifactIdentityManifest {
    /// Construct a manifest and derive its [`ArtifactId`] from canonical
    /// code-generation inputs.
    pub fn new<I, S>(
        source_id: Option<SourceId>,
        semantic_id: SemanticId,
        compiler_version: impl Into<String>,
        target: impl Into<String>,
        abi: impl Into<String>,
        backend: impl Into<String>,
        flags: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let compiler_version = compiler_version.into();
        let target = target.into();
        let abi = abi.into();
        let backend = backend.into();
        let flags: BTreeSet<String> = flags
            .into_iter()
            .map(|flag| flag.as_ref().to_string())
            .collect();
        let artifact_id = ArtifactId::from_semantic(
            semantic_id,
            &compiler_version,
            &target,
            &abi,
            &backend,
            flags.iter(),
        );

        Self {
            source_id,
            semantic_id,
            artifact_id,
            compiler_version,
            target,
            abi,
            backend,
            flags,
        }
    }

    pub fn source_id(&self) -> Option<SourceId> {
        self.source_id
    }

    pub fn semantic_id(&self) -> SemanticId {
        self.semantic_id
    }

    pub fn artifact_id(&self) -> ArtifactId {
        self.artifact_id
    }

    pub fn compiler_version(&self) -> &str {
        &self.compiler_version
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn abi(&self) -> &str {
        &self.abi
    }

    pub fn backend(&self) -> &str {
        &self.backend
    }

    pub fn flags(&self) -> impl Iterator<Item = &str> {
        self.flags.iter().map(String::as_str)
    }

    /// Serialize the stable versioned JSON representation.
    pub fn to_json(&self) -> Result<Vec<u8>, ArtifactIdentityError> {
        let persisted = PersistedArtifactIdentityV1 {
            version: ARTIFACT_IDENTITY_MANIFEST_VERSION,
            source_id: self.source_id.map(|id| id.to_string()),
            semantic_id: self.semantic_id.to_string(),
            artifact_id: self.artifact_id.to_string(),
            compiler_version: self.compiler_version.clone(),
            target: self.target.clone(),
            abi: self.abi.clone(),
            backend: self.backend.clone(),
            flags: self.flags.iter().cloned().collect(),
        };
        serde_json::to_vec(&persisted).map_err(ArtifactIdentityError::from)
    }

    /// Decode and validate a versioned artifact-identity manifest.
    ///
    /// The artifact ID is re-derived from the persisted semantic identity and
    /// code-generation inputs. Tampering with target/backend/compiler/ABI/flags
    /// therefore fails closed rather than silently changing artifact identity.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ArtifactIdentityError> {
        let persisted: PersistedArtifactIdentityV1 =
            serde_json::from_slice(bytes).map_err(ArtifactIdentityError::from)?;
        if persisted.version != ARTIFACT_IDENTITY_MANIFEST_VERSION {
            return Err(ArtifactIdentityError::UnsupportedVersion {
                actual: persisted.version,
            });
        }

        let source_id = persisted
            .source_id
            .as_deref()
            .map(|value| parse_identity::<SourceId>("source_id", value))
            .transpose()?;
        let semantic_id = parse_identity::<SemanticId>("semantic_id", &persisted.semantic_id)?;
        let actual_artifact_id =
            parse_identity::<ArtifactId>("artifact_id", &persisted.artifact_id)?;
        let flags: BTreeSet<String> = persisted.flags.into_iter().collect();
        let expected_artifact_id = ArtifactId::from_semantic(
            semantic_id,
            &persisted.compiler_version,
            &persisted.target,
            &persisted.abi,
            &persisted.backend,
            flags.iter(),
        );
        if actual_artifact_id != expected_artifact_id {
            return Err(ArtifactIdentityError::ArtifactIdentityMismatch {
                expected: expected_artifact_id,
                actual: actual_artifact_id,
            });
        }

        Ok(Self {
            source_id,
            semantic_id,
            artifact_id: actual_artifact_id,
            compiler_version: persisted.compiler_version,
            target: persisted.target,
            abi: persisted.abi,
            backend: persisted.backend,
            flags,
        })
    }
}

fn parse_identity<T>(field: &'static str, value: &str) -> Result<T, ArtifactIdentityError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|error| ArtifactIdentityError::InvalidIdentity {
            field,
            message: error.to_string(),
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactIdentityError {
    Json(String),
    UnsupportedVersion {
        actual: u16,
    },
    InvalidIdentity {
        field: &'static str,
        message: String,
    },
    ArtifactIdentityMismatch {
        expected: ArtifactId,
        actual: ArtifactId,
    },
}

impl fmt::Display for ArtifactIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid artifact identity manifest: {message}"),
            Self::UnsupportedVersion { actual } => write!(
                f,
                "unsupported artifact identity manifest version {actual}; runtime supports version {ARTIFACT_IDENTITY_MANIFEST_VERSION}"
            ),
            Self::InvalidIdentity { field, message } => {
                write!(f, "invalid {field} in artifact identity manifest: {message}")
            }
            Self::ArtifactIdentityMismatch { expected, actual } => write!(
                f,
                "artifact identity mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for ArtifactIdentityError {}

impl From<serde_json::Error> for ArtifactIdentityError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PersistedArtifactIdentityV1 {
    version: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_id: Option<String>,
    semantic_id: String,
    artifact_id: String,
    compiler_version: String,
    target: String,
    abi: String,
    backend: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    flags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> ArtifactIdentityManifest {
        let source_id = SourceId::from_bytes(b"fn main() { 42 }");
        let semantic_id = SemanticId::from_canonical_bytes(b"main => Int(42)", []);
        ArtifactIdentityManifest::new(
            Some(source_id),
            semantic_id,
            "nulangc-0.1.0",
            "x86_64-unknown-linux-gnu",
            "nulang-abi-v1",
            "native",
            ["opt=3", "lto=thin"],
        )
    }

    #[test]
    fn manifest_round_trips_with_strong_ids() {
        let manifest = manifest();
        let bytes = manifest.to_json().unwrap();
        let restored = ArtifactIdentityManifest::from_json(&bytes).unwrap();
        assert_eq!(restored, manifest);
    }

    #[test]
    fn flag_order_and_duplicates_do_not_change_artifact_identity() {
        let semantic_id = SemanticId::from_canonical_bytes(b"main => Int(42)", []);
        let first = ArtifactIdentityManifest::new(
            None,
            semantic_id,
            "nulangc-0.1.0",
            "x86_64-unknown-linux-gnu",
            "nulang-abi-v1",
            "native",
            ["opt=3", "lto=thin"],
        );
        let reordered = ArtifactIdentityManifest::new(
            None,
            semantic_id,
            "nulangc-0.1.0",
            "x86_64-unknown-linux-gnu",
            "nulang-abi-v1",
            "native",
            ["lto=thin", "opt=3", "lto=thin"],
        );
        assert_eq!(first.artifact_id(), reordered.artifact_id());
        assert_eq!(
            first.flags().collect::<Vec<_>>(),
            reordered.flags().collect::<Vec<_>>()
        );
    }

    #[test]
    fn tampered_codegen_inputs_fail_closed() {
        let bytes = manifest().to_json().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["target"] = serde_json::Value::from("aarch64-unknown-linux-gnu");
        let bytes = serde_json::to_vec(&value).unwrap();

        assert!(matches!(
            ArtifactIdentityManifest::from_json(&bytes),
            Err(ArtifactIdentityError::ArtifactIdentityMismatch { .. })
        ));
    }

    #[test]
    fn unknown_manifest_version_fails_closed() {
        let bytes = manifest().to_json().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["version"] = serde_json::Value::from(2);
        let bytes = serde_json::to_vec(&value).unwrap();

        assert_eq!(
            ArtifactIdentityManifest::from_json(&bytes).unwrap_err(),
            ArtifactIdentityError::UnsupportedVersion { actual: 2 }
        );
    }

    #[test]
    fn malformed_identity_fails_closed() {
        let bytes = manifest().to_json().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["semantic_id"] = serde_json::Value::from("not-a-semantic-id");
        let bytes = serde_json::to_vec(&value).unwrap();

        assert!(matches!(
            ArtifactIdentityManifest::from_json(&bytes),
            Err(ArtifactIdentityError::InvalidIdentity {
                field: "semantic_id",
                ..
            })
        ));
    }
}
