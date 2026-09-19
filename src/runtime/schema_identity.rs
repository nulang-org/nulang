//! Durable actor-schema identity rules.
//!
//! Recovery must not infer an actor schema from behavior-table position or
//! select the first metadata entry in a multi-actor module. New snapshots
//! persist an exact `ActorMeta.name`. Legacy snapshots without that field may
//! be recovered only when the loaded module contains exactly one actor schema.

use crate::bytecode::{ActorMeta, CodeModule};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SnapshotSchemaError {
    UnknownPersistedSchema {
        schema_name: String,
        candidates: Vec<String>,
    },
    UnexpectedPersistedSchema {
        expected_schema_name: String,
        actual_schema_name: String,
    },
    AmbiguousLegacySnapshot {
        candidates: Vec<String>,
    },
    ModuleHasNoActorSchema,
}

impl fmt::Display for SnapshotSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotSchemaError::UnknownPersistedSchema {
                schema_name,
                candidates,
            } => write!(
                f,
                "persisted actor schema '{schema_name}' is not declared by the loaded module (declared: {})",
                candidates.join(", ")
            ),
            SnapshotSchemaError::UnexpectedPersistedSchema {
                expected_schema_name,
                actual_schema_name,
            } => write!(
                f,
                "persisted actor schema '{actual_schema_name}' does not match expected schema '{expected_schema_name}'"
            ),
            SnapshotSchemaError::AmbiguousLegacySnapshot { candidates } => write!(
                f,
                "legacy snapshot has no actor schema identity and the loaded module declares multiple schemas: {}",
                candidates.join(", ")
            ),
            SnapshotSchemaError::ModuleHasNoActorSchema => {
                write!(f, "loaded module declares no actor schema")
            }
        }
    }
}

impl std::error::Error for SnapshotSchemaError {}

/// Canonical schema represented by a live runtime actor name.
///
/// Ordinary actors carry the exact `ActorMeta.name`; virtual actors carry an
/// instance label such as `Counter@customer-42`, which is reduced to its
/// declared virtual actor schema by the shared ownership adapter.
pub(crate) fn canonical_schema_name_for_runtime_actor<'a>(
    module: &'a CodeModule,
    runtime_name: &str,
) -> Option<&'a str> {
    super::behavior_ownership::actor_meta_for_runtime_name(module, runtime_name)
        .map(|meta| meta.name.as_str())
}

/// Resolve the actor metadata a durable snapshot is allowed to hydrate.
///
/// New snapshots carry an exact schema name and must match it exactly. For a
/// pre-schema-identity legacy snapshot, compatibility is deliberately narrow:
/// a module with exactly one actor schema is unambiguous and may recover;
/// modules with multiple actor schemas fail closed instead of guessing.
pub(crate) fn resolve_snapshot_actor_meta<'a>(
    module: &'a CodeModule,
    persisted_schema_name: Option<&str>,
) -> Result<&'a ActorMeta, SnapshotSchemaError> {
    if let Some(schema_name) = persisted_schema_name.filter(|name| !name.is_empty()) {
        return module
            .actor_metadata
            .iter()
            .find(|meta| meta.name == schema_name)
            .ok_or_else(|| SnapshotSchemaError::UnknownPersistedSchema {
                schema_name: schema_name.to_string(),
                candidates: sorted_schema_names(module),
            });
    }

    let mut candidates = module.actor_metadata.iter();
    let Some(first) = candidates.next() else {
        return Err(SnapshotSchemaError::ModuleHasNoActorSchema);
    };
    if candidates.next().is_some() {
        return Err(SnapshotSchemaError::AmbiguousLegacySnapshot {
            candidates: sorted_schema_names(module),
        });
    }
    Ok(first)
}

/// Resolve a durable snapshot for a caller that already knows which schema it
/// must represent, such as `resolve_or_hydrate_grain(Type@key)`.
///
/// This closes a subtle multi-schema hole: an exact persisted schema can be
/// valid for the module while still being the wrong schema for the requested
/// virtual actor type.
pub(crate) fn resolve_expected_snapshot_actor_meta<'a>(
    module: &'a CodeModule,
    persisted_schema_name: Option<&str>,
    expected_schema_name: &str,
) -> Result<&'a ActorMeta, SnapshotSchemaError> {
    let meta = resolve_snapshot_actor_meta(module, persisted_schema_name)?;
    if meta.name != expected_schema_name {
        return Err(SnapshotSchemaError::UnexpectedPersistedSchema {
            expected_schema_name: expected_schema_name.to_string(),
            actual_schema_name: meta.name.clone(),
        });
    }
    Ok(meta)
}

fn sorted_schema_names(module: &CodeModule) -> Vec<String> {
    let mut names: Vec<String> = module
        .actor_metadata
        .iter()
        .map(|meta| meta.name.clone())
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;

    fn compile(source: &str) -> CodeModule {
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");
        let mut typechecker = TypeChecker::new();
        typechecker.check_module(&ast).expect("typecheck");
        let hir = crate::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).expect("MIR lowering");
        crate::mir_codegen::compile_mir(&mut mir, "snapshot_schema_identity")
            .expect("bytecode codegen")
    }

    #[test]
    fn persisted_schema_resolves_exact_actor_meta() {
        let module = compile(
            r#"
            actor First { behavior hit() { nil } }
            actor Second { behavior hit() { nil } }
            "#,
        );
        let meta = resolve_snapshot_actor_meta(&module, Some("Second")).expect("Second schema");
        assert_eq!(meta.name, "Second");
    }

    #[test]
    fn persisted_unknown_schema_fails_closed() {
        let module = compile(
            r#"
            actor First { behavior hit() { nil } }
            actor Second { behavior hit() { nil } }
            "#,
        );
        assert_eq!(
            resolve_snapshot_actor_meta(&module, Some("Missing")).unwrap_err(),
            SnapshotSchemaError::UnknownPersistedSchema {
                schema_name: "Missing".to_string(),
                candidates: vec!["First".to_string(), "Second".to_string()],
            }
        );
    }

    #[test]
    fn expected_schema_rejects_another_valid_module_schema() {
        let module = compile(
            r#"
            virtual entity Counter(key: String) { behavior hit() { nil } }
            actor Other { behavior hit() { nil } }
            "#,
        );
        assert_eq!(
            resolve_expected_snapshot_actor_meta(&module, Some("Other"), "Counter").unwrap_err(),
            SnapshotSchemaError::UnexpectedPersistedSchema {
                expected_schema_name: "Counter".to_string(),
                actual_schema_name: "Other".to_string(),
            }
        );
    }

    #[test]
    fn legacy_single_schema_snapshot_is_compatible() {
        let module = compile("actor Only { behavior hit() { nil } }");
        let meta = resolve_snapshot_actor_meta(&module, None).expect("unambiguous legacy schema");
        assert_eq!(meta.name, "Only");
    }

    #[test]
    fn legacy_multi_schema_snapshot_never_guesses_owner() {
        let module = compile(
            r#"
            actor First { behavior hit() { nil } }
            actor Second { behavior hit() { nil } }
            "#,
        );
        assert_eq!(
            resolve_snapshot_actor_meta(&module, None).unwrap_err(),
            SnapshotSchemaError::AmbiguousLegacySnapshot {
                candidates: vec!["First".to_string(), "Second".to_string()],
            }
        );
    }

    #[test]
    fn virtual_actor_instance_name_canonicalizes_to_schema() {
        let module = compile(
            r#"
            virtual entity Counter(key: String) {
                behavior hit() { nil }
            }
            "#,
        );
        assert_eq!(
            canonical_schema_name_for_runtime_actor(&module, "Counter@customer-42"),
            Some("Counter")
        );
    }
}