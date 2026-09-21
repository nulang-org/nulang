//! Compiler/package-produced semantic sidecar for deployable Nulang artifacts.
//!
//! RFC 0020 defines the long-term Behavior Manifest contract between Nulang
//! and deployment systems such as Nulang Cloud. This module implements the
//! first deliberately narrow slice of that contract: exact artifact binding,
//! package/compiler identity, reproducible provenance, and explicitly labelled
//! package-declared authority requirements.
//!
//! Semantic inventories that are not yet compiler-emitted are represented as
//! incomplete rather than guessed from source text. Cloud and other consumers
//! must therefore treat the completeness markers as part of admission policy.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::format::constants::LANGUAGE_VERSION_STR;
use crate::package::identity::source_id_for_package_dir;
use crate::package::lockfile::LOCKFILE_FILE;
use crate::package::manifest::Manifest;
use crate::semantic_inventory::CompilerSemanticInventory;

pub const BEHAVIOR_MANIFEST_SCHEMA: &str = "nulang.behavior/v0alpha1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorManifest {
    pub schema: String,
    pub package: BehaviorPackage,
    pub artifact: ArtifactBinding,
    pub compiler: CompilerIdentity,
    #[serde(default)]
    pub interfaces: Vec<serde_json::Value>,
    #[serde(default)]
    pub actors: Vec<serde_json::Value>,
    #[serde(default)]
    pub effects: Vec<serde_json::Value>,
    #[serde(default)]
    pub authority: Vec<AuthorityRequirement>,
    #[serde(default)]
    pub durability: Vec<serde_json::Value>,
    #[serde(default)]
    pub replay: Vec<serde_json::Value>,
    #[serde(default)]
    pub resources: BTreeMap<String, serde_json::Value>,
    pub provenance: Provenance,
    pub completeness: SemanticCompleteness,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorityRequirement {
    pub capability: String,
    pub source: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source_tree: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency_lock: Option<String>,
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

impl BehaviorManifest {
    /// Build the first RFC 0020 sidecar for an already-emitted portable WASM
    /// artifact.
    ///
    /// This function intentionally does not parse source to infer effects,
    /// actors, or durability. Those facts belong to the checked compiler
    /// pipeline and will be populated by later RFC 0020 implementation slices.
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
        let artifact_digest = format!("blake3:{}", blake3::hash(&wasm).to_hex());

        let source_id =
            source_id_for_package_dir(package_root).map_err(|source| BehaviorManifestError::Io {
                operation: "compute package source identity",
                path: package_root.to_path_buf(),
                source,
            })?;

        let lock_path = package_root.join(LOCKFILE_FILE);
        let dependency_lock = if lock_path.exists() {
            let bytes = fs::read(&lock_path).map_err(|source| BehaviorManifestError::Io {
                operation: "read dependency lockfile",
                path: lock_path.clone(),
                source,
            })?;
            Some(format!("blake3:{}", blake3::hash(&bytes).to_hex()))
        } else {
            None
        };

        let declared_capabilities: BTreeSet<String> = package_manifest
            .package
            .capabilities
            .iter()
            .cloned()
            .collect();
        let authority = declared_capabilities
            .into_iter()
            .map(|capability| AuthorityRequirement {
                capability,
                source: "package-manifest".to_string(),
                status: "declared-not-inferred".to_string(),
            })
            .collect();

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
            },
            interfaces: Vec::new(),
            actors: Vec::new(),
            effects: Vec::new(),
            authority,
            durability: Vec::new(),
            replay: Vec::new(),
            resources: BTreeMap::new(),
            provenance: Provenance {
                source_tree: format!("nulang-source-id-v1:{source_id}"),
                dependency_lock,
            },
            completeness: SemanticCompleteness {
                interfaces: "not-emitted".to_string(),
                actors: "not-emitted".to_string(),
                effects: "not-emitted".to_string(),
                authority: "package-declared-only".to_string(),
                durability: "not-emitted".to_string(),
                replay: "not-emitted".to_string(),
            },
        })
    }

    /// Merge compiler-derived semantics into the artifact-bound package
    /// manifest. The compiler inventory is authoritative for program meaning;
    /// package declarations remain visible as user-declared configuration.
    pub fn with_compiler_semantics(
        mut self,
        inventory: &CompilerSemanticInventory,
    ) -> Result<Self, BehaviorManifestError> {
        self.effects = inventory
            .effects
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(BehaviorManifestError::Json)?;
        self.actors = inventory
            .actors
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(BehaviorManifestError::Json)?;

        for capability in &inventory.required_authority {
            self.authority.push(AuthorityRequirement {
                capability: capability.clone(),
                source: "compiler-effect-inference".to_string(),
                status: "required-category".to_string(),
            });
        }
        self.authority.sort_by(|left, right| {
            (&left.capability, &left.source).cmp(&(&right.capability, &right.source))
        });
        self.authority.dedup_by(|left, right| {
            left.capability == right.capability
                && left.source == right.source
                && left.status == right.status
        });

        self.completeness.actors = "compiler-derived-with-per-actor-status".to_string();
        self.completeness.effects = "compiler-module-conservative".to_string();
        self.completeness.authority =
            "compiler-inferred-categories+package-declared".to_string();

        Ok(self)
    }

    /// Deterministic JSON bytes used as the canonical v0alpha1 representation.
    ///
    /// Struct field order is fixed by this schema, maps are BTreeMaps, and
    /// set-like authority declarations are sorted before construction.
    pub fn canonical_json(&self) -> Result<Vec<u8>, BehaviorManifestError> {
        serde_json::to_vec(self).map_err(BehaviorManifestError::Json)
    }

    pub fn digest(&self) -> Result<String, BehaviorManifestError> {
        let bytes = self.canonical_json()?;
        Ok(format!("blake3:{}", blake3::hash(&bytes).to_hex()))
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

#[derive(Debug)]
pub enum BehaviorManifestError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },
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
    fn first_slice_binds_exact_artifact_and_labels_incomplete_semantics() {
        let (root, package, wasm_path) = fixture("binding");
        let manifest = BehaviorManifest::for_wasm(&root, &package, &wasm_path).unwrap();

        let expected = format!(
            "blake3:{}",
            blake3::hash(&fs::read(&wasm_path).unwrap()).to_hex()
        );
        assert_eq!(manifest.schema, BEHAVIOR_MANIFEST_SCHEMA);
        assert_eq!(manifest.artifact.digest, expected);
        assert_eq!(manifest.package.language_version, LANGUAGE_VERSION_STR);
        assert_eq!(manifest.completeness.effects, "not-emitted");
        assert_eq!(manifest.completeness.authority, "package-declared-only");
        assert_eq!(
            manifest
                .authority
                .iter()
                .map(|entry| entry.capability.as_str())
                .collect::<Vec<_>>(),
            vec!["fs", "net"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn compiler_semantics_promote_effect_actor_and_authority_completeness() {
        use crate::semantic_inventory::{
            SemanticActor, SemanticBehavior, SemanticEffect,
            COMPILER_SEMANTIC_INVENTORY_SCHEMA,
        };

        let (root, package, wasm_path) = fixture("semantics");
        let inventory = CompilerSemanticInventory {
            schema: COMPILER_SEMANTIC_INVENTORY_SCHEMA.to_string(),
            effects: vec![SemanticEffect {
                subject_kind: "function".to_string(),
                subject: "fetch".to_string(),
                effects: vec!["Net".to_string()],
                open: false,
            }],
            actors: vec![SemanticActor {
                name: "Worker".to_string(),
                protocol_id: Some(format!("blake3:{}", "a".repeat(64))),
                protocol_status: "complete".to_string(),
                protocol_error: None,
                behaviors: vec![SemanticBehavior {
                    name: "ping".to_string(),
                    effects: vec!["Net".to_string()],
                    open_effects: false,
                }],
            }],
            required_authority: vec!["net".to_string()],
        };

        let manifest = BehaviorManifest::for_wasm(&root, &package, &wasm_path)
            .unwrap()
            .with_compiler_semantics(&inventory)
            .unwrap();

        assert_eq!(manifest.effects.len(), 1);
        assert_eq!(manifest.actors.len(), 1);
        assert_eq!(manifest.completeness.effects, "compiler-module-conservative");
        assert_eq!(
            manifest.completeness.authority,
            "compiler-inferred-categories+package-declared"
        );
        assert!(manifest.authority.iter().any(|entry| {
            entry.capability == "net"
                && entry.source == "compiler-effect-inference"
                && entry.status == "required-category"
        }));
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
