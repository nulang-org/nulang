//! Compiler-side Nulang Behavior Manifest v0alpha1 emission.
//!
//! The manifest is a semantic summary for deployment/admission systems. It is
//! intentionally derived from already-checked compiler state and does not
//! grant authority.

use crate::effect_checker::EffectChecker;
use crate::types::{Effect, EffectRow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const BEHAVIOR_SCHEMA_V0ALPHA1: &str = "nulang.behavior/v0alpha1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BehaviorManifest {
    pub schema: String,
    pub package: PackageIdentity,
    pub artifact: ArtifactIdentity,
    pub compiler: CompilerIdentity,
    pub interfaces: Vec<serde_json::Value>,
    pub actors: Vec<serde_json::Value>,
    pub effects: Vec<EffectEntry>,
    pub authority: Vec<serde_json::Value>,
    pub durability: Vec<serde_json::Value>,
    pub replay: Vec<ReplayEntry>,
    pub resources: serde_json::Value,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageIdentity {
    pub name: String,
    pub version: String,
    pub language_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
pub struct CompilerIdentity {
    pub implementation: String,
    pub version: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EffectEntry {
    pub effect: String,
    pub class: EffectClass,
    pub determinism: Determinism,
    pub replay: EffectReplay,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EffectClass {
    Local,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Determinism {
    Deterministic,
    Nondeterministic,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EffectReplay {
    Safe,
    RequiresJournal,
    Nonreplayable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplayEntry {
    pub effect: String,
    pub class: ReplayClass,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReplayClass {
    LocalReplaySafe,
    JournalResult,
    ExternalNonreplayable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Provenance {
    pub source_digest: String,
    pub dependency_digest: String,
    pub interface_digests: Vec<String>,
    pub state_schema_digests: Vec<String>,
}

#[derive(Debug, Clone)]
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
}

impl BehaviorManifest {
    pub fn from_checked_program(input: BehaviorManifestInput<'_>) -> Self {
        let effects = collect_effects(input.effect_checker);
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

        Self {
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
            interfaces: Vec::new(),
            actors: Vec::new(),
            effects,
            authority: Vec::new(),
            durability: Vec::new(),
            replay,
            resources: serde_json::json!({}),
            provenance: Provenance {
                source_digest: input.source_digest.to_string(),
                dependency_digest: input.dependency_digest.to_string(),
                interface_digests: Vec::new(),
                state_schema_digests: Vec::new(),
            },
        }
    }

    pub fn to_pretty_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}

fn collect_effects(checker: &EffectChecker) -> Vec<EffectEntry> {
    let mut unique = BTreeSet::new();

    for row in checker.function_rows().values() {
        let effects = match row {
            EffectRow::Closed(effects) | EffectRow::Open(effects, _) => effects,
        };
        unique.extend(effects.iter().cloned());
    }

    unique.into_iter().map(effect_entry).collect()
}

fn effect_entry(effect: Effect) -> EffectEntry {
    let external = matches!(
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
    );

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Module;
    use crate::effect_checker::flatten_decls;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn checker(source: &str) -> EffectChecker {
        let tokens = Lexer::new(source).lex().unwrap();
        let module: Module = Parser::new(tokens).parse_module().unwrap();
        let mut checker = EffectChecker::new();
        checker
            .register_function_rows(&flatten_decls(&module.decls))
            .unwrap();
        checker
    }

    fn digest(fill: char) -> String {
        format!("blake3:{}", fill.to_string().repeat(64))
    }

    #[test]
    fn emits_deterministic_compiler_derived_effect_inventory() {
        let checker = checker(
            r#"
fn helper() { perform FS.read("/tmp/x") }
fn main() {
    helper()
    perform IO.print("ok")
}
"#,
        );

        let manifest = BehaviorManifest::from_checked_program(BehaviorManifestInput {
            package_name: "demo",
            package_version: "0.1.0",
            language_version: "1.0.0-frozen",
            artifact_kind: ArtifactKind::WasmModule,
            artifact_bytes: b"wasm",
            compiler_digest: &digest('a'),
            source_digest: &digest('b'),
            dependency_digest: &digest('c'),
            effect_checker: &checker,
        });

        let names: Vec<_> = manifest.effects.iter().map(|entry| entry.effect.as_str()).collect();
        assert_eq!(names, vec!["FS", "IO"]);
        assert_eq!(manifest.effects[0].class, EffectClass::External);
        assert_eq!(manifest.effects[1].class, EffectClass::Local);
        assert_eq!(
            manifest.artifact.digest,
            format!("blake3:{}", blake3::hash(b"wasm").to_hex())
        );
    }

    #[test]
    fn process_and_ffi_are_not_overclaimed_as_replayable() {
        let checker = checker(
            r#"
fn main() {
    perform Process.run("echo")
    perform FFI.call("x")
}
"#,
        );

        let manifest = BehaviorManifest::from_checked_program(BehaviorManifestInput {
            package_name: "demo",
            package_version: "0.1.0",
            language_version: "1.0.0-frozen",
            artifact_kind: ArtifactKind::WasmModule,
            artifact_bytes: b"wasm",
            compiler_digest: &digest('a'),
            source_digest: &digest('b'),
            dependency_digest: &digest('c'),
            effect_checker: &checker,
        });

        assert!(manifest
            .effects
            .iter()
            .filter(|entry| matches!(entry.effect.as_str(), "Process" | "FFI"))
            .all(|entry| entry.replay == EffectReplay::Nonreplayable));
    }
}
