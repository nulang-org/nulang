//! Compiler/package-produced RFC 0020 Behavior Manifest.
//!
//! The public JSON shape in this module intentionally mirrors
//! `spec/behavior/v0alpha1.schema.json`. Compiler-internal analysis may be
//! richer, but anything emitted with schema identity
//! `nulang.behavior/v0alpha1` must remain valid against that checked schema.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::format::constants::LANGUAGE_VERSION_STR;
use crate::package::identity::source_id_for_package_dir;
use crate::package::lockfile::LOCKFILE_FILE;
use crate::package::manifest::Manifest;

pub const BEHAVIOR_MANIFEST_SCHEMA: &str = "nulang.behavior/v0alpha1";
pub const COMPLETENESS_EXTENSION: &str = "nulang.org/completeness";
pub const PACKAGE_CAPABILITIES_EXTENSION: &str = "nulang.org/package-capabilities";
pub const HOST_EFFECT_ABI_EXTENSION: &str = "nulang.org/host-effect-abi";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorManifest {
    pub schema: String,
    pub package: BehaviorPackage,
    pub artifact: ArtifactBinding,
    pub compiler: CompilerIdentity,
    #[serde(default)]
    pub interfaces: Vec<BehaviorInterface>,
    #[serde(default)]
    pub actors: Vec<BehaviorActor>,
    #[serde(default)]
    pub effects: Vec<BehaviorEffect>,
    #[serde(default)]
    pub authority: Vec<BehaviorAuthority>,
    #[serde(default)]
    pub durability: Vec<BehaviorDurability>,
    #[serde(default)]
    pub replay: Vec<BehaviorReplay>,
    #[serde(default)]
    pub resources: BTreeMap<String, serde_json::Value>,
    pub provenance: Provenance,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorPackage {
    pub name: String,
    pub version: String,
    pub language_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactBinding {
    pub kind: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompilerIdentity {
    pub implementation: String,
    pub version: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorInterface {
    pub name: String,
    pub input: String,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorActor {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub durability: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_schema: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorEffect {
    pub effect: String,
    pub class: String,
    pub determinism: String,
    pub replay: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorAuthority {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorDurability {
    pub owner: String,
    pub persistence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_contract: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorReplay {
    pub effect: String,
    pub class: String,
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
pub struct SemanticCompleteness {
    pub interfaces: String,
    pub actors: String,
    pub effects: String,
    pub authority: String,
    pub durability: String,
    pub replay: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEffectAbiBinding {
    pub schema: String,
    pub contract_digest: String,
}

impl BehaviorManifest {
    /// Build the artifact/provenance slice of RFC 0020 for an already-emitted
    /// portable WASM artifact.
    ///
    /// Package-declared capabilities are configuration, not compiler proof, so
    /// v0alpha1 records them under an advisory namespaced extension rather than
    /// the normative `authority` array.
    pub fn for_wasm(
        package_root: &Path,
        package_manifest: &Manifest,
        wasm_path: &Path,
    ) -> Result<Self, BehaviorManifestError> {
        let wasm = fs::read(wasm_path).map_err(|source| BehaviorManifestError::Io {
            operation: "read compiled WASM artifact",
            path: wasm_path.to_path_buf(),
            source,
        })?;
        let artifact_digest = digest_bytes(&wasm);

        let source_id =
            source_id_for_package_dir(package_root).map_err(|source| BehaviorManifestError::Io {
                operation: "compute package source identity",
                path: package_root.to_path_buf(),
                source,
            })?;

        let lock_path = package_root.join(LOCKFILE_FILE);
        let dependency_digest = if lock_path.exists() {
            let bytes = fs::read(&lock_path).map_err(|source| BehaviorManifestError::Io {
                operation: "read dependency lockfile",
                path: lock_path.clone(),
                source,
            })?;
            digest_bytes(&bytes)
        } else {
            // Canonical empty dependency graph for callers that construct a
            // manifest before a lockfile is materialized.
            digest_bytes(&[])
        };

        let compiler_path =
            std::env::current_exe().map_err(BehaviorManifestError::CurrentExecutable)?;
        let compiler_bytes =
            fs::read(&compiler_path).map_err(|source| BehaviorManifestError::Io {
                operation: "read compiler executable",
                path: compiler_path,
                source,
            })?;

        let declared_capabilities: BTreeSet<String> = package_manifest
            .package
            .capabilities
            .iter()
            .cloned()
            .collect();

        let mut extensions = BTreeMap::new();
        extensions.insert(
            HOST_EFFECT_ABI_EXTENSION.to_string(),
            serde_json::to_value(HostEffectAbiBinding {
                schema: crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA.to_string(),
                contract_digest: digest_bytes(include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/spec/host-effects/v0alpha1.json"
                ))),
            })
            .map_err(BehaviorManifestError::Json)?,
        );
        extensions.insert(
            COMPLETENESS_EXTENSION.to_string(),
            serde_json::to_value(SemanticCompleteness {
                interfaces: "not-emitted".to_string(),
                actors: "not-emitted".to_string(),
                effects: "not-emitted".to_string(),
                authority: "not-emitted".to_string(),
                durability: "not-emitted".to_string(),
                replay: "not-emitted".to_string(),
            })
            .map_err(BehaviorManifestError::Json)?,
        );
        if !declared_capabilities.is_empty() {
            extensions.insert(
                PACKAGE_CAPABILITIES_EXTENSION.to_string(),
                serde_json::to_value(declared_capabilities.into_iter().collect::<Vec<_>>())
                    .map_err(BehaviorManifestError::Json)?,
            );
        }

        Ok(Self {
            schema: BEHAVIOR_MANIFEST_SCHEMA.to_string(),
            package: BehaviorPackage {
                name: package_manifest.package.name.clone(),
                version: package_manifest.package.version.clone(),
                language_version: LANGUAGE_VERSION_STR.to_string(),
            },
            artifact: ArtifactBinding {
                kind: "wasm-module".to_string(),
                digest: artifact_digest,
            },
            compiler: CompilerIdentity {
                implementation: "nulang-rust".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                digest: digest_bytes(&compiler_bytes),
            },
            interfaces: Vec::new(),
            actors: Vec::new(),
            effects: Vec::new(),
            authority: Vec::new(),
            durability: Vec::new(),
            replay: Vec::new(),
            resources: BTreeMap::new(),
            provenance: Provenance {
                source_digest: format!("blake3:{source_id}"),
                dependency_digest,
                interface_digests: Vec::new(),
                state_schema_digests: Vec::new(),
            },
            extensions,
        })
    }

    pub fn completeness(&self) -> Option<SemanticCompleteness> {
        self.extensions
            .get(COMPLETENESS_EXTENSION)
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
    }

    pub fn set_completeness(
        &mut self,
        completeness: SemanticCompleteness,
    ) -> Result<(), BehaviorManifestError> {
        self.extensions.insert(
            COMPLETENESS_EXTENSION.to_string(),
            serde_json::to_value(completeness).map_err(BehaviorManifestError::Json)?,
        );
        Ok(())
    }

    /// Deterministic JSON bytes used as the canonical v0alpha1 representation.
    pub fn canonical_json(&self) -> Result<Vec<u8>, BehaviorManifestError> {
        serde_json::to_vec(self).map_err(BehaviorManifestError::Json)
    }

    pub fn digest(&self) -> Result<String, BehaviorManifestError> {
        Ok(digest_bytes(&self.canonical_json()?))
    }

    /// Write the human-readable sidecar next to the portable artifact.
    ///
    /// `app.wasm` becomes `app.behavior.json`.
    pub fn write_next_to(&self, wasm_path: &Path) -> Result<PathBuf, BehaviorManifestError> {
        let file_stem = wasm_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| BehaviorManifestError::InvalidArtifactPath(wasm_path.to_path_buf()))?;
        let output = wasm_path.with_file_name(format!("{file_stem}.behavior.json"));
        let mut bytes = serde_json::to_vec_pretty(self).map_err(BehaviorManifestError::Json)?;
        bytes.push(b'\n');
        fs::write(&output, bytes).map_err(|source| BehaviorManifestError::Io {
            operation: "write behavior manifest",
            path: output.clone(),
            source,
        })?;
        Ok(output)
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

#[derive(Debug)]
pub enum BehaviorManifestError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
    CurrentExecutable(std::io::Error),
    Json(serde_json::Error),
    InvalidArtifactPath(PathBuf),
}

impl fmt::Display for BehaviorManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "{operation} at {} failed: {source}", path.display()),
            Self::CurrentExecutable(source) => {
                write!(f, "cannot identify compiler executable: {source}")
            }
            Self::Json(source) => write!(f, "behavior manifest JSON failed: {source}"),
            Self::InvalidArtifactPath(path) => {
                write!(f, "invalid compiled artifact path: {}", path.display())
            }
        }
    }
}

impl std::error::Error for BehaviorManifestError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_behavior_manifest_{name}_{}",
            std::process::id()
        ))
    }

    fn fixture(name: &str) -> (PathBuf, Manifest, PathBuf) {
        let root = scratch(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".nula/dist")).unwrap();
        fs::write(
            root.join("Nulang.toml"),
            r#"[package]
name = "demo"
version = "0.3.0"
capabilities = ["net", "fs", "net"]
"#,
        )
        .unwrap();
        fs::write(root.join("src/main.nula"), "fn main() { 42 }\n").unwrap();
        fs::write(root.join("Nulang.lock"), "version = 1\n").unwrap();
        let wasm_path = root.join(".nula/dist/demo.wasm");
        fs::write(&wasm_path, b"\0asm\x01\0\0\0fixture").unwrap();
        let manifest = Manifest::load(&root).unwrap();
        (root, manifest, wasm_path)
    }

    #[test]
    fn artifact_slice_matches_public_v0alpha1_shape() {
        let (root, package, wasm_path) = fixture("shape");
        let manifest = BehaviorManifest::for_wasm(&root, &package, &wasm_path).unwrap();
        let value = serde_json::to_value(&manifest).unwrap();

        assert_eq!(value["schema"], BEHAVIOR_MANIFEST_SCHEMA);
        assert!(value.get("completeness").is_none());
        assert!(value["compiler"]["digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("blake3:") && digest.len() == 71));
        assert!(value["provenance"]["source_digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("blake3:") && digest.len() == 71));
        assert!(value["provenance"]["dependency_digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("blake3:") && digest.len() == 71));
        assert!(manifest.authority.is_empty());
        let host_abi: HostEffectAbiBinding = serde_json::from_value(
            manifest
                .extensions
                .get(HOST_EFFECT_ABI_EXTENSION)
                .cloned()
                .expect("host effect ABI binding"),
        )
        .unwrap();
        assert_eq!(host_abi.schema, crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA);
        assert!(
            host_abi.contract_digest.starts_with("blake3:")
                && host_abi.contract_digest.len() == 71
        );
        assert_eq!(
            manifest
                .extensions
                .get(PACKAGE_CAPABILITIES_EXTENSION)
                .and_then(serde_json::Value::as_array)
                .unwrap()
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["fs", "net"]
        );

        let completeness = manifest.completeness().unwrap();
        assert_eq!(completeness.effects, "not-emitted");
        assert_eq!(completeness.authority, "not-emitted");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn canonical_json_is_reproducible_and_changes_when_artifact_changes() {
        let (root, package, wasm_path) = fixture("canonical");
        let first = BehaviorManifest::for_wasm(&root, &package, &wasm_path).unwrap();
        let same = BehaviorManifest::for_wasm(&root, &package, &wasm_path).unwrap();
        assert_eq!(first.canonical_json().unwrap(), same.canonical_json().unwrap());
        assert_eq!(first.digest().unwrap(), same.digest().unwrap());

        fs::write(&wasm_path, b"\0asm\x01\0\0\0changed").unwrap();
        let changed = BehaviorManifest::for_wasm(&root, &package, &wasm_path).unwrap();
        assert_ne!(first.artifact.digest, changed.artifact.digest);
        assert_ne!(first.digest().unwrap(), changed.digest().unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn writes_expected_sidecar_path() {
        let (root, package, wasm_path) = fixture("sidecar");
        let manifest = BehaviorManifest::for_wasm(&root, &package, &wasm_path).unwrap();
        let path = manifest.write_next_to(&wasm_path).unwrap();
        assert_eq!(path.file_name().unwrap(), "demo.behavior.json");

        let bytes = fs::read(path).unwrap();
        let restored: BehaviorManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored, manifest);
        let _ = fs::remove_dir_all(root);
    }
}
