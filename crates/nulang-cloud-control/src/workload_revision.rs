use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

pub const WORKLOAD_REVISION_SCHEMA: &str = "nulang.workload-revision/v0alpha1";
pub const WORKLOAD_ARTIFACT_KIND_NBC_V1: &str = "nulang-bytecode-v1";
const WORKLOAD_REVISION_DIGEST_DOMAIN: &[u8] = b"nulang.workload-revision.v0alpha1\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadArtifactIdentity {
    /// RFC 0020 artifact kind. The first Cloud launch contract supports NBC v1.
    pub kind: String,
    /// Compiler-derived ArtifactId. Cloud treats the value as opaque identity
    /// and does not duplicate compiler identity derivation logic.
    pub artifact_id: String,
    /// Digest of the exact executable bytes.
    pub digest: String,
    /// Digest of the canonical RFC 0020 behavior manifest.
    pub behavior_manifest_digest: String,
    /// Target/ABI/backend are copied from the admitted manifest so a node can
    /// reject an incompatible artifact before attempting execution.
    pub target: String,
    pub abi: String,
    pub backend: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkloadLaunchConfig {
    /// Ordered application arguments. Argument order is identity-bearing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Names of runtime configuration values required by the workload.
    ///
    /// Values and secrets are deliberately absent from the immutable revision
    /// contract. They are injected at execution time through the workload
    /// identity/configuration boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadRevisionSpec {
    pub schema: String,
    pub deployment_id: String,
    pub revision: u64,
    pub package_name: String,
    pub package_version: String,
    pub artifact: WorkloadArtifactIdentity,
    #[serde(default)]
    pub launch: WorkloadLaunchConfig,
}

impl WorkloadRevisionSpec {
    pub fn new(
        deployment_id: impl Into<String>,
        revision: u64,
        package_name: impl Into<String>,
        package_version: impl Into<String>,
        artifact: WorkloadArtifactIdentity,
        launch: WorkloadLaunchConfig,
    ) -> Result<Self, WorkloadRevisionError> {
        let mut spec = Self {
            schema: WORKLOAD_REVISION_SCHEMA.to_string(),
            deployment_id: deployment_id.into(),
            revision,
            package_name: package_name.into(),
            package_version: package_version.into(),
            artifact,
            launch,
        };
        spec.normalize();
        spec.validate()?;
        Ok(spec)
    }

    /// Validate an untrusted workload revision without importing compiler
    /// internals into the Cloud control-plane crate.
    pub fn validate(&self) -> Result<(), WorkloadRevisionError> {
        if self.schema != WORKLOAD_REVISION_SCHEMA {
            return Err(WorkloadRevisionError::Invalid(format!(
                "unsupported workload revision schema {}",
                self.schema
            )));
        }
        validate_identifier("deployment_id", &self.deployment_id)?;
        if self.revision == 0 {
            return Err(WorkloadRevisionError::Invalid(
                "revision must be greater than zero".into(),
            ));
        }
        validate_identifier("package_name", &self.package_name)?;
        validate_identifier("package_version", &self.package_version)?;

        if self.artifact.kind != WORKLOAD_ARTIFACT_KIND_NBC_V1 {
            return Err(WorkloadRevisionError::Invalid(format!(
                "unsupported workload artifact kind {}",
                self.artifact.kind
            )));
        }
        validate_identifier("artifact_id", &self.artifact.artifact_id)?;
        validate_blake3_digest("artifact.digest", &self.artifact.digest)?;
        validate_blake3_digest(
            "artifact.behavior_manifest_digest",
            &self.artifact.behavior_manifest_digest,
        )?;
        validate_identifier("artifact.target", &self.artifact.target)?;
        validate_identifier("artifact.abi", &self.artifact.abi)?;
        validate_identifier("artifact.backend", &self.artifact.backend)?;

        for (index, argument) in self.launch.args.iter().enumerate() {
            if argument.contains('\0') {
                return Err(WorkloadRevisionError::Invalid(format!(
                    "launch.args[{index}] must not contain NUL"
                )));
            }
        }

        let mut config_keys = BTreeSet::new();
        for key in &self.launch.config_keys {
            validate_identifier("launch.config_keys[]", key)?;
            if !config_keys.insert(key) {
                return Err(WorkloadRevisionError::Invalid(format!(
                    "duplicate launch config key {key}"
                )));
            }
        }

        Ok(())
    }

    /// Domain-separated digest of the canonical workload launch contract.
    ///
    /// Artifact locations are intentionally not part of this structure. A CAS
    /// mirror may move without changing workload identity as long as fetched
    /// executable/manifest bytes verify against these digests.
    pub fn digest(&self) -> Result<String, WorkloadRevisionError> {
        let mut normalized = self.clone();
        normalized.normalize();
        normalized.validate()?;
        let bytes = serde_json::to_vec(&normalized)
            .map_err(|error| WorkloadRevisionError::Serialization(error.to_string()))?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(WORKLOAD_REVISION_DIGEST_DOMAIN);
        hasher.update(&bytes);
        Ok(format!("blake3:{}", hasher.finalize().to_hex()))
    }

    pub fn normalized(mut self) -> Result<Self, WorkloadRevisionError> {
        self.normalize();
        self.validate()?;
        Ok(self)
    }

    fn normalize(&mut self) {
        self.launch.config_keys.sort();
        self.launch.config_keys.dedup();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadRevisionRegistrationOutcome {
    Applied,
    AlreadyRegistered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkloadRevisionError {
    Invalid(String),
    Serialization(String),
}

impl fmt::Display for WorkloadRevisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid workload revision: {message}"),
            Self::Serialization(message) => {
                write!(f, "workload revision serialization failed: {message}")
            }
        }
    }
}

impl std::error::Error for WorkloadRevisionError {}

fn validate_identifier(field: &str, value: &str) -> Result<(), WorkloadRevisionError> {
    if value.is_empty() || value.trim() != value {
        return Err(WorkloadRevisionError::Invalid(format!(
            "{field} must be non-empty and have no surrounding whitespace"
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(WorkloadRevisionError::Invalid(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

fn validate_blake3_digest(field: &str, value: &str) -> Result<(), WorkloadRevisionError> {
    let Some(hex) = value.strip_prefix("blake3:") else {
        return Err(WorkloadRevisionError::Invalid(format!(
            "{field} must use blake3:<64 hex>"
        )));
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WorkloadRevisionError::Invalid(format!(
            "{field} must use blake3:<64 hex>"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(byte: char) -> String {
        format!("blake3:{}", byte.to_string().repeat(64))
    }

    fn spec() -> WorkloadRevisionSpec {
        WorkloadRevisionSpec::new(
            "api",
            7,
            "api-package",
            "1.2.3",
            WorkloadArtifactIdentity {
                kind: WORKLOAD_ARTIFACT_KIND_NBC_V1.into(),
                artifact_id: "artifact:api-v7".into(),
                digest: digest('a'),
                behavior_manifest_digest: digest('b'),
                target: "x86_64-unknown-linux-gnu".into(),
                abi: "nulang-v1".into(),
                backend: "bytecode".into(),
            },
            WorkloadLaunchConfig {
                args: vec!["serve".into(), "--port=8080".into()],
                config_keys: vec!["DATABASE_URL".into(), "API_KEY".into()],
            },
        )
        .unwrap()
    }

    #[test]
    fn digest_is_deterministic_and_config_key_order_is_canonical() {
        let first = spec();
        let mut second = spec();
        second.launch.config_keys.reverse();
        second = second.normalized().unwrap();

        assert_eq!(first, second);
        assert_eq!(first.digest().unwrap(), second.digest().unwrap());
    }

    #[test]
    fn executable_and_manifest_digests_both_participate_in_identity() {
        let first = spec();
        let mut artifact_changed = first.clone();
        artifact_changed.artifact.digest = digest('c');
        let mut manifest_changed = first.clone();
        manifest_changed.artifact.behavior_manifest_digest = digest('d');

        assert_ne!(first.digest().unwrap(), artifact_changed.digest().unwrap());
        assert_ne!(first.digest().unwrap(), manifest_changed.digest().unwrap());
    }

    #[test]
    fn launch_argument_order_is_identity_bearing() {
        let first = spec();
        let mut changed = first.clone();
        changed.launch.args.reverse();

        assert_ne!(first.digest().unwrap(), changed.digest().unwrap());
    }

    #[test]
    fn mutable_locations_are_not_part_of_revision_contract() {
        let json = serde_json::to_value(spec()).unwrap();
        let artifact = json.get("artifact").unwrap().as_object().unwrap();

        assert!(!artifact.contains_key("url"));
        assert!(!artifact.contains_key("uri"));
        assert!(!artifact.contains_key("path"));
        assert!(!artifact.contains_key("tag"));
    }

    #[test]
    fn invalid_digest_and_unknown_artifact_kind_fail_closed() {
        let mut invalid = spec();
        invalid.artifact.digest = "latest".into();
        assert!(invalid.validate().is_err());

        let mut unknown = spec();
        unknown.artifact.kind = "container-tag".into();
        assert!(unknown.validate().is_err());
    }
}
