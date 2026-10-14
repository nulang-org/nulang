//! Exact executable/behavior pin for a concrete durable execution.
//!
//! This is a cloud-neutral admission contract. It does not silently infer
//! replay compatibility from schema migration topology or a shared semantic ID.
//! The caller must persist the pin atomically with execution creation and
//! retain its referenced artifact for future recovery.

use crate::behavior_manifest::{BehaviorManifest, BehaviorManifestError};
use crate::content_identity::ArtifactId;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const DURABLE_EXECUTION_PIN_VERSION: u16 = 1;

/// Immutable recovery identity for a single durable execution.
///
/// A pin is not a signed attestation. Storage owners must protect it with the
/// same integrity and atomicity guarantees as the execution's committed history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableExecutionPin {
    version: u16,
    execution_id: String,
    package_name: String,
    artifact_id: String,
    executable_digest: String,
    behavior_digest: String,
}

impl DurableExecutionPin {
    /// Construct a pin only after verifying the sidecar against actual bytes.
    pub fn create(
        execution_id: impl Into<String>,
        manifest: &BehaviorManifest,
        artifact_bytes: &[u8],
    ) -> Result<Self, ExecutionPinError> {
        manifest
            .verify_artifact_bytes(artifact_bytes)
            .map_err(ExecutionPinError::Manifest)?;
        let pin = Self {
            version: DURABLE_EXECUTION_PIN_VERSION,
            execution_id: execution_id.into(),
            package_name: manifest.package.name.clone(),
            artifact_id: manifest.artifact.artifact_id.clone(),
            executable_digest: manifest.artifact.digest.clone(),
            behavior_digest: manifest.digest().map_err(ExecutionPinError::Manifest)?,
        };
        pin.validate()?;
        Ok(pin)
    }

    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    pub fn to_json(&self) -> Result<Vec<u8>, ExecutionPinError> {
        self.validate()?;
        serde_json::to_vec_pretty(self).map_err(|error| ExecutionPinError::Json(error.to_string()))
    }

    /// Parse an untrusted stored pin. Missing metadata is never interpreted as
    /// permission to resume using a newer artifact.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ExecutionPinError> {
        let pin: Self = serde_json::from_slice(bytes)
            .map_err(|error| ExecutionPinError::Json(error.to_string()))?;
        pin.validate()?;
        Ok(pin)
    }

    /// Admit recovery only when the execution ID, artifact bytes, and complete
    /// behavior manifest match the original pin.
    ///
    /// An upgrade with compatible state schemas is *not* sufficient evidence
    /// to resume an already-started execution on different executable code.
    /// Running a verified replay migration will require a separate, explicit
    /// attestation protocol, not an implicit bypass of this check.
    pub fn validate_resume(
        &self,
        execution_id: &str,
        candidate: &BehaviorManifest,
        artifact_bytes: &[u8],
    ) -> Result<(), ExecutionPinError> {
        self.validate()?;
        if execution_id != self.execution_id {
            return Err(ExecutionPinError::ExecutionIdMismatch {
                pinned: self.execution_id.clone(),
                requested: execution_id.to_string(),
            });
        }

        candidate
            .verify_artifact_bytes(artifact_bytes)
            .map_err(ExecutionPinError::Manifest)?;

        if candidate.package.name != self.package_name {
            return Err(ExecutionPinError::PackageMismatch {
                pinned: self.package_name.clone(),
                incoming: candidate.package.name.clone(),
            });
        }
        if candidate.artifact.artifact_id != self.artifact_id {
            return Err(ExecutionPinError::ArtifactIdMismatch {
                pinned: self.artifact_id.clone(),
                incoming: candidate.artifact.artifact_id.clone(),
            });
        }
        if candidate.artifact.digest != self.executable_digest {
            return Err(ExecutionPinError::ExecutableDigestMismatch {
                pinned: self.executable_digest.clone(),
                incoming: candidate.artifact.digest.clone(),
            });
        }

        let incoming = candidate.digest().map_err(ExecutionPinError::Manifest)?;
        if incoming != self.behavior_digest {
            return Err(ExecutionPinError::BehaviorDigestMismatch {
                pinned: self.behavior_digest.clone(),
                incoming,
            });
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ExecutionPinError> {
        if self.version != DURABLE_EXECUTION_PIN_VERSION {
            return Err(ExecutionPinError::UnsupportedVersion {
                version: self.version,
            });
        }
        if self.execution_id.trim().is_empty() {
            return Err(ExecutionPinError::InvalidExecutionId);
        }
        if self.package_name.trim().is_empty() {
            return Err(ExecutionPinError::InvalidPackage);
        }
        if self.artifact_id.parse::<ArtifactId>().is_err() {
            return Err(ExecutionPinError::InvalidArtifactId);
        }
        for (field, digest) in [
            ("executable_digest", &self.executable_digest),
            ("behavior_digest", &self.behavior_digest),
        ] {
            let Some(hex) = digest.strip_prefix("blake3:") else {
                return Err(ExecutionPinError::InvalidDigest { field });
            };
            if hex.len() != 64
                || !hex
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(ExecutionPinError::InvalidDigest { field });
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum ExecutionPinError {
    Json(String),
    Manifest(BehaviorManifestError),
    UnsupportedVersion { version: u16 },
    InvalidExecutionId,
    InvalidPackage,
    InvalidArtifactId,
    InvalidDigest { field: &'static str },
    ExecutionIdMismatch { pinned: String, requested: String },
    PackageMismatch { pinned: String, incoming: String },
    ArtifactIdMismatch { pinned: String, incoming: String },
    ExecutableDigestMismatch { pinned: String, incoming: String },
    BehaviorDigestMismatch { pinned: String, incoming: String },
}

impl fmt::Display for ExecutionPinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid execution pin JSON: {message}"),
            Self::Manifest(error) => write!(f, "invalid execution artifact: {error}"),
            Self::UnsupportedVersion { version } => write!(
                f,
                "unsupported durable execution pin version {version}; expected {DURABLE_EXECUTION_PIN_VERSION}"
            ),
            Self::InvalidExecutionId => write!(f, "durable execution pin has empty execution ID"),
            Self::InvalidPackage => write!(f, "durable execution pin has empty package name"),
            Self::InvalidArtifactId => write!(f, "durable execution pin has invalid artifact ID"),
            Self::InvalidDigest { field } => write!(f, "durable execution pin has invalid {field}"),
            Self::ExecutionIdMismatch { pinned, requested } => write!(
                f,
                "execution identity mismatch: pin belongs to '{pinned}', requested '{requested}'"
            ),
            Self::PackageMismatch { pinned, incoming } => write!(
                f,
                "durable execution package mismatch: pinned '{pinned}', incoming '{incoming}'"
            ),
            Self::ArtifactIdMismatch { pinned, incoming } => write!(
                f,
                "durable execution artifact ID mismatch: pinned '{pinned}', incoming '{incoming}'"
            ),
            Self::ExecutableDigestMismatch { pinned, incoming } => write!(
                f,
                "durable execution executable digest mismatch: pinned '{pinned}', incoming '{incoming}'"
            ),
            Self::BehaviorDigestMismatch { pinned, incoming } => write!(
                f,
                "durable execution behavior digest mismatch: pinned '{pinned}', incoming '{incoming}'"
            ),
        }
    }
}

impl std::error::Error for ExecutionPinError {}
