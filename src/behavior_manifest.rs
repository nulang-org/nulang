//! Experimental compiler-emitted Behavior Manifest (RFC 0020).
//!
//! This module deliberately sits outside HIR/MIR stability. It consumes
//! already-checked compiler semantics and emits a versioned, deterministic
//! deployment contract. The manifest describes required behavior; it never
//! grants authority.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ast::{Decl, Expr, StateModel, WorkflowItem};
use crate::effect_checker::{flatten_decls, EffectChecker, EffectContext};
use crate::types::{
    canonical_type_bytes, Effect, EffectRow, NuError, NuResult, Type, RECORD_ROW_TAIL_FIELD,
};

pub const BEHAVIOR_MANIFEST_SCHEMA: &str = "nulang.behavior/v0alpha1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Bytecode,
    Native,
    WasmModule,
    WasmComponent,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompilerIdentity {
    pub implementation: String,
    pub version: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostAbiRequirement {
    /// Versioned compiler-owned host ABI required by this exact artifact.
    pub schema: String,
    /// Canonical operation identities sorted lexicographically.
    pub required_operations: Vec<String>,
    /// True when the artifact still contains an unresolved custom/legacy
    /// source-name dispatch. Canonical-only admission MUST reject such an
    /// artifact until an explicit versioned extension contract exists.
    pub requires_legacy_extension_dispatch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterfaceDecl {
    pub name: String,
    pub input: String,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorDecl {
    pub name: String,
    pub durability: PersistenceClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_schema: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersistenceClass {
    Transient,
    Durable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectDecl {
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
pub struct AuthorityDecl {
    pub kind: AuthorityKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
pub struct DurabilityDecl {
    pub owner: String,
    pub persistence: PersistenceClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_contract: Option<String>,
}

/// Compiler-derived durable-state inventory attached to a checked manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DurabilityInventory {
    pub actors: Vec<ActorDecl>,
    pub durability: Vec<DurabilityDecl>,
    pub state_schema_digests: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayDecl {
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
pub struct ResourceIntent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_min_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_weight: Option<u32>,
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
#[serde(deny_unknown_fields)]
pub struct BehaviorManifest {
    pub schema: String,
    pub package: PackageIdentity,
    pub artifact: ArtifactIdentity,
    pub compiler: CompilerIdentity,
    pub host_abi: HostAbiRequirement,
    pub interfaces: Vec<InterfaceDecl>,
    pub actors: Vec<ActorDecl>,
    pub effects: Vec<EffectDecl>,
    pub authority: Vec<AuthorityDecl>,
    pub durability: Vec<DurabilityDecl>,
    pub replay: Vec<ReplayDecl>,
    pub resources: ResourceIntent,
    pub provenance: Provenance,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestAdmissionError {
    InvalidJson(String),
    UnsupportedManifestSchema { found: String },
    ArtifactDigestMismatch { expected: String, actual: String },
    UnsupportedHostAbiSchema { found: String },
    NonCanonicalHostOperationInventory,
    UnknownHostOperation { canonical_id: String },
    LegacyExtensionDispatchRequired,
}

impl std::fmt::Display for ManifestAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson(error) => write!(f, "invalid Behavior Manifest JSON: {error}"),
            Self::UnsupportedManifestSchema { found } => write!(
                f,
                "unsupported Behavior Manifest schema '{found}' (expected '{BEHAVIOR_MANIFEST_SCHEMA}')"
            ),
            Self::ArtifactDigestMismatch { expected, actual } => write!(
                f,
                "Behavior Manifest artifact digest mismatch: manifest={expected}, artifact={actual}"
            ),
            Self::UnsupportedHostAbiSchema { found } => write!(
                f,
                "unsupported host-effect ABI schema '{found}' (expected '{}')",
                crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA
            ),
            Self::NonCanonicalHostOperationInventory => write!(
                f,
                "Behavior Manifest host operation inventory must be sorted and duplicate-free"
            ),
            Self::UnknownHostOperation { canonical_id } => write!(
                f,
                "Behavior Manifest requires unknown canonical host operation '{canonical_id}'"
            ),
            Self::LegacyExtensionDispatchRequired => write!(
                f,
                "artifact requires legacy/custom source-name host dispatch; canonical-only admission refuses it"
            ),
        }
    }
}

impl std::error::Error for ManifestAdmissionError {}

/// Inputs whose identity comes from the build/package layer rather than
/// language semantics.
pub struct ManifestBuildInput<'a> {
    pub package_name: &'a str,
    pub package_version: &'a str,
    pub language_version: &'a str,
    pub artifact_kind: ArtifactKind,
    pub artifact_bytes: &'a [u8],
    pub compiler_implementation: &'a str,
    pub compiler_version: &'a str,
    /// Exact compiler executable bytes (or another explicitly versioned
    /// compiler artifact). This is content-hashed, not synthesized from a
    /// version string.
    pub compiler_bytes: &'a [u8],
    pub host_abi: HostAbiRequirement,
    pub source_bytes: &'a [u8],
    /// Canonical dependency-lock bytes. An empty dependency set should pass
    /// the canonical empty lock representation, not ambient filesystem state.
    pub dependency_bytes: &'a [u8],
}

impl BehaviorManifest {
    /// Build v0alpha1 from the same checked HIR that produced the executable.
    ///
    /// This is the canonical integrated build path. Actor durability and state
    /// schema identities are derived from typed HIR rather than re-parsing
    /// source text or inspecting backend-specific artifact bytes.
    pub fn from_checked_module_with_hir(
        input: ManifestBuildInput<'_>,
        effect_checker: &mut EffectChecker,
        decls: &[Decl],
        hir: &crate::hir::Module,
    ) -> NuResult<Self> {
        let mut manifest = Self::from_checked_module(input, effect_checker, decls)?;
        let inventory = durability_inventory_from_hir(hir)?;
        manifest.actors = inventory.actors;
        manifest.durability = inventory.durability;
        manifest.provenance.state_schema_digests = inventory.state_schema_digests;
        Ok(manifest)
    }

    /// Build v0alpha1 from checked compiler semantics.
    ///
    /// `effect_checker.check_module(...)` must have succeeded before this is
    /// called. This method reuses the checker's inferred function rows and
    /// expression inference, so it never discovers effects by source-text
    /// scanning or emitted bytecode inspection.
    pub fn from_checked_module(
        input: ManifestBuildInput<'_>,
        effect_checker: &mut EffectChecker,
        decls: &[Decl],
    ) -> NuResult<Self> {
        let effects = collect_checked_effects(effect_checker, decls)?;
        let effect_decls: Vec<EffectDecl> = effects.iter().map(classify_effect).collect();
        let authority = authority_requirements(&effects);
        let mut host_abi = input.host_abi;
        host_abi.required_operations.sort();
        host_abi.required_operations.dedup();

        Ok(Self {
            schema: BEHAVIOR_MANIFEST_SCHEMA.to_string(),
            package: PackageIdentity {
                name: input.package_name.to_string(),
                version: input.package_version.to_string(),
                language_version: input.language_version.to_string(),
            },
            artifact: ArtifactIdentity {
                kind: input.artifact_kind,
                digest: digest(input.artifact_bytes),
            },
            compiler: CompilerIdentity {
                implementation: input.compiler_implementation.to_string(),
                version: input.compiler_version.to_string(),
                digest: digest(input.compiler_bytes),
            },
            host_abi,
            // Phase 0 intentionally emits no speculative contracts. These
            // arrays become non-empty only when their compiler source of truth
            // is authoritative and covered by conformance tests.
            interfaces: Vec::new(),
            actors: Vec::new(),
            effects: effect_decls,
            authority,
            durability: Vec::new(),
            replay: Vec::new(),
            resources: ResourceIntent::default(),
            provenance: Provenance {
                source_digest: digest(input.source_bytes),
                dependency_digest: digest(input.dependency_bytes),
                interface_digests: Vec::new(),
                state_schema_digests: Vec::new(),
            },
            extensions: BTreeMap::new(),
        })
    }

    /// Parse a manifest and enforce the canonical deployment contract against
    /// the exact executable artifact bytes supplied by the caller.
    ///
    /// This is intentionally stricter than raw serde deserialization: an
    /// artifact is not admissible merely because its JSON is structurally
    /// readable.
    pub fn parse_for_canonical_host(
        manifest_bytes: &[u8],
        artifact_bytes: &[u8],
    ) -> Result<Self, ManifestAdmissionError> {
        let manifest: Self = serde_json::from_slice(manifest_bytes)
            .map_err(|error| ManifestAdmissionError::InvalidJson(error.to_string()))?;
        manifest.validate_for_canonical_host(artifact_bytes)?;
        Ok(manifest)
    }

    /// Validate the enforcement-relevant artifact + host-ABI binding.
    pub fn validate_for_canonical_host(
        &self,
        artifact_bytes: &[u8],
    ) -> Result<(), ManifestAdmissionError> {
        if self.schema != BEHAVIOR_MANIFEST_SCHEMA {
            return Err(ManifestAdmissionError::UnsupportedManifestSchema {
                found: self.schema.clone(),
            });
        }

        let actual_digest = digest(artifact_bytes);
        if self.artifact.digest != actual_digest {
            return Err(ManifestAdmissionError::ArtifactDigestMismatch {
                expected: self.artifact.digest.clone(),
                actual: actual_digest,
            });
        }

        if self.host_abi.schema != crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA {
            return Err(ManifestAdmissionError::UnsupportedHostAbiSchema {
                found: self.host_abi.schema.clone(),
            });
        }

        let mut canonical = self.host_abi.required_operations.clone();
        canonical.sort();
        canonical.dedup();
        if canonical != self.host_abi.required_operations {
            return Err(ManifestAdmissionError::NonCanonicalHostOperationInventory);
        }

        for canonical_id in &self.host_abi.required_operations {
            let known = crate::host_effect_abi::HOST_OPERATIONS
                .iter()
                .any(|operation| operation.canonical_id() == *canonical_id);
            if !known {
                return Err(ManifestAdmissionError::UnknownHostOperation {
                    canonical_id: canonical_id.clone(),
                });
            }
        }

        if self.host_abi.requires_legacy_extension_dispatch {
            return Err(ManifestAdmissionError::LegacyExtensionDispatchRequired);
        }

        Ok(())
    }

    /// Deterministic UTF-8 JSON. Struct field order is fixed, maps are
    /// BTreeMaps, and effect/authority vectors are sorted by construction.
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }
}

pub fn digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

const STATE_SCHEMA_DOMAIN: &[u8] = b"nulang.state-schema/v0alpha1\0";

/// Derive deployment-visible actor durability and stable state schema identity
/// from checked HIR.
///
/// The digest describes recovery-relevant schema, not implementation bytes:
/// actor schema version, every non-local state field (name, persistence model,
/// canonical compiler type), and typed durable event payload declarations.
/// Local-only state is deliberately excluded because it is discarded on
/// recovery. Migration contracts remain absent until RFC 0008 has an actual
/// runtime trigger and versioned persistence semantics.
pub fn durability_inventory_from_hir(module: &crate::hir::Module) -> NuResult<DurabilityInventory> {
    let mut actors = Vec::new();
    let mut durability = Vec::new();
    let mut state_schema_digests = std::collections::BTreeSet::new();

    collect_hir_durability(
        &module.decls,
        &mut actors,
        &mut durability,
        &mut state_schema_digests,
    )?;

    actors.sort_by(|a, b| a.name.cmp(&b.name));
    durability.sort_by(|a, b| a.owner.cmp(&b.owner));

    Ok(DurabilityInventory {
        actors,
        durability,
        state_schema_digests: state_schema_digests.into_iter().collect(),
    })
}

fn collect_hir_durability(
    decls: &[crate::hir::Decl],
    actors: &mut Vec<ActorDecl>,
    durability: &mut Vec<DurabilityDecl>,
    state_schema_digests: &mut std::collections::BTreeSet<String>,
) -> NuResult<()> {
    for decl in decls {
        match decl {
            crate::hir::Decl::Actor(actor) => {
                let has_persistent_state = actor
                    .state_fields
                    .iter()
                    .any(|(_, model, _, _)| !matches!(model, StateModel::Local));
                // Explicit non-local state is a deployment durability
                // requirement even when the source omitted the persistent
                // modifier. This is conservative for the mixed legacy surface
                // where state models and the actor persistence flag remain
                // represented separately.
                let is_durable =
                    actor.persistent || has_persistent_state || !actor.events.is_empty();
                let persistence = if is_durable {
                    PersistenceClass::Durable
                } else {
                    PersistenceClass::Transient
                };
                let state_schema = if is_durable {
                    let schema = durable_state_schema_digest(actor)?;
                    state_schema_digests.insert(schema.clone());
                    Some(schema)
                } else {
                    None
                };

                actors.push(ActorDecl {
                    name: actor.name.clone(),
                    durability: persistence,
                    protocol: None,
                    state_schema: state_schema.clone(),
                });
                durability.push(DurabilityDecl {
                    owner: actor.name.clone(),
                    persistence,
                    schema: state_schema,
                    // RFC 0008 syntax exists, but runtime migration triggering
                    // remains inert. Emitting a migration contract digest here
                    // would falsely advertise an enforcement guarantee.
                    migration_contract: None,
                });
            }
            crate::hir::Decl::Module { decls, .. } => {
                collect_hir_durability(decls, actors, durability, state_schema_digests)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn durable_state_schema_digest(actor: &crate::hir::ActorDef) -> NuResult<String> {
    let mut bytes = STATE_SCHEMA_DOMAIN.to_vec();
    bytes.extend_from_slice(&actor.version.to_le_bytes());

    let mut fields: Vec<_> = actor
        .state_fields
        .iter()
        .filter(|(_, model, _, _)| !matches!(model, StateModel::Local))
        .collect();
    fields.sort_by(|a, b| a.0.cmp(&b.0));
    bytes.extend_from_slice(&(fields.len() as u32).to_le_bytes());

    for (name, model, ty, _) in fields {
        put_schema_str(&mut bytes, name);
        put_state_model(&mut bytes, *model);
        let runtime_ty = ty.erase_actor_protocols();
        ensure_closed_schema_type(
            &runtime_ty,
            &format!("state field '{}' on actor '{}'", name, actor.name),
            actor.span,
        )?;
        put_schema_bytes(&mut bytes, &canonical_type_bytes(&runtime_ty));
    }

    let mut events: Vec<_> = actor.events.iter().collect();
    events.sort_by(|a, b| a.name.cmp(&b.name));
    bytes.extend_from_slice(&(events.len() as u32).to_le_bytes());
    for event in events {
        put_schema_str(&mut bytes, &event.name);
        bytes.extend_from_slice(&(event.params.len() as u32).to_le_bytes());
        for (param_name, param_ty) in &event.params {
            put_schema_str(&mut bytes, param_name);
            let runtime_ty = param_ty.erase_actor_protocols();
            ensure_closed_schema_type(
                &runtime_ty,
                &format!(
                    "event '{}.{}' parameter '{}'",
                    actor.name, event.name, param_name
                ),
                event.span,
            )?;
            put_schema_bytes(&mut bytes, &canonical_type_bytes(&runtime_ty));
        }
    }

    Ok(digest(&bytes))
}

fn put_state_model(out: &mut Vec<u8>, model: StateModel) {
    match model {
        StateModel::Local => out.push(0),
        StateModel::Durable => out.push(1),
        StateModel::EventSourced => out.push(2),
        StateModel::Crdt(kind) => {
            out.push(3);
            out.push(kind.to_u8());
        }
    }
}

fn put_schema_str(out: &mut Vec<u8>, value: &str) {
    put_schema_bytes(out, value.as_bytes());
}

fn put_schema_bytes(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value);
}

/// Canonical type bytes include compiler variable/skolem IDs, so deployment
/// identities reject unresolved/open types instead of hashing process-local
/// identifiers.
fn ensure_closed_schema_type(ty: &Type, surface: &str, span: crate::types::Span) -> NuResult<()> {
    if schema_type_is_closed(ty) {
        Ok(())
    } else {
        Err(NuError::type_error(
            format!(
                "cannot emit deterministic durable state schema for {surface}: type {ty} is unresolved or open"
            ),
            span,
        ))
    }
}

fn schema_type_is_closed(ty: &Type) -> bool {
    match ty {
        Type::Var(_) | Type::Skolem(_) | Type::Scheme { .. } => false,
        Type::Primitive(_) => true,
        Type::Tuple(items) => items.iter().all(schema_type_is_closed),
        Type::Record(fields) => fields
            .iter()
            .all(|(name, ty)| name != RECORD_ROW_TAIL_FIELD && schema_type_is_closed(ty)),
        Type::Variant(cases) => cases.iter().all(|(_, payload)| {
            payload
                .as_ref()
                .map_or(true, |payload| schema_type_is_closed(payload))
        }),
        Type::Array(item) => schema_type_is_closed(item),
        Type::Function {
            param, ret, effect, ..
        } => {
            schema_type_is_closed(param)
                && schema_type_is_closed(ret)
                && matches!(effect, EffectRow::Closed(_))
        }
        Type::Actor { state, behavior } => {
            schema_type_is_closed(state) && schema_type_is_closed(behavior)
        }
        Type::App { constructor, args } => {
            schema_type_is_closed(constructor) && args.iter().all(schema_type_is_closed)
        }
        Type::Reference { inner, .. } => schema_type_is_closed(inner),
        Type::Nominal { underlying, .. } => schema_type_is_closed(underlying),
    }
}

/// Collect every effect that can be reached through an executable declaration
/// surface currently understood by the compiler.
///
/// Function effects come from the checker's fixed-point function rows. Other
/// expression-bearing declarations are inferred directly using those rows, so
/// transitive calls remain represented. The result is deduplicated and sorted
/// by the stable effect display name.
pub fn collect_checked_effects(
    checker: &mut EffectChecker,
    decls: &[Decl],
) -> NuResult<Vec<Effect>> {
    let mut effects: BTreeMap<String, Effect> = BTreeMap::new();

    for row in checker.function_rows().values() {
        add_row(&mut effects, row);
    }

    let ctx = EffectContext::empty();
    for decl in flatten_decls(decls) {
        collect_decl_effects(checker, &ctx, decl, &mut effects)?;
    }

    Ok(effects.into_values().collect())
}

fn collect_decl_effects(
    checker: &mut EffectChecker,
    ctx: &EffectContext,
    decl: &Decl,
    out: &mut BTreeMap<String, Effect>,
) -> NuResult<()> {
    match decl {
        Decl::Function { .. } => {
            // Already covered by the fixed-point function rows above.
        }
        Decl::Actor {
            behaviors,
            state_fields,
            init,
            initializer,
            apply_handlers,
            migrations,
            ..
        } => {
            for behavior in behaviors {
                add_expr(checker, ctx, &behavior.body, out)?;
            }
            for (_, _, _, default) in state_fields {
                add_expr(checker, ctx, default, out)?;
            }
            for (_, expr) in init {
                add_expr(checker, ctx, expr, out)?;
            }
            if let Some((_, _, body)) = initializer {
                add_expr(checker, ctx, body, out)?;
            }
            for handler in apply_handlers {
                add_expr(checker, ctx, &handler.body, out)?;
            }
            for migration in migrations {
                if let Some(body) = &migration.state_body {
                    add_expr(checker, ctx, body, out)?;
                }
                for (_, _, body) in &migration.event_migrations {
                    add_expr(checker, ctx, body, out)?;
                }
            }
        }
        Decl::StateMachine {
            name,
            states,
            events,
            entry_hooks,
            exit_hooks,
            span,
        } => {
            let actor = crate::ast::desugar_state_machine(
                name,
                states,
                events,
                entry_hooks,
                exit_hooks,
                *span,
            );
            collect_decl_effects(checker, ctx, &actor, out)?;
        }
        Decl::Workflow {
            items, compensate, ..
        } => {
            for item in items {
                let steps = match item {
                    WorkflowItem::Step(step) => std::slice::from_ref(step),
                    WorkflowItem::Parallel(steps) => steps.as_slice(),
                };
                for step in steps {
                    add_expr(checker, ctx, &step.body, out)?;
                    if let Some(comp) = &step.compensate {
                        add_expr(checker, ctx, comp, out)?;
                    }
                }
            }
            if let Some(comp) = compensate {
                add_expr(checker, ctx, comp, out)?;
            }
        }
        Decl::CrdtDecl { fields, .. } => {
            for (_, _, _, default) in fields {
                add_expr(checker, ctx, default, out)?;
            }
        }
        Decl::NamedHandler { handlers, .. } => {
            for handler in handlers {
                add_expr(checker, ctx, &handler.body, out)?;
            }
        }
        Decl::Class { methods, .. } => {
            for method in methods {
                if let Some(body) = &method.default_body {
                    add_expr(checker, ctx, body, out)?;
                }
            }
        }
        Decl::Impl { methods, .. } => {
            for method in methods {
                add_expr(checker, ctx, &method.body, out)?;
            }
        }
        Decl::LetBinding { value, .. } => add_expr(checker, ctx, value, out)?,
        Decl::Signal { init, .. } => add_expr(checker, ctx, init, out)?,
        Decl::Given { value, .. } => add_expr(checker, ctx, value, out)?,
        // Remaining declarations contain no executable expression body whose
        // runtime effects need to be admitted in v0alpha1.
        Decl::TypeAlias { .. }
        | Decl::RecordType { .. }
        | Decl::VariantType { .. }
        | Decl::EffectDecl { .. }
        | Decl::Module { .. }
        | Decl::Import { .. }
        | Decl::Extern { .. }
        | Decl::Agent { .. }
        | Decl::Database { .. } => {}
    }
    Ok(())
}

fn add_expr(
    checker: &mut EffectChecker,
    ctx: &EffectContext,
    expr: &Expr,
    out: &mut BTreeMap<String, Effect>,
) -> NuResult<()> {
    let row = checker.infer_effects(ctx, expr)?;
    add_row(out, &row);
    Ok(())
}

fn add_row(out: &mut BTreeMap<String, Effect>, row: &crate::types::EffectRow) {
    for effect in row.effects() {
        out.entry(effect.to_string())
            .or_insert_with(|| effect.clone());
    }
}

fn classify_effect(effect: &Effect) -> EffectDecl {
    use Effect::*;

    let external = matches!(
        effect,
        IO | Net
            | FS
            | Rand
            | Time
            | Inference
            | FFI
            | DB
            | Python
            | Env
            | Process
            | System
            | Request
            | Respond
            | Realtime
            | Client
            | Web
            | UserDefined(_)
    );
    let nondeterministic = matches!(
        effect,
        IO | Net
            | FS
            | Rand
            | Time
            | Inference
            | FFI
            | DB
            | Python
            | Env
            | Process
            | System
            | Request
            | Realtime
            | Client
            | Web
            | UserDefined(_)
    );

    let replay =
        match effect {
            String | Array | Spawn | Send | Receive | Migrate | STM | Async | Cost | Event
            | Render => EffectReplay::Safe,
            Inference | Time | Rand => EffectReplay::RequiresJournal,
            Net | FS | DB | FFI | Python | Process | System | Respond | Realtime
            | UserDefined(_) => EffectReplay::RequiresIdempotencyKey,
            IO | Env | Request | Client | Web => EffectReplay::Nonreplayable,
            Test => EffectReplay::Safe,
        };

    EffectDecl {
        effect: effect.to_string(),
        class: if external {
            EffectClass::External
        } else {
            EffectClass::Local
        },
        determinism: if nondeterministic {
            Determinism::Nondeterministic
        } else {
            Determinism::Deterministic
        },
        replay,
        cost_class: match effect {
            Inference => Some("inference".to_string()),
            Net => Some("network".to_string()),
            DB => Some("storage".to_string()),
            _ => None,
        },
    }
}

fn authority_requirements(effects: &[Effect]) -> Vec<AuthorityDecl> {
    use Effect::*;

    let mut required: BTreeMap<std::string::String, AuthorityDecl> = BTreeMap::new();
    for effect in effects {
        let decl = match effect {
            FS => Some(AuthorityDecl {
                kind: AuthorityKind::Filesystem,
                resource: Some("*".to_string()),
                operations: vec!["read".to_string(), "write".to_string()],
                required: true,
            }),
            Net => Some(AuthorityDecl {
                kind: AuthorityKind::Network,
                resource: Some("*".to_string()),
                operations: vec!["connect".to_string()],
                required: true,
            }),
            Inference => Some(AuthorityDecl {
                kind: AuthorityKind::Inference,
                resource: Some("*".to_string()),
                operations: vec!["invoke".to_string()],
                required: true,
            }),
            DB => Some(AuthorityDecl {
                kind: AuthorityKind::State,
                resource: Some("*".to_string()),
                operations: vec!["read".to_string(), "write".to_string()],
                required: true,
            }),
            Env | Process | System | FFI | Python => Some(AuthorityDecl {
                kind: AuthorityKind::Custom,
                resource: Some("os".to_string()),
                operations: vec![effect.to_string()],
                required: true,
            }),
            UserDefined(name) => Some(AuthorityDecl {
                kind: AuthorityKind::Custom,
                resource: Some(name.clone()),
                operations: vec!["perform".to_string()],
                required: true,
            }),
            _ => None,
        };
        if let Some(mut decl) = decl {
            decl.operations.sort();
            let key = format!(
                "{:?}|{}|{}",
                decl.kind,
                decl.resource.as_deref().unwrap_or(""),
                decl.operations.join(",")
            );
            required.entry(key).or_insert(decl);
        }
    }
    required.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect_checker::EffectChecker;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn checked(source: &str) -> (crate::ast::AstModule, EffectChecker) {
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");
        let mut checker = EffectChecker::new();
        checker.check_module(&ast.decls).expect("effect check");
        (ast, checker)
    }

    fn checked_hir(source: &str) -> crate::hir::Module {
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");
        let mut checker = crate::typechecker::TypeChecker::new();
        checker.check_module(&ast).expect("typecheck");
        crate::hir_lower::lower_module(&ast, &checker.inferred_decl_types)
    }

    fn actor_schema(inventory: &DurabilityInventory, name: &str) -> Option<String> {
        inventory
            .actors
            .iter()
            .find(|actor| actor.name == name)
            .and_then(|actor| actor.state_schema.clone())
    }

    #[test]
    fn durability_inventory_marks_transient_and_durable_actors() {
        let hir = checked_hir(
            r#"
            actor Ephemeral {
                state local scratch: Int = 0
                behavior get() { self.scratch }
            }

            persistent actor Counter {
                state durable count: Int = 0
                behavior get() { self.count }
            }
            "#,
        );
        let inventory = durability_inventory_from_hir(&hir).expect("inventory");

        let ephemeral = inventory
            .actors
            .iter()
            .find(|actor| actor.name == "Ephemeral")
            .unwrap();
        assert_eq!(ephemeral.durability, PersistenceClass::Transient);
        assert_eq!(ephemeral.state_schema, None);

        let counter = inventory
            .actors
            .iter()
            .find(|actor| actor.name == "Counter")
            .unwrap();
        assert_eq!(counter.durability, PersistenceClass::Durable);
        assert!(counter.state_schema.is_some());
        assert_eq!(inventory.state_schema_digests.len(), 1);
    }

    #[test]
    fn durable_schema_is_independent_of_state_declaration_order_and_local_state() {
        let first = checked_hir(
            r#"
            persistent actor Counter {
                state durable count: Int = 0
                state durable label: String = "x"
                state local scratch: Int = 0
                behavior get() { self.count }
            }
            "#,
        );
        let reordered = checked_hir(
            r#"
            persistent actor Counter {
                state local scratch: String = "ignored"
                state durable label: String = "different-default"
                state durable count: Int = 99
                behavior get() { self.count }
            }
            "#,
        );

        let first = durability_inventory_from_hir(&first).unwrap();
        let reordered = durability_inventory_from_hir(&reordered).unwrap();
        assert_eq!(
            actor_schema(&first, "Counter"),
            actor_schema(&reordered, "Counter")
        );
    }

    #[test]
    fn durable_schema_changes_with_persistence_model_or_field_type() {
        let base = checked_hir(
            r#"
            persistent actor Counter {
                state durable value: Int = 0
                behavior get() { self.value }
            }
            "#,
        );
        let event_sourced = checked_hir(
            r#"
            persistent actor Counter {
                state event_sourced value: Int = 0
                behavior get() { self.value }
            }
            "#,
        );
        let different_type = checked_hir(
            r#"
            persistent actor Counter {
                state durable value: String = "0"
                behavior get() { self.value }
            }
            "#,
        );

        let base = durability_inventory_from_hir(&base).unwrap();
        let event_sourced = durability_inventory_from_hir(&event_sourced).unwrap();
        let different_type = durability_inventory_from_hir(&different_type).unwrap();

        assert_ne!(
            actor_schema(&base, "Counter"),
            actor_schema(&event_sourced, "Counter")
        );
        assert_ne!(
            actor_schema(&base, "Counter"),
            actor_schema(&different_type, "Counter")
        );
    }

    #[test]
    fn durable_schema_includes_entity_event_payload_contract() {
        let int_event = checked_hir(
            r#"
            entity Counter {
                state count: Int = 0
                events
                    | Incremented(by: Int)
                behavior get() { self.count }
            }
            "#,
        );
        let string_event = checked_hir(
            r#"
            entity Counter {
                state count: Int = 0
                events
                    | Incremented(by: String)
                behavior get() { self.count }
            }
            "#,
        );

        let int_event = durability_inventory_from_hir(&int_event).unwrap();
        let string_event = durability_inventory_from_hir(&string_event).unwrap();
        assert_ne!(
            actor_schema(&int_event, "Counter"),
            actor_schema(&string_event, "Counter")
        );
    }

    #[test]
    fn durability_inventory_does_not_claim_inert_migration_contracts() {
        let hir = checked_hir(
            r#"
            entity Counter {
                version: 2
                state count: Int = 0
                behavior get() { self.count }
                migration from 1 to 2 {
                    state => { self.count = 1 }
                }
            }
            "#,
        );
        let inventory = durability_inventory_from_hir(&hir).unwrap();
        let durability = inventory
            .durability
            .iter()
            .find(|entry| entry.owner == "Counter")
            .unwrap();
        assert_eq!(durability.persistence, PersistenceClass::Durable);
        assert!(durability.schema.is_some());
        assert_eq!(durability.migration_contract, None);
    }

    #[test]
    fn effect_inventory_is_semantic_sorted_and_deduplicated() {
        let (ast, mut checker) = checked(
            r#"
            fn helper() { perform Http.get("https://example.com") }
            fn main() {
                helper()
                perform FS.read("/tmp/a")
                helper()
            }
            "#,
        );
        let effects = collect_checked_effects(&mut checker, &ast.decls).expect("effects");
        let names: Vec<_> = effects.iter().map(ToString::to_string).collect();
        assert_eq!(names, vec!["FS", "Net"]);
    }

    #[test]
    fn manifest_binds_exact_artifact_and_compiler_bytes() {
        let (ast, mut checker) = checked("fn main() { perform IO.print(\"hi\") }");
        let manifest = BehaviorManifest::from_checked_module(
            ManifestBuildInput {
                package_name: "demo",
                package_version: "0.1.0",
                language_version: "1.0.0-frozen",
                artifact_kind: ArtifactKind::WasmModule,
                artifact_bytes: b"wasm-bytes",
                compiler_implementation: "nulang-rust",
                compiler_version: "0.1.0",
                compiler_bytes: b"compiler-bytes",
                host_abi: HostAbiRequirement {
                    schema: crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA.to_string(),
                    required_operations: Vec::new(),
                    requires_legacy_extension_dispatch: false,
                },
                source_bytes: b"fn main() { perform IO.print(\"hi\") }",
                dependency_bytes: b"",
            },
            &mut checker,
            &ast.decls,
        )
        .expect("manifest");

        assert_eq!(manifest.artifact.digest, digest(b"wasm-bytes"));
        assert_eq!(manifest.compiler.digest, digest(b"compiler-bytes"));
        assert_eq!(manifest.effects.len(), 1);
        assert_eq!(manifest.effects[0].effect, "IO");
    }

    #[test]
    fn canonical_json_is_stable_for_identical_inputs() {
        let (ast, mut checker_a) = checked("fn main() { perform Http.get(\"x\") }");
        let (_, mut checker_b) = checked("fn main() { perform Http.get(\"x\") }");
        let input = || ManifestBuildInput {
            package_name: "demo",
            package_version: "0.1.0",
            language_version: "1.0.0-frozen",
            artifact_kind: ArtifactKind::WasmModule,
            artifact_bytes: b"artifact",
            compiler_implementation: "nulang-rust",
            compiler_version: "0.1.0",
            compiler_bytes: b"compiler",
            host_abi: HostAbiRequirement {
                schema: crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA.to_string(),
                required_operations: Vec::new(),
                requires_legacy_extension_dispatch: false,
            },
            source_bytes: b"source",
            dependency_bytes: b"lock",
        };
        let a = BehaviorManifest::from_checked_module(input(), &mut checker_a, &ast.decls)
            .unwrap()
            .to_canonical_json()
            .unwrap();
        let b = BehaviorManifest::from_checked_module(input(), &mut checker_b, &ast.decls)
            .unwrap()
            .to_canonical_json()
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_host_validation_binds_exact_artifact_and_host_abi() {
        let artifact = b"wasm-artifact";
        let mut checker = EffectChecker::new();
        let manifest = BehaviorManifest::from_checked_module(
            ManifestBuildInput {
                package_name: "admission",
                package_version: "0.1.0",
                language_version: "1.0.0-frozen",
                artifact_kind: ArtifactKind::WasmModule,
                artifact_bytes: artifact,
                compiler_implementation: "nulang-rust-test",
                compiler_version: "test",
                compiler_bytes: b"compiler",
                host_abi: HostAbiRequirement {
                    schema: crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA.to_string(),
                    required_operations: vec![
                        "nulang.host-effects/v0alpha1:nulang:storage/string#Write".to_string(),
                    ],
                    requires_legacy_extension_dispatch: false,
                },
                source_bytes: b"source",
                dependency_bytes: b"lock",
            },
            &mut checker,
            &[],
        )
        .unwrap();

        let json = manifest.to_canonical_json().unwrap();
        let parsed = BehaviorManifest::parse_for_canonical_host(&json, artifact).unwrap();
        assert_eq!(parsed, manifest);
    }

    #[test]
    fn canonical_host_validation_fails_closed_on_binding_or_abi_mismatch() {
        let artifact = b"wasm-artifact";
        let mut checker = EffectChecker::new();
        let base = BehaviorManifest::from_checked_module(
            ManifestBuildInput {
                package_name: "admission",
                package_version: "0.1.0",
                language_version: "1.0.0-frozen",
                artifact_kind: ArtifactKind::WasmModule,
                artifact_bytes: artifact,
                compiler_implementation: "nulang-rust-test",
                compiler_version: "test",
                compiler_bytes: b"compiler",
                host_abi: HostAbiRequirement {
                    schema: crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA.to_string(),
                    required_operations: vec![
                        "nulang.host-effects/v0alpha1:nulang:storage/string#Write".to_string(),
                    ],
                    requires_legacy_extension_dispatch: false,
                },
                source_bytes: b"source",
                dependency_bytes: b"lock",
            },
            &mut checker,
            &[],
        )
        .unwrap();

        assert!(matches!(
            base.validate_for_canonical_host(b"different-artifact"),
            Err(ManifestAdmissionError::ArtifactDigestMismatch { .. })
        ));

        let mut wrong_schema = base.clone();
        wrong_schema.schema = "nulang.behavior/v999".into();
        assert!(matches!(
            wrong_schema.validate_for_canonical_host(artifact),
            Err(ManifestAdmissionError::UnsupportedManifestSchema { .. })
        ));

        let mut wrong_abi = base.clone();
        wrong_abi.host_abi.schema = "nulang.host-effects/v999".into();
        assert!(matches!(
            wrong_abi.validate_for_canonical_host(artifact),
            Err(ManifestAdmissionError::UnsupportedHostAbiSchema { .. })
        ));

        let mut unknown = base.clone();
        unknown.host_abi.required_operations =
            vec!["nulang.host-effects/v0alpha1:nulang:unknown/op#run".into()];
        assert!(matches!(
            unknown.validate_for_canonical_host(artifact),
            Err(ManifestAdmissionError::UnknownHostOperation { .. })
        ));

        let mut legacy = base.clone();
        legacy.host_abi.requires_legacy_extension_dispatch = true;
        assert!(matches!(
            legacy.validate_for_canonical_host(artifact),
            Err(ManifestAdmissionError::LegacyExtensionDispatchRequired)
        ));
    }

    #[test]
    fn canonical_host_validation_rejects_ambiguous_operation_inventory() {
        let artifact = b"wasm-artifact";
        let operation = "nulang.host-effects/v0alpha1:nulang:storage/string#Write".to_string();
        let mut checker = EffectChecker::new();
        let mut manifest = BehaviorManifest::from_checked_module(
            ManifestBuildInput {
                package_name: "admission",
                package_version: "0.1.0",
                language_version: "1.0.0-frozen",
                artifact_kind: ArtifactKind::WasmModule,
                artifact_bytes: artifact,
                compiler_implementation: "nulang-rust-test",
                compiler_version: "test",
                compiler_bytes: b"compiler",
                host_abi: HostAbiRequirement {
                    schema: crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA.to_string(),
                    required_operations: vec![operation.clone()],
                    requires_legacy_extension_dispatch: false,
                },
                source_bytes: b"source",
                dependency_bytes: b"lock",
            },
            &mut checker,
            &[],
        )
        .unwrap();

        manifest.host_abi.required_operations = vec![operation.clone(), operation];
        assert!(matches!(
            manifest.validate_for_canonical_host(artifact),
            Err(ManifestAdmissionError::NonCanonicalHostOperationInventory)
        ));
    }
}
