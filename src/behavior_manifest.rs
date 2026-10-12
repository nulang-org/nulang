//! Experimental RFC 0020 Behavior Manifest support.
//!
//! This module intentionally implements a narrow, enforceable subset first:
//! artifact/compiler identity plus durable actor state-schema identity and
//! migration topology. It is designed for deployment admission and upgrade
//! preflight checks, not as a claim that runtime migration execution is
//! complete. Migration bodies do not yet have a canonical semantic encoding,
//! so `migration_identity` is explicitly `topology-only`.

use crate::artifact_identity::ArtifactIdentityManifest;
use crate::content_identity::{ArtifactId, SemanticId, SourceId};
use crate::format::constants::LANGUAGE_VERSION_STR;
use crate::hir;
use crate::semantic_schema::{
    actor_state_schemas_from_hir, canonical_actor_state_schema_bytes, ActorStateSchema,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

pub const BEHAVIOR_MANIFEST_SCHEMA: &str = "nulang.behavior/v0alpha1";
pub const BEHAVIOR_ARTIFACT_KIND_NBC_V1: &str = "nulang-bytecode-v1";
pub const BEHAVIOR_ARTIFACT_KIND_WASM_MODULE: &str = "wasm-module";
const BEHAVIOR_MANIFEST_DIGEST_DOMAIN: &[u8] = b"nulang.behavior-manifest.v0alpha1\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorManifest {
    pub schema: String,
    pub package: BehaviorPackage,
    pub artifact: BehaviorArtifact,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actors: Vec<BehaviorActor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorPackage {
    pub name: String,
    pub version: String,
    pub language_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorArtifact {
    pub kind: String,
    /// Digest of the exact executable bytes this manifest accompanies.
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    pub semantic_id: String,
    pub artifact_id: String,
    pub compiler_version: String,
    pub target: String,
    pub abi: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorActor {
    pub name: String,
    #[serde(default)]
    pub kind: BehaviorOwnerKind,
    pub persistence: BehaviorPersistence,
    pub schema_version: u32,
    pub state_schema_semantic_id: String,
    pub migration_identity: MigrationIdentityCoverage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub migrations: Vec<BehaviorMigrationStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<BehaviorWorkflow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BehaviorOwnerKind {
    Actor,
    Workflow,
    Agent,
    Organization,
}

impl Default for BehaviorOwnerKind {
    fn default() -> Self {
        Self::Actor
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorWorkflow {
    pub steps: Vec<BehaviorWorkflowStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorWorkflowStep {
    pub name: String,
    #[serde(default)]
    pub compensation: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parallel_branches: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BehaviorPersistence {
    Ephemeral,
    Durable,
    EventSourced,
    Crdt,
    Mixed,
}

impl BehaviorPersistence {
    pub fn is_durable(self) -> bool {
        !matches!(self, Self::Ephemeral)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MigrationIdentityCoverage {
    /// Version topology, state-migration presence, and migrated event
    /// names/arities are canonicalized. Migration expression bodies are not.
    TopologyOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BehaviorMigrationStep {
    pub from: u32,
    pub to: u32,
    pub state: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<BehaviorMigrationEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BehaviorMigrationEvent {
    pub name: String,
    pub arity: usize,
}

impl BehaviorManifest {
    /// Build the RFC 0020 durability subset from typed HIR plus the compiler's
    /// already-derived artifact identity.
    pub fn from_typed_hir(
        package_name: impl Into<String>,
        package_version: impl Into<String>,
        artifact: &ArtifactIdentityManifest,
        artifact_bytes: &[u8],
        hir: &hir::Module,
    ) -> Result<Self, BehaviorManifestError> {
        let package_name = package_name.into();
        let package_version = package_version.into();
        let actor_defs = actor_defs_by_name(hir);
        let schemas = actor_state_schemas_from_hir(hir);

        let mut actors = Vec::with_capacity(schemas.len());
        for schema in schemas {
            let actor = actor_defs.get(&schema.actor_name).ok_or_else(|| {
                BehaviorManifestError::ActorSchemaMismatch {
                    actor: schema.actor_name.clone(),
                }
            })?;

            actors.push(BehaviorActor {
                name: schema.actor_name.clone(),
                kind: classify_owner_kind(actor),
                persistence: classify_persistence(actor),
                schema_version: schema.version,
                state_schema_semantic_id: state_schema_semantic_id(&schema).to_string(),
                migration_identity: MigrationIdentityCoverage::TopologyOnly,
                migrations: migration_steps(&schema),
                workflow: workflow_metadata(actor),
            });
        }

        let mut manifest = Self {
            schema: BEHAVIOR_MANIFEST_SCHEMA.to_string(),
            package: BehaviorPackage {
                name: package_name,
                version: package_version,
                language_version: LANGUAGE_VERSION_STR.to_string(),
            },
            artifact: BehaviorArtifact {
                kind: behavior_artifact_kind(artifact)?.to_string(),
                digest: artifact_digest(artifact_bytes),
                source_id: artifact.source_id().map(|id| id.to_string()),
                semantic_id: artifact.semantic_id().to_string(),
                artifact_id: artifact.artifact_id().to_string(),
                compiler_version: artifact.compiler_version().to_string(),
                target: artifact.target().to_string(),
                abi: artifact.abi().to_string(),
                backend: artifact.backend().to_string(),
                flags: artifact.flags().map(str::to_string).collect(),
            },
            actors,
        };
        manifest.normalize();
        manifest.validate()?;
        Ok(manifest)
    }

    /// Deterministic JSON bytes suitable for a sidecar file and digest input.
    pub fn to_json(&self) -> Result<Vec<u8>, BehaviorManifestError> {
        let mut normalized = self.clone();
        normalized.normalize();
        normalized.validate()?;
        serde_json::to_vec_pretty(&normalized).map_err(BehaviorManifestError::from)
    }

    /// Parse untrusted manifest JSON and fail closed on unknown schema,
    /// malformed identities, duplicate actors, or invalid migration topology.
    pub fn from_json(bytes: &[u8]) -> Result<Self, BehaviorManifestError> {
        let mut manifest: Self =
            serde_json::from_slice(bytes).map_err(BehaviorManifestError::from)?;
        manifest.normalize();
        manifest.validate()?;
        Ok(manifest)
    }

    /// Domain-separated digest of the canonical compact JSON representation.
    /// Human-readable sidecar whitespace therefore does not participate in
    /// manifest identity.
    pub fn digest(&self) -> Result<String, BehaviorManifestError> {
        let mut normalized = self.clone();
        normalized.normalize();
        normalized.validate()?;
        let bytes = serde_json::to_vec(&normalized).map_err(BehaviorManifestError::from)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(BEHAVIOR_MANIFEST_DIGEST_DOMAIN);
        hasher.update(&bytes);
        Ok(format!("blake3:{}", hasher.finalize().to_hex()))
    }

    /// Verify that this sidecar accompanies the exact executable bytes it
    /// claims to describe.
    pub fn verify_artifact_bytes(
        &self,
        artifact_bytes: &[u8],
    ) -> Result<(), BehaviorManifestError> {
        self.validate()?;
        let actual = artifact_digest(artifact_bytes);
        if actual != self.artifact.digest {
            return Err(BehaviorManifestError::ArtifactDigestMismatch {
                expected: self.artifact.digest.clone(),
                actual,
            });
        }
        Ok(())
    }

    /// Manifest-level preflight for replacing a previously deployed package.
    ///
    /// Success means no structural durable-state incompatibility is visible in
    /// the two manifests. It does not prove migration-body equivalence because
    /// v0alpha1 currently identifies migration topology only; runtime recovery
    /// must still execute and validate the actual migration implementation.
    pub fn validate_upgrade_from(
        &self,
        previous: &BehaviorManifest,
    ) -> Result<(), BehaviorAdmissionError> {
        self.validate()
            .map_err(BehaviorAdmissionError::InvalidIncomingManifest)?;
        previous
            .validate()
            .map_err(BehaviorAdmissionError::InvalidPreviousManifest)?;

        if self.package.name != previous.package.name {
            return Err(BehaviorAdmissionError::PackageMismatch {
                previous: previous.package.name.clone(),
                incoming: self.package.name.clone(),
            });
        }

        let incoming_by_name: BTreeMap<_, _> = self
            .actors
            .iter()
            .map(|actor| (&actor.name, actor))
            .collect();

        for old in previous
            .actors
            .iter()
            .filter(|actor| actor.persistence.is_durable())
        {
            let Some(new) = incoming_by_name.get(&old.name) else {
                return Err(BehaviorAdmissionError::DurableOwnerRemoved {
                    actor: old.name.clone(),
                });
            };
            if !new.persistence.is_durable() {
                return Err(BehaviorAdmissionError::DurabilityRemoved {
                    actor: old.name.clone(),
                    previous: old.persistence,
                    incoming: new.persistence,
                });
            }
            if new.persistence != old.persistence {
                return Err(BehaviorAdmissionError::PersistenceClassChanged {
                    actor: old.name.clone(),
                    previous: old.persistence,
                    incoming: new.persistence,
                });
            }
            if new.schema_version < old.schema_version {
                return Err(BehaviorAdmissionError::SchemaDowngrade {
                    actor: old.name.clone(),
                    previous: old.schema_version,
                    incoming: new.schema_version,
                });
            }
            if new.schema_version == old.schema_version {
                if new.state_schema_semantic_id != old.state_schema_semantic_id {
                    return Err(BehaviorAdmissionError::UnversionedSchemaChange {
                        actor: old.name.clone(),
                        version: old.schema_version,
                    });
                }
                if new.migrations != old.migrations {
                    return Err(BehaviorAdmissionError::ExistingMigrationTopologyChanged {
                        actor: old.name.clone(),
                    });
                }
                continue;
            }

            for old_step in &old.migrations {
                if !new.migrations.iter().any(|step| step == old_step) {
                    return Err(BehaviorAdmissionError::ExistingMigrationTopologyChanged {
                        actor: old.name.clone(),
                    });
                }
            }

            for version in old.schema_version..new.schema_version {
                if !new
                    .migrations
                    .iter()
                    .any(|step| step.from == version && step.to == version + 1)
                {
                    return Err(BehaviorAdmissionError::MissingMigrationStep {
                        actor: old.name.clone(),
                        from: version,
                        to: version + 1,
                    });
                }
            }
        }

        Ok(())
    }

    fn normalize(&mut self) {
        self.artifact.flags.sort();
        self.artifact.flags.dedup();
        self.actors
            .sort_by(|left, right| left.name.cmp(&right.name));
        for actor in &mut self.actors {
            actor.migrations.sort_by_key(|step| (step.from, step.to));
            for step in &mut actor.migrations {
                step.events.sort();
                step.events.dedup();
            }
        }
    }

    fn validate(&self) -> Result<(), BehaviorManifestError> {
        if self.schema != BEHAVIOR_MANIFEST_SCHEMA {
            return Err(BehaviorManifestError::UnsupportedSchema(
                self.schema.clone(),
            ));
        }
        if self.package.name.trim().is_empty() {
            return Err(BehaviorManifestError::InvalidPackage(
                "package name must not be empty".to_string(),
            ));
        }
        if self.package.version.trim().is_empty() {
            return Err(BehaviorManifestError::InvalidPackage(
                "package version must not be empty".to_string(),
            ));
        }
        if self.artifact.kind != BEHAVIOR_ARTIFACT_KIND_NBC_V1
            && self.artifact.kind != BEHAVIOR_ARTIFACT_KIND_WASM_MODULE
        {
            return Err(BehaviorManifestError::UnsupportedArtifactKind(
                self.artifact.kind.clone(),
            ));
        }
        validate_blake3_digest("artifact.digest", &self.artifact.digest)?;

        let semantic_id =
            parse_identity::<SemanticId>("artifact.semantic_id", &self.artifact.semantic_id)?;
        let artifact_id =
            parse_identity::<ArtifactId>("artifact.artifact_id", &self.artifact.artifact_id)?;
        if let Some(source_id) = &self.artifact.source_id {
            parse_identity::<SourceId>("artifact.source_id", source_id)?;
        }
        let expected_artifact_id = ArtifactId::from_semantic(
            semantic_id,
            &self.artifact.compiler_version,
            &self.artifact.target,
            &self.artifact.abi,
            &self.artifact.backend,
            self.artifact.flags.iter(),
        );
        if artifact_id != expected_artifact_id {
            return Err(BehaviorManifestError::ArtifactIdentityMismatch {
                expected: expected_artifact_id,
                actual: artifact_id,
            });
        }

        let mut actor_names = BTreeSet::new();
        for actor in &self.actors {
            if !actor_names.insert(actor.name.clone()) {
                return Err(BehaviorManifestError::DuplicateActor(actor.name.clone()));
            }
            if actor.schema_version == 0 {
                return Err(BehaviorManifestError::InvalidActor {
                    actor: actor.name.clone(),
                    message: "schema_version must be positive".to_string(),
                });
            }
            parse_identity::<SemanticId>(
                "actors[].state_schema_semantic_id",
                &actor.state_schema_semantic_id,
            )?;

            let mut origins = BTreeSet::new();
            for step in &actor.migrations {
                if step.from == 0 || step.to != step.from.saturating_add(1) {
                    return Err(BehaviorManifestError::InvalidActor {
                        actor: actor.name.clone(),
                        message: format!(
                            "migration {} -> {} must advance exactly one positive schema version",
                            step.from, step.to
                        ),
                    });
                }
                if step.to > actor.schema_version {
                    return Err(BehaviorManifestError::InvalidActor {
                        actor: actor.name.clone(),
                        message: format!(
                            "migration {} -> {} exceeds current schema version {}",
                            step.from, step.to, actor.schema_version
                        ),
                    });
                }
                if !origins.insert(step.from) {
                    return Err(BehaviorManifestError::InvalidActor {
                        actor: actor.name.clone(),
                        message: format!("duplicate migration origin schema version {}", step.from),
                    });
                }
            }
            for from in 1..actor.schema_version {
                if !origins.contains(&from) {
                    return Err(BehaviorManifestError::InvalidActor {
                        actor: actor.name.clone(),
                        message: format!(
                            "missing migration {} -> {} required by schema version {}",
                            from,
                            from + 1,
                            actor.schema_version
                        ),
                    });
                }
            }
        }
        Ok(())
    }
}

fn behavior_artifact_kind(
    artifact: &ArtifactIdentityManifest,
) -> Result<&'static str, BehaviorManifestError> {
    match artifact.backend() {
        "bytecode" => Ok(BEHAVIOR_ARTIFACT_KIND_NBC_V1),
        "wasm" | "wasmfx" => Ok(BEHAVIOR_ARTIFACT_KIND_WASM_MODULE),
        other => Err(BehaviorManifestError::UnsupportedArtifactBackend(
            other.to_string(),
        )),
    }
}

fn parse_identity<T>(field: &'static str, value: &str) -> Result<T, BehaviorManifestError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|error| BehaviorManifestError::InvalidIdentity {
            field,
            message: error.to_string(),
        })
}

fn state_schema_semantic_id(schema: &ActorStateSchema) -> SemanticId {
    // The manifest exposes state schema separately from migration/version
    // identity. Strip RFC 0008 evolution metadata before hashing so a version
    // bump with unchanged fields does not masquerade as a state-shape change.
    let mut state_only = schema.clone();
    state_only.version = 1;
    state_only.migrations.clear();
    SemanticId::from_canonical_bytes(
        &canonical_actor_state_schema_bytes(std::slice::from_ref(&state_only)),
        [],
    )
}

fn artifact_digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn validate_blake3_digest(field: &'static str, value: &str) -> Result<(), BehaviorManifestError> {
    let Some(hex) = value.strip_prefix("blake3:") else {
        return Err(BehaviorManifestError::InvalidDigest {
            field,
            message: "expected blake3:<64 lowercase hex characters>".to_string(),
        });
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BehaviorManifestError::InvalidDigest {
            field,
            message: "expected blake3:<64 hex characters>".to_string(),
        });
    }
    Ok(())
}

fn migration_steps(schema: &ActorStateSchema) -> Vec<BehaviorMigrationStep> {
    let mut steps: Vec<_> = schema
        .migrations
        .iter()
        .map(|migration| BehaviorMigrationStep {
            from: migration.from_version,
            to: migration.to_version,
            state: migration.has_state_migration,
            events: migration
                .event_handlers
                .iter()
                .map(|event| BehaviorMigrationEvent {
                    name: event.name.clone(),
                    arity: event.arity,
                })
                .collect(),
        })
        .collect();
    steps.sort_by_key(|step| (step.from, step.to));
    for step in &mut steps {
        step.events.sort();
        step.events.dedup();
    }
    steps
}

fn classify_owner_kind(actor: &hir::ActorDef) -> BehaviorOwnerKind {
    if actor.is_workflow {
        BehaviorOwnerKind::Workflow
    } else if actor.is_agent {
        BehaviorOwnerKind::Agent
    } else if actor.is_organization {
        BehaviorOwnerKind::Organization
    } else {
        BehaviorOwnerKind::Actor
    }
}

fn workflow_metadata(actor: &hir::ActorDef) -> Option<BehaviorWorkflow> {
    if !actor.is_workflow {
        return None;
    }

    Some(BehaviorWorkflow {
        steps: actor
            .behaviors
            .iter()
            .map(|behavior| BehaviorWorkflowStep {
                name: behavior.name.clone(),
                compensation: behavior.compensate.is_some(),
                parallel_branches: behavior.parallel_branches.clone().unwrap_or_default(),
            })
            .collect(),
    })
}

fn classify_persistence(actor: &hir::ActorDef) -> BehaviorPersistence {
    let mut durable = false;
    let mut event_sourced = false;
    let mut crdt = false;

    for (_, model, _, _) in &actor.state_fields {
        match model {
            crate::ast::StateModel::Local => {}
            crate::ast::StateModel::Durable => durable = true,
            crate::ast::StateModel::EventSourced => event_sourced = true,
            crate::ast::StateModel::Crdt(_) => crdt = true,
        }
    }

    let classes = durable as usize + event_sourced as usize + crdt as usize;
    match classes {
        0 if actor.persistent => BehaviorPersistence::Durable,
        0 => BehaviorPersistence::Ephemeral,
        1 if durable => BehaviorPersistence::Durable,
        1 if event_sourced => BehaviorPersistence::EventSourced,
        1 => BehaviorPersistence::Crdt,
        _ => BehaviorPersistence::Mixed,
    }
}

fn actor_defs_by_name(module: &hir::Module) -> BTreeMap<String, &hir::ActorDef> {
    fn collect<'a>(
        decls: &'a [hir::Decl],
        namespace: &mut Vec<String>,
        out: &mut BTreeMap<String, &'a hir::ActorDef>,
    ) {
        for decl in decls {
            match decl {
                hir::Decl::Actor(actor) => {
                    let name = if namespace.is_empty() {
                        actor.name.clone()
                    } else {
                        format!("{}::{}", namespace.join("::"), actor.name)
                    };
                    out.insert(name, actor);
                }
                hir::Decl::Module { name, decls, .. } => {
                    namespace.push(name.clone());
                    collect(decls, namespace, out);
                    namespace.pop();
                }
                _ => {}
            }
        }
    }

    let mut out = BTreeMap::new();
    let mut namespace = Vec::new();
    if !module.name.is_empty() {
        namespace.push(module.name.clone());
    }
    collect(&module.decls, &mut namespace, &mut out);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BehaviorManifestError {
    Json(String),
    UnsupportedSchema(String),
    UnsupportedArtifactKind(String),
    UnsupportedArtifactBackend(String),
    InvalidPackage(String),
    InvalidDigest {
        field: &'static str,
        message: String,
    },
    InvalidIdentity {
        field: &'static str,
        message: String,
    },
    DuplicateActor(String),
    ArtifactIdentityMismatch {
        expected: ArtifactId,
        actual: ArtifactId,
    },
    ArtifactDigestMismatch {
        expected: String,
        actual: String,
    },
    ActorSchemaMismatch {
        actor: String,
    },
    InvalidActor {
        actor: String,
        message: String,
    },
}

impl fmt::Display for BehaviorManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid behavior manifest JSON: {message}"),
            Self::UnsupportedSchema(schema) => write!(
                f,
                "unsupported behavior manifest schema '{schema}'; expected {BEHAVIOR_MANIFEST_SCHEMA}"
            ),
            Self::UnsupportedArtifactKind(kind) => write!(
                f,
                "unsupported behavior manifest artifact kind '{kind}'; expected {BEHAVIOR_ARTIFACT_KIND_NBC_V1} or {BEHAVIOR_ARTIFACT_KIND_WASM_MODULE}"
            ),
            Self::UnsupportedArtifactBackend(backend) => write!(
                f,
                "unsupported behavior manifest artifact backend '{backend}'"
            ),
            Self::InvalidPackage(message) => write!(f, "invalid behavior manifest package: {message}"),
            Self::InvalidDigest { field, message } => {
                write!(f, "invalid {field} in behavior manifest: {message}")
            }
            Self::InvalidIdentity { field, message } => {
                write!(f, "invalid {field} in behavior manifest: {message}")
            }
            Self::DuplicateActor(actor) => {
                write!(f, "behavior manifest contains duplicate actor '{actor}'")
            }
            Self::ArtifactIdentityMismatch { expected, actual } => write!(
                f,
                "behavior manifest artifact identity mismatch: expected {expected}, got {actual}"
            ),
            Self::ArtifactDigestMismatch { expected, actual } => write!(
                f,
                "behavior manifest executable digest mismatch: expected {expected}, got {actual}"
            ),
            Self::ActorSchemaMismatch { actor } => {
                write!(f, "typed actor schema '{actor}' has no matching HIR actor")
            }
            Self::InvalidActor { actor, message } => {
                write!(f, "invalid behavior manifest actor '{actor}': {message}")
            }
        }
    }
}

impl std::error::Error for BehaviorManifestError {}

impl From<serde_json::Error> for BehaviorManifestError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

#[derive(Debug)]
pub enum BehaviorAdmissionError {
    InvalidPreviousManifest(BehaviorManifestError),
    InvalidIncomingManifest(BehaviorManifestError),
    PackageMismatch {
        previous: String,
        incoming: String,
    },
    DurableOwnerRemoved {
        actor: String,
    },
    DurabilityRemoved {
        actor: String,
        previous: BehaviorPersistence,
        incoming: BehaviorPersistence,
    },
    PersistenceClassChanged {
        actor: String,
        previous: BehaviorPersistence,
        incoming: BehaviorPersistence,
    },
    SchemaDowngrade {
        actor: String,
        previous: u32,
        incoming: u32,
    },
    UnversionedSchemaChange {
        actor: String,
        version: u32,
    },
    MissingMigrationStep {
        actor: String,
        from: u32,
        to: u32,
    },
    ExistingMigrationTopologyChanged {
        actor: String,
    },
}

impl fmt::Display for BehaviorAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPreviousManifest(error) => {
                write!(f, "previous behavior manifest is invalid: {error}")
            }
            Self::InvalidIncomingManifest(error) => {
                write!(f, "incoming behavior manifest is invalid: {error}")
            }
            Self::PackageMismatch { previous, incoming } => write!(
                f,
                "behavior manifest package mismatch: deployed '{previous}', incoming '{incoming}'"
            ),
            Self::DurableOwnerRemoved { actor } => write!(
                f,
                "incoming deployment removes durable state owner '{actor}'"
            ),
            Self::DurabilityRemoved {
                actor,
                previous,
                incoming,
            } => write!(
                f,
                "incoming deployment removes durability from '{actor}' ({previous:?} -> {incoming:?})"
            ),
            Self::PersistenceClassChanged {
                actor,
                previous,
                incoming,
            } => write!(
                f,
                "incoming deployment changes persistence class for '{actor}' ({previous:?} -> {incoming:?}) without a storage-model migration contract"
            ),
            Self::SchemaDowngrade {
                actor,
                previous,
                incoming,
            } => write!(
                f,
                "incoming deployment downgrades '{actor}' schema from {previous} to {incoming}"
            ),
            Self::UnversionedSchemaChange { actor, version } => write!(
                f,
                "incoming deployment changes '{actor}' state schema without bumping schema version {version}"
            ),
            Self::MissingMigrationStep { actor, from, to } => write!(
                f,
                "incoming deployment lacks migration {from} -> {to} required for durable actor '{actor}'"
            ),
            Self::ExistingMigrationTopologyChanged { actor } => write!(
                f,
                "incoming deployment changes an existing migration topology for durable actor '{actor}'"
            ),
        }
    }
}

impl std::error::Error for BehaviorAdmissionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Literal, MigrationDecl, StateModel};
    use crate::content_identity::SemanticId;
    use crate::hir::{ActorDef, Module, Operand};
    use crate::types::{PrimitiveType, Span, Type};

    fn artifact() -> ArtifactIdentityManifest {
        ArtifactIdentityManifest::new(
            Some(SourceId::from_bytes(b"entity Counter {}")),
            SemanticId::from_canonical_bytes(b"program", []),
            "nulang-rust-0.1.0",
            "nulang-bytecode-v1",
            "nulang-vm-v1",
            "bytecode",
            ["opt=0"],
        )
    }

    fn wasm_artifact() -> ArtifactIdentityManifest {
        ArtifactIdentityManifest::new(
            Some(SourceId::from_bytes(b"actor Counter {}")),
            SemanticId::from_canonical_bytes(b"program", []),
            "nulang-rust-0.1.0",
            "wasm32",
            "nulang-host-effects-v0alpha1",
            "wasm",
            ["opt=0"],
        )
    }

    fn typed_hir(version: u32) -> Module {
        let migrations = if version > 1 {
            vec![MigrationDecl {
                from_version: 1,
                to_version: 2,
                state_body: Some(Expr::Literal(Literal::Int(0), Span::default())),
                event_migrations: Vec::new(),
                span: Span::default(),
            }]
        } else {
            Vec::new()
        };

        Module {
            name: "test".to_string(),
            decls: vec![hir::Decl::Actor(ActorDef {
                name: "Counter".to_string(),
                type_params: Vec::new(),
                persistent: true,
                state_fields: vec![(
                    "count".to_string(),
                    StateModel::Durable,
                    Type::Primitive(PrimitiveType::Int),
                    Operand::Literal(Literal::Int(0), Type::Primitive(PrimitiveType::Int)),
                )],
                behaviors: Vec::new(),
                init: vec![(
                    "count".to_string(),
                    Operand::Literal(Literal::Int(0), Type::Primitive(PrimitiveType::Int)),
                )],
                events: Vec::new(),
                apply_handlers: Vec::new(),
                version,
                migrations,
                is_organization: false,
                is_workflow: false,
                is_agent: false,
                virtual_: false,
                tools: Vec::new(),
                semantic_memory_dimensions: None,
                procedural_memory_namespace: None,
                fallback_config: String::new(),
                retry_config: String::new(),
                span: Span::default(),
            })],
        }
    }

    fn schema_id(seed: &[u8]) -> String {
        SemanticId::from_canonical_bytes(seed, []).to_string()
    }

    fn base_manifest(actor: BehaviorActor) -> BehaviorManifest {
        BehaviorManifest {
            schema: BEHAVIOR_MANIFEST_SCHEMA.to_string(),
            package: BehaviorPackage {
                name: "payments".to_string(),
                version: "1.0.0".to_string(),
                language_version: LANGUAGE_VERSION_STR.to_string(),
            },
            artifact: BehaviorArtifact {
                kind: BEHAVIOR_ARTIFACT_KIND_NBC_V1.to_string(),
                digest: artifact_digest(b"artifact-bytes"),
                source_id: None,
                semantic_id: schema_id(b"program"),
                artifact_id: ArtifactId::from_semantic(
                    SemanticId::from_canonical_bytes(b"program", []),
                    "compiler",
                    "target",
                    "abi",
                    "bytecode",
                    std::iter::empty::<&str>(),
                )
                .to_string(),
                compiler_version: "compiler".to_string(),
                target: "target".to_string(),
                abi: "abi".to_string(),
                backend: "bytecode".to_string(),
                flags: Vec::new(),
            },
            actors: vec![actor],
        }
    }

    fn actor(version: u32, schema_seed: &[u8]) -> BehaviorActor {
        BehaviorActor {
            name: "payments::Account".to_string(),
            kind: BehaviorOwnerKind::Actor,
            persistence: BehaviorPersistence::Durable,
            schema_version: version,
            state_schema_semantic_id: schema_id(schema_seed),
            migration_identity: MigrationIdentityCoverage::TopologyOnly,
            migrations: if version == 1 {
                Vec::new()
            } else {
                (1..version)
                    .map(|from| BehaviorMigrationStep {
                        from,
                        to: from + 1,
                        state: true,
                        events: Vec::new(),
                    })
                    .collect()
            },
            workflow: None,
        }
    }

    #[test]
    fn typed_hir_binds_wasm_artifact_kind_to_exact_bytes() {
        let manifest = BehaviorManifest::from_typed_hir(
            "demo",
            "0.1.0",
            &wasm_artifact(),
            b"canonical-wasm-bytes",
            &typed_hir(1),
        )
        .unwrap();

        assert_eq!(manifest.artifact.kind, BEHAVIOR_ARTIFACT_KIND_WASM_MODULE);
        manifest
            .verify_artifact_bytes(b"canonical-wasm-bytes")
            .unwrap();
    }

    #[test]
    fn typed_hir_emits_ordered_workflow_step_metadata() {
        let mut module = typed_hir(1);
        let hir::Decl::Actor(actor) = &mut module.decls[0] else {
            panic!("expected actor");
        };
        actor.name = "ApprovalFlow".into();
        actor.is_workflow = true;
        actor.behaviors = vec![
            crate::hir::BehaviorDef {
                name: "reserve".into(),
                params: Vec::new(),
                ret: Type::unit(),
                effect: crate::types::EffectRow::empty(),
                cap: crate::types::Capability::Ref,
                body: crate::hir::Body::default(),
                compensate: Some(crate::hir::Body::default()),
                parallel_branches: None,
                span: Span::default(),
            },
            crate::hir::BehaviorDef {
                name: "parallel_0".into(),
                params: Vec::new(),
                ret: Type::unit(),
                effect: crate::types::EffectRow::empty(),
                cap: crate::types::Capability::Ref,
                body: crate::hir::Body::default(),
                compensate: None,
                parallel_branches: Some(vec!["email".into(), "sms".into()]),
                span: Span::default(),
            },
        ];

        let manifest = BehaviorManifest::from_typed_hir(
            "demo",
            "0.1.0",
            &artifact(),
            b"compiled-nbc",
            &module,
        )
        .unwrap();

        let owner = &manifest.actors[0];
        assert_eq!(owner.kind, BehaviorOwnerKind::Workflow);
        assert_eq!(
            owner.workflow,
            Some(BehaviorWorkflow {
                steps: vec![
                    BehaviorWorkflowStep {
                        name: "reserve".into(),
                        compensation: true,
                        parallel_branches: Vec::new(),
                    },
                    BehaviorWorkflowStep {
                        name: "parallel_0".into(),
                        compensation: false,
                        parallel_branches: vec!["email".into(), "sms".into()],
                    },
                ],
            })
        );
    }

    #[test]
    fn non_workflow_actor_omits_workflow_metadata() {
        let manifest = BehaviorManifest::from_typed_hir(
            "demo",
            "0.1.0",
            &artifact(),
            b"compiled-nbc",
            &typed_hir(1),
        )
        .unwrap();

        assert_eq!(manifest.actors[0].kind, BehaviorOwnerKind::Actor);
        assert_eq!(manifest.actors[0].workflow, None);
    }

    #[test]
    fn typed_hir_emits_durable_schema_and_migration_metadata() {
        let manifest = BehaviorManifest::from_typed_hir(
            "demo",
            "0.1.0",
            &artifact(),
            b"compiled-nbc",
            &typed_hir(2),
        )
        .unwrap();
        assert_eq!(manifest.actors.len(), 1);
        let actor = &manifest.actors[0];
        assert_eq!(actor.name, "test::Counter");
        assert_eq!(actor.persistence, BehaviorPersistence::Durable);
        assert_eq!(actor.schema_version, 2);
        assert_eq!(actor.migrations.len(), 1);
        assert_eq!(actor.migrations[0].from, 1);
        assert_eq!(actor.migrations[0].to, 2);
        assert!(actor.migrations[0].state);
    }

    #[test]
    fn canonical_json_and_digest_ignore_input_ordering_noise() {
        let mut first = base_manifest(actor(2, b"v2"));
        first.artifact.flags = vec!["z".to_string(), "a".to_string(), "z".to_string()];
        first.artifact.artifact_id = ArtifactId::from_semantic(
            SemanticId::from_canonical_bytes(b"program", []),
            "compiler",
            "target",
            "abi",
            "bytecode",
            first.artifact.flags.iter(),
        )
        .to_string();
        first.actors[0].migrations[0].events = vec![
            BehaviorMigrationEvent {
                name: "Zed".to_string(),
                arity: 0,
            },
            BehaviorMigrationEvent {
                name: "Alpha".to_string(),
                arity: 1,
            },
        ];

        let mut second = first.clone();
        second.artifact.flags.reverse();
        second.actors[0].migrations[0].events.reverse();

        assert_eq!(first.to_json().unwrap(), second.to_json().unwrap());
        assert_eq!(first.digest().unwrap(), second.digest().unwrap());
        assert_eq!(
            BehaviorManifest::from_json(&first.to_json().unwrap()).unwrap(),
            BehaviorManifest::from_json(&second.to_json().unwrap()).unwrap()
        );
    }

    #[test]
    fn same_version_schema_change_is_rejected() {
        let previous = base_manifest(actor(1, b"schema-a"));
        let incoming = base_manifest(actor(1, b"schema-b"));

        assert!(matches!(
            incoming.validate_upgrade_from(&previous),
            Err(BehaviorAdmissionError::UnversionedSchemaChange {
                actor: _,
                version: 1
            })
        ));
    }

    #[test]
    fn versioned_upgrade_with_complete_chain_is_admitted() {
        let previous = base_manifest(actor(1, b"schema-v1"));
        let incoming = base_manifest(actor(3, b"schema-v3"));
        incoming.validate_upgrade_from(&previous).unwrap();
    }

    #[test]
    fn downgrade_and_missing_durable_owner_are_rejected() {
        let previous = base_manifest(actor(2, b"schema-v2"));
        let incoming = base_manifest(actor(1, b"schema-v1"));
        assert!(matches!(
            incoming.validate_upgrade_from(&previous),
            Err(BehaviorAdmissionError::SchemaDowngrade { .. })
        ));

        let mut removed = base_manifest(actor(2, b"schema-v2"));
        removed.actors.clear();
        assert!(matches!(
            removed.validate_upgrade_from(&previous),
            Err(BehaviorAdmissionError::DurableOwnerRemoved { .. })
        ));
    }

    #[test]
    fn executable_digest_binds_sidecar_to_exact_bytes() {
        let manifest = base_manifest(actor(1, b"schema-v1"));
        manifest.verify_artifact_bytes(b"artifact-bytes").unwrap();
        assert!(matches!(
            manifest.verify_artifact_bytes(b"different-bytes"),
            Err(BehaviorManifestError::ArtifactDigestMismatch { .. })
        ));
    }

    #[test]
    fn durable_persistence_model_changes_fail_closed() {
        let previous = base_manifest(actor(1, b"schema-v1"));
        let mut incoming = previous.clone();
        incoming.actors[0].persistence = BehaviorPersistence::EventSourced;

        assert!(matches!(
            incoming.validate_upgrade_from(&previous),
            Err(BehaviorAdmissionError::PersistenceClassChanged { .. })
        ));
    }

    #[test]
    fn existing_migration_topology_cannot_be_rewritten_during_upgrade() {
        let previous = base_manifest(actor(2, b"schema-v2"));
        let mut incoming = base_manifest(actor(3, b"schema-v3"));
        incoming.actors[0].migrations[0].state = false;

        assert!(matches!(
            incoming.validate_upgrade_from(&previous),
            Err(BehaviorAdmissionError::ExistingMigrationTopologyChanged { .. })
        ));
    }

    #[test]
    fn tampered_artifact_binding_fails_closed() {
        let valid = base_manifest(actor(1, b"schema-v1"));
        let mut value: serde_json::Value =
            serde_json::from_slice(&valid.to_json().unwrap()).unwrap();
        value["artifact"]["target"] = serde_json::Value::from("different-target");
        let bytes = serde_json::to_vec(&value).unwrap();

        assert!(matches!(
            BehaviorManifest::from_json(&bytes),
            Err(BehaviorManifestError::ArtifactIdentityMismatch { .. })
        ));
    }

    #[test]
    fn malformed_or_unknown_manifests_fail_closed() {
        let valid = base_manifest(actor(1, b"schema-v1"));
        let mut value: serde_json::Value =
            serde_json::from_slice(&valid.to_json().unwrap()).unwrap();
        value["schema"] = serde_json::Value::from("nulang.behavior/v9");
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            BehaviorManifest::from_json(&bytes),
            Err(BehaviorManifestError::UnsupportedSchema(_))
        ));

        let mut value: serde_json::Value =
            serde_json::from_slice(&valid.to_json().unwrap()).unwrap();
        value["actors"][0]["schema_version"] = serde_json::Value::from(2);
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            BehaviorManifest::from_json(&bytes),
            Err(BehaviorManifestError::InvalidActor { .. })
        ));
    }
}
