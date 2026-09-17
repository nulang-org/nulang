//! Experimental compiler-emitted Behavior Manifest (RFC 0020).
//!
//! This module deliberately sits outside HIR/MIR stability. It consumes
//! already-checked compiler semantics and emits a versioned, deterministic
//! deployment contract. The manifest describes required behavior; it never
//! grants authority.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ast::{Decl, Expr, WorkflowItem};
use crate::effect_checker::{flatten_decls, EffectChecker, EffectContext};
use crate::types::{Effect, NuResult};

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
pub struct BehaviorManifest {
    pub schema: String,
    pub package: PackageIdentity,
    pub artifact: ArtifactIdentity,
    pub compiler: CompilerIdentity,
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
    pub source_bytes: &'a [u8],
    /// Canonical dependency-lock bytes. An empty dependency set should pass
    /// the canonical empty lock representation, not ambient filesystem state.
    pub dependency_bytes: &'a [u8],
}

impl BehaviorManifest {
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

    /// Deterministic UTF-8 JSON. Struct field order is fixed, maps are
    /// BTreeMaps, and effect/authority vectors are sorted by construction.
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }
}

pub fn digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
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

    let replay = match effect {
        String | Array | Spawn | Send | Receive | Migrate | STM | Async | Cost | Event | Render => {
            EffectReplay::Safe
        }
        Inference | Time | Rand => EffectReplay::RequiresJournal,
        Net | FS | DB | FFI | Python | Process | System | Respond | Realtime | UserDefined(_) => {
            EffectReplay::RequiresIdempotencyKey
        }
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

    let mut required: BTreeMap<String, AuthorityDecl> = BTreeMap::new();
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
}
