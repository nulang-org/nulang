//! Compiler-side Nulang Behavior Manifest v0alpha1 emission.
//!
//! The manifest is a semantic summary for deployment/admission systems. It is
//! derived from already-checked compiler state and never grants authority.

use crate::authority::AuthorityGrant;
use crate::effect_checker::EffectChecker;
use crate::hir;
use crate::mir;
use crate::protocol::{ProtocolMember, ProtocolSchema};
use crate::types::{canonical_type_bytes, Effect, EffectRow, Type};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

pub const BEHAVIOR_SCHEMA_V0ALPHA1: &str = "nulang.behavior/v0alpha1";
const STATE_SCHEMA_DOMAIN: &[u8] = b"nulang.behavior.state-schema.v0alpha1\0";
const INTERFACE_DOMAIN: &[u8] = b"nulang.behavior.interface.v0alpha1\0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BehaviorManifest {
    pub schema: String,
    pub package: PackageIdentity,
    pub artifact: ArtifactIdentity,
    pub compiler: CompilerIdentity,
    pub interfaces: Vec<InterfaceEntry>,
    pub actors: Vec<ActorEntry>,
    pub effects: Vec<EffectEntry>,
    pub authority: Vec<AuthorityEntry>,
    pub durability: Vec<DurabilityEntry>,
    pub replay: Vec<ReplayEntry>,
    pub resources: Resources,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageIdentity {
    pub name: String,
    pub version: String,
    pub language_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentity {
    pub kind: ArtifactKind,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Bytecode,
    Native,
    WasmModule,
    WasmComponent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompilerIdentity {
    pub implementation: String,
    pub version: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InterfaceEntry {
    pub name: String,
    pub input: String,
    pub output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActorEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub durability: PersistenceClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_schema: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PersistenceClass {
    Transient,
    Durable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EffectEntry {
    pub effect: String,
    pub class: EffectClass,
    pub determinism: Determinism,
    pub replay: EffectReplay,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_class: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EffectClass {
    Local,
    External,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Determinism {
    Deterministic,
    Nondeterministic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EffectReplay {
    Safe,
    RequiresJournal,
    Nonreplayable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorityEntry {
    pub kind: AuthorityKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DurabilityEntry {
    pub owner: String,
    pub persistence: PersistenceClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_contract: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReplayEntry {
    pub effect: String,
    pub class: ReplayClass,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReplayClass {
    LocalReplaySafe,
    JournalResult,
    ExternalNonreplayable,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_min_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_weight: Option<u64>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub source_digest: String,
    pub dependency_digest: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interface_digests: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state_schema_digests: Vec<String>,
}

#[derive(Clone)]
pub struct BehaviorManifestInput<'a> {
    pub package_name: &'a str,
    pub package_version: &'a str,
    pub language_version: &'a str,
    pub artifact_kind: ArtifactKind,
    pub artifact_bytes: &'a [u8],
    pub compiler_digest: &'a str,
    pub source_digest: &'a str,
    pub dependency_digest: &'a str,
    pub effect_checker: &'a EffectChecker,
    /// Checked HIR is the source of typed public interfaces, actor protocols,
    /// durability, and state schema identity.
    pub hir: Option<&'a hir::Module>,
    /// MIR is used only for semantic facts that HIR intentionally lowers away,
    /// currently exact spawn-site delegated authority.
    pub mir: Option<&'a mir::Module>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BehaviorManifestBuildError {
    Protocol(String),
    Authority(String),
    Serialization(String),
}

impl fmt::Display for BehaviorManifestBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(message) => write!(f, "cannot derive actor protocol: {message}"),
            Self::Authority(message) => write!(f, "cannot derive external authority: {message}"),
            Self::Serialization(message) => {
                write!(f, "cannot derive behavior manifest identity: {message}")
            }
        }
    }
}

impl std::error::Error for BehaviorManifestBuildError {}

impl BehaviorManifest {
    pub fn from_checked_program(
        input: BehaviorManifestInput<'_>,
    ) -> Result<Self, BehaviorManifestBuildError> {
        let effect_set = collect_effect_set(input.effect_checker, input.hir);
        let effects: Vec<_> = effect_set.iter().cloned().map(effect_entry).collect();
        let replay = effects
            .iter()
            .map(|entry| ReplayEntry {
                effect: entry.effect.clone(),
                class: match entry.replay {
                    EffectReplay::Safe => ReplayClass::LocalReplaySafe,
                    EffectReplay::RequiresJournal => ReplayClass::JournalResult,
                    EffectReplay::Nonreplayable => ReplayClass::ExternalNonreplayable,
                },
            })
            .collect();

        let (interfaces, actors, durability, interface_digests, state_schema_digests) =
            if let Some(hir) = input.hir {
                collect_hir_contract(hir)?
            } else {
                (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new())
            };

        let authority = collect_authority(&effect_set, input.mir)?;

        Ok(Self {
            schema: BEHAVIOR_SCHEMA_V0ALPHA1.to_string(),
            package: PackageIdentity {
                name: input.package_name.to_string(),
                version: input.package_version.to_string(),
                language_version: input.language_version.to_string(),
            },
            artifact: ArtifactIdentity {
                kind: input.artifact_kind,
                digest: format!("blake3:{}", blake3::hash(input.artifact_bytes).to_hex()),
            },
            compiler: CompilerIdentity {
                implementation: "nulang-rust".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                digest: input.compiler_digest.to_string(),
            },
            interfaces,
            actors,
            effects,
            authority,
            durability,
            replay,
            resources: Resources::default(),
            provenance: Provenance {
                source_digest: input.source_digest.to_string(),
                dependency_digest: input.dependency_digest.to_string(),
                interface_digests,
                state_schema_digests,
            },
        })
    }

    pub fn to_pretty_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}

fn collect_effect_set(checker: &EffectChecker, hir: Option<&hir::Module>) -> BTreeSet<Effect> {
    let mut unique = BTreeSet::new();

    for row in checker.function_rows().values() {
        extend_effect_row(&mut unique, row);
    }
    if let Some(hir) = hir {
        collect_hir_effects(&hir.decls, &mut unique);
    }

    unique
}

fn collect_hir_effects(decls: &[hir::Decl], out: &mut BTreeSet<Effect>) {
    for decl in decls {
        match decl {
            hir::Decl::Function(function) => extend_effect_row(out, &function.effect),
            hir::Decl::Actor(actor) => {
                for behavior in &actor.behaviors {
                    extend_effect_row(out, &behavior.effect);
                }
            }
            hir::Decl::Module { decls, .. } => collect_hir_effects(decls, out),
            _ => {}
        }
    }
}

fn extend_effect_row(out: &mut BTreeSet<Effect>, row: &EffectRow) {
    match row {
        EffectRow::Closed(effects) | EffectRow::Open(effects, _) => {
            out.extend(effects.iter().cloned())
        }
    }
}

fn effect_entry(effect: Effect) -> EffectEntry {
    let external = is_external_effect(&effect);
    let nonreplayable = matches!(effect, Effect::FFI | Effect::Process);
    let nondeterministic = external || matches!(effect, Effect::Rand | Effect::Time);

    EffectEntry {
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
        replay: if nonreplayable {
            EffectReplay::Nonreplayable
        } else if external {
            EffectReplay::RequiresJournal
        } else {
            EffectReplay::Safe
        },
        cost_class: None,
    }
}

fn is_external_effect(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::Net
            | Effect::FS
            | Effect::Inference
            | Effect::FFI
            | Effect::DB
            | Effect::Python
            | Effect::Env
            | Effect::Process
            | Effect::System
    )
}

type HirContract = (
    Vec<InterfaceEntry>,
    Vec<ActorEntry>,
    Vec<DurabilityEntry>,
    Vec<String>,
    Vec<String>,
);

fn collect_hir_contract(module: &hir::Module) -> Result<HirContract, BehaviorManifestBuildError> {
    let mut interfaces = Vec::new();
    let mut actors = Vec::new();
    let mut durability = Vec::new();
    let mut interface_digests = BTreeSet::new();
    let mut state_schema_digests = BTreeSet::new();

    collect_hir_decls(
        &module.decls,
        &mut interfaces,
        &mut actors,
        &mut durability,
        &mut interface_digests,
        &mut state_schema_digests,
    )?;

    interfaces.sort_by(|a, b| a.name.cmp(&b.name));
    actors.sort_by(|a, b| a.name.cmp(&b.name));
    durability.sort_by(|a, b| a.owner.cmp(&b.owner));

    Ok((
        interfaces,
        actors,
        durability,
        interface_digests.into_iter().collect(),
        state_schema_digests.into_iter().collect(),
    ))
}

fn collect_hir_decls(
    decls: &[hir::Decl],
    interfaces: &mut Vec<InterfaceEntry>,
    actors: &mut Vec<ActorEntry>,
    durability: &mut Vec<DurabilityEntry>,
    interface_digests: &mut BTreeSet<String>,
    state_schema_digests: &mut BTreeSet<String>,
) -> Result<(), BehaviorManifestBuildError> {
    for decl in decls {
        match decl {
            hir::Decl::Function(function) if function.public => {
                let input_ty =
                    Type::Tuple(function.params.iter().map(|(_, ty)| ty.clone()).collect());
                let signature = Type::Function {
                    param: Box::new(input_ty.clone()),
                    ret: Box::new(function.ret.clone()),
                    effect: function.effect.clone(),
                    cap: function.cap,
                };
                let digest = interface_digest(&function.name, &signature);
                interface_digests.insert(digest.clone());
                interfaces.push(InterfaceEntry {
                    name: function.name.clone(),
                    input: input_ty.to_string(),
                    output: function.ret.to_string(),
                    contract_digest: Some(digest),
                });
            }
            hir::Decl::Actor(actor) => {
                let members = actor
                    .behaviors
                    .iter()
                    .map(|behavior| {
                        ProtocolMember::behavior(
                            behavior.name.clone(),
                            behavior.params.iter().map(|(_, ty)| ty.clone()).collect(),
                            behavior.ret.clone(),
                            behavior.effect.clone(),
                            behavior.cap,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| BehaviorManifestBuildError::Protocol(error.to_string()))?;
                let protocol = ProtocolSchema::new(actor.name.clone(), members)
                    .map_err(|error| BehaviorManifestBuildError::Protocol(error.to_string()))?;
                let protocol_id = format!("blake3:{}", protocol.id());

                let is_durable = actor.persistent
                    || actor
                        .state_fields
                        .iter()
                        .any(|(_, model, _, _)| !matches!(model, crate::ast::StateModel::Local));
                let persistence = if is_durable {
                    PersistenceClass::Durable
                } else {
                    PersistenceClass::Transient
                };

                let state_schema = if is_durable {
                    let digest = state_schema_digest(actor)?;
                    state_schema_digests.insert(digest.clone());
                    Some(digest)
                } else {
                    None
                };

                actors.push(ActorEntry {
                    name: actor.name.clone(),
                    protocol: Some(protocol_id),
                    durability: persistence,
                    state_schema: state_schema.clone(),
                });

                if is_durable {
                    durability.push(DurabilityEntry {
                        owner: actor.name.clone(),
                        persistence: PersistenceClass::Durable,
                        schema: state_schema,
                        // RFC 0008 migrations are not runtime-enforced yet. Do not
                        // publish a migration contract the runtime cannot honor.
                        migration_contract: None,
                    });
                }
            }
            hir::Decl::Module { decls, .. } => {
                collect_hir_decls(
                    decls,
                    interfaces,
                    actors,
                    durability,
                    interface_digests,
                    state_schema_digests,
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn interface_digest(name: &str, signature: &Type) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(INTERFACE_DOMAIN);
    put_bytes(&mut hasher, name.as_bytes());
    put_bytes(&mut hasher, &canonical_type_bytes(signature));
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn state_schema_digest(actor: &hir::ActorDef) -> Result<String, BehaviorManifestBuildError> {
    let mut fields: Vec<_> = actor.state_fields.iter().collect();
    fields.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(STATE_SCHEMA_DOMAIN);
    hasher.update(&actor.version.to_le_bytes());
    put_u32(&mut hasher, fields.len() as u32);

    for (name, model, ty, _) in fields {
        put_bytes(&mut hasher, name.as_bytes());
        let model_bytes = serde_json::to_vec(model)
            .map_err(|error| BehaviorManifestBuildError::Serialization(error.to_string()))?;
        put_bytes(&mut hasher, &model_bytes);
        put_bytes(&mut hasher, &canonical_type_bytes(ty));
    }

    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

fn collect_authority(
    effects: &BTreeSet<Effect>,
    mir: Option<&mir::Module>,
) -> Result<Vec<AuthorityEntry>, BehaviorManifestBuildError> {
    let mut authority: BTreeMap<(AuthorityKind, Option<String>), BTreeSet<String>> =
        BTreeMap::new();

    // Effect rows establish required authority shape even when the exact
    // runtime resource is data-dependent and cannot be proven statically.
    for effect in effects {
        if let Some((kind, resource)) = broad_authority_for_effect(effect) {
            authority.entry((kind, resource)).or_default();
        }
    }

    // Spawn grants are exact, compiler-preserved authority. Keep the exact
    // resource/operation instead of widening them to their broad effect class.
    if let Some(mir) = mir {
        for token in spawn_authority_tokens(mir) {
            let grant = AuthorityGrant::from_str(&token).map_err(|error| {
                BehaviorManifestBuildError::Authority(format!("{token}: {error}"))
            })?;
            let (kind, resource, operation) = authority_entry_for_grant(&grant);
            let operations = authority.entry((kind, resource)).or_default();
            if let Some(operation) = operation {
                operations.insert(operation);
            }
        }
    }

    Ok(authority
        .into_iter()
        .map(|((kind, resource), operations)| AuthorityEntry {
            kind,
            resource,
            operations: operations.into_iter().collect(),
            required: true,
        })
        .collect())
}

fn broad_authority_for_effect(effect: &Effect) -> Option<(AuthorityKind, Option<String>)> {
    match effect {
        Effect::FS => Some((AuthorityKind::Filesystem, None)),
        Effect::Net => Some((AuthorityKind::Network, None)),
        Effect::Inference => Some((AuthorityKind::Inference, None)),
        Effect::FFI
        | Effect::DB
        | Effect::Python
        | Effect::Env
        | Effect::Process
        | Effect::System => Some((AuthorityKind::Custom, Some(format!("effect:{}", effect)))),
        _ => None,
    }
}

fn authority_entry_for_grant(
    grant: &AuthorityGrant,
) -> (AuthorityKind, Option<String>, Option<String>) {
    match grant {
        AuthorityGrant::NetTcpOut { host, port } => (
            AuthorityKind::Network,
            Some(format!("{host}:{port}")),
            Some("connect".to_string()),
        ),
        AuthorityGrant::FsRead { path } => (
            AuthorityKind::Filesystem,
            Some(path.clone()),
            Some("read".to_string()),
        ),
        AuthorityGrant::FsWrite { path } => (
            AuthorityKind::Filesystem,
            Some(path.clone()),
            Some("write".to_string()),
        ),
        AuthorityGrant::EnvRead { name } => (
            AuthorityKind::Custom,
            Some(format!("Env::{name}")),
            Some("read".to_string()),
        ),
        AuthorityGrant::SecretRead { name } => (
            AuthorityKind::Secret,
            Some(name.clone()),
            Some("read".to_string()),
        ),
        AuthorityGrant::ProcessRun { command } => (
            AuthorityKind::Custom,
            Some(format!("Process::Run({command})")),
            Some("run".to_string()),
        ),
        AuthorityGrant::Other {
            namespace,
            operation,
            argument,
        } => (
            AuthorityKind::Custom,
            Some(match argument {
                Some(argument) => format!("{namespace}::{operation}({argument})"),
                None => format!("{namespace}::{operation}"),
            }),
            Some(operation.to_ascii_lowercase()),
        ),
    }
}

fn spawn_authority_tokens(module: &mir::Module) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    for function in module.functions.iter().chain(module.behaviors.iter()) {
        for block in &function.blocks {
            for stmt in &block.stmts {
                if let mir::Stmt::Assign {
                    op: mir::RValue::Spawn { capabilities, .. },
                    ..
                } = stmt
                {
                    tokens.extend(capabilities.iter().cloned());
                }
            }
        }
    }
    tokens
}

fn put_u32(hasher: &mut blake3::Hasher, value: u32) {
    hasher.update(&value.to_le_bytes());
}

fn put_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    put_u32(hasher, value.len() as u32);
    hasher.update(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect_checker::EffectChecker;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;

    fn checked(source: &str) -> (EffectChecker, hir::Module, mir::Module) {
        let tokens = Lexer::new(source).lex().unwrap();
        let ast = Parser::new(tokens).parse_module().unwrap();
        let mut type_checker = TypeChecker::new();
        type_checker.check_module(&ast).unwrap();

        let mut effect_checker = EffectChecker::new();
        effect_checker.check_module(&ast.decls).unwrap();

        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mir = crate::mir_lower::lower_module(&hir).unwrap();
        (effect_checker, hir, mir)
    }

    fn digest(fill: char) -> String {
        format!("blake3:{}", fill.to_string().repeat(64))
    }

    fn emit(source: &str) -> BehaviorManifest {
        let (effect_checker, hir, mir) = checked(source);
        BehaviorManifest::from_checked_program(BehaviorManifestInput {
            package_name: "demo",
            package_version: "0.1.0",
            language_version: "1.0.0-frozen",
            artifact_kind: ArtifactKind::WasmModule,
            artifact_bytes: b"wasm",
            compiler_digest: &digest('a'),
            source_digest: &digest('b'),
            dependency_digest: &digest('c'),
            effect_checker: &effect_checker,
            hir: Some(&hir),
            mir: Some(&mir),
        })
        .unwrap()
    }

    #[test]
    fn emits_effects_from_functions_and_actor_behaviors() {
        let manifest = emit(
            r#"
actor Worker {
    behavior save() { perform FS.write("/tmp/x", "ok") }
}
fn main() { perform IO.print("ok") }
"#,
        );

        let names: Vec<_> = manifest
            .effects
            .iter()
            .map(|entry| entry.effect.as_str())
            .collect();
        assert!(names.contains(&"FS"));
        assert!(names.contains(&"IO"));
        assert!(manifest
            .authority
            .iter()
            .any(|entry| entry.kind == AuthorityKind::Filesystem));
    }

    #[test]
    fn emits_protocol_and_durable_state_schema_from_checked_hir() {
        let manifest = emit(
            r#"
persistent actor Counter {
    state durable count: Int = 0
    behavior add(by: Int) { self.count = self.count + by }
}
fn main() { 0 }
"#,
        );

        let actor = manifest
            .actors
            .iter()
            .find(|actor| actor.name == "Counter")
            .unwrap();
        assert_eq!(actor.durability, PersistenceClass::Durable);
        assert!(actor.protocol.as_deref().unwrap().starts_with("blake3:"));
        assert!(actor
            .state_schema
            .as_deref()
            .unwrap()
            .starts_with("blake3:"));
        assert_eq!(manifest.durability.len(), 1);
        assert_eq!(manifest.provenance.state_schema_digests.len(), 1);
    }

    #[test]
    fn emits_exact_spawn_authority_without_widening_resource() {
        let manifest = emit(
            r#"
actor Worker { behavior run() { nil } }
fn main() {
    let worker = spawn Worker {} with [Net::TcpOut("api.example.com:443")]
    worker ! run()
}
"#,
        );

        assert!(manifest.authority.iter().any(|entry| {
            entry.kind == AuthorityKind::Network
                && entry.resource.as_deref() == Some("api.example.com:443")
                && entry.operations == vec!["connect"]
        }));
    }

    #[test]
    fn public_function_interface_uses_canonical_contract_digest() {
        let manifest = emit(
            r#"
pub fn add(a: Int, b: Int) -> Int { a + b }
fn main() { add(1, 2) }
"#,
        );

        assert_eq!(manifest.interfaces.len(), 1);
        assert_eq!(manifest.interfaces[0].name, "add");
        assert!(manifest.interfaces[0]
            .contract_digest
            .as_deref()
            .unwrap()
            .starts_with("blake3:"));
        assert_eq!(manifest.provenance.interface_digests.len(), 1);
    }

    #[test]
    fn process_and_ffi_are_not_overclaimed_as_replayable() {
        let manifest = emit(
            r#"
fn main() {
    perform Process.run("echo")
    perform FFI.call("x")
}
"#,
        );

        assert!(manifest
            .effects
            .iter()
            .filter(|entry| matches!(entry.effect.as_str(), "Process" | "FFI"))
            .all(|entry| entry.replay == EffectReplay::Nonreplayable));
    }

    #[test]
    fn artifact_digest_is_bound_to_exact_bytes() {
        let manifest = emit("fn main() { 0 }");
        assert_eq!(
            manifest.artifact.digest,
            format!("blake3:{}", blake3::hash(b"wasm").to_hex())
        );
    }
}
