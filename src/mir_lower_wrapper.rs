//! Public HIR -> MIR lowering facade with RFC 0008 artifact validation.
//!
//! The legacy lowerer builds all MIR code and actor metadata. This facade adds
//! the migration-specific invariant that compiled entity artifacts carry a
//! validated, versioned migration manifest instead of silently dropping the
//! source migration contracts.

use crate::hir;
use crate::migration_manifest::MigrationManifest;
use crate::mir;
use crate::types::{NuError, NuResult};

/// Lower HIR to MIR, then bind declarative RFC 0008 migration metadata to each
/// actor artifact.
///
/// Executable migration bodies are deliberately not encoded here yet. The
/// manifest proves the version chain that later codegen/recovery work must bind
/// to executable migration code without serializing raw AST into bytecode.
pub fn lower_module(hir: &hir::Module) -> NuResult<mir::Module> {
    let mut module = crate::mir_lower_impl::lower_module(hir)?;
    attach_manifests(&hir.decls, &mut module)?;
    Ok(module)
}

fn attach_manifests(decls: &[hir::Decl], module: &mut mir::Module) -> NuResult<()> {
    for decl in decls {
        match decl {
            hir::Decl::Actor(actor) => {
                let manifest = MigrationManifest::from_decls(actor.version, &actor.migrations)
                    .map_err(|error| NuError::VMError {
                        msg: format!(
                            "invalid RFC 0008 migration chain for entity '{}': {error}",
                            actor.name
                        ),
                        span: actor.span,
                    })?;
                let json = manifest.to_json().map_err(|error| NuError::VMError {
                    msg: format!(
                        "failed to encode RFC 0008 migration manifest for entity '{}': {error}",
                        actor.name
                    ),
                    span: actor.span,
                })?;
                let meta = module
                    .actor_metadata
                    .iter_mut()
                    .find(|meta| meta.name == actor.name)
                    .ok_or_else(|| NuError::VMError {
                        msg: format!(
                            "internal: MIR actor metadata missing for entity '{}'",
                            actor.name
                        ),
                        span: actor.span,
                    })?;
                meta.version = actor.version;
                meta.migrations = json;
            }
            hir::Decl::Module { decls, .. } => attach_manifests(decls, module)?,
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower_source(source: &str) -> NuResult<mir::Module> {
        let tokens = crate::lexer::Lexer::new(source).lex()?;
        let mut parser = crate::parser::Parser::new(tokens);
        let ast = parser.parse_module()?;
        let mut checker = crate::typechecker::TypeChecker::new();
        checker.check_module(&ast)?;
        crate::migration_purity::check_module(&ast.decls)?;
        let hir = crate::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
        lower_module(&hir)
    }

    #[test]
    fn compiled_actor_metadata_carries_manifest() {
        let module = lower_source(
            r#"
            entity Counter {
                version: 2
                state count: Int = 0
                events | Changed(value: Int)
                migration from 1 to 2 {
                    state => { self.count = self.count + 1 }
                    events {
                        | Changed(value) => emit Changed(value)
                    }
                }
                behavior get() { self.count }
            }
            "#,
        )
        .unwrap();

        let meta = module
            .actor_metadata
            .iter()
            .find(|meta| meta.name == "Counter")
            .expect("Counter actor metadata");
        assert_eq!(meta.version, 2);
        assert!(!meta.migrations.is_empty());
        let manifest = MigrationManifest::from_json(&meta.migrations).unwrap();
        assert_eq!(manifest.target_version, 2);
        assert_eq!(manifest.contracts.len(), 1);
        assert!(manifest.contracts[0].has_state_transform);
        assert_eq!(manifest.contracts[0].event_transforms.len(), 1);
    }

    #[test]
    fn version_one_actor_carries_empty_valid_manifest() {
        let module = lower_source(
            r#"
            entity Counter {
                state count: Int = 0
                behavior get() { self.count }
            }
            "#,
        )
        .unwrap();
        let meta = module
            .actor_metadata
            .iter()
            .find(|meta| meta.name == "Counter")
            .expect("Counter actor metadata");
        let manifest = MigrationManifest::from_json(&meta.migrations).unwrap();
        assert_eq!(manifest.target_version, 1);
        assert!(manifest.contracts.is_empty());
    }

    #[test]
    fn lowering_rejects_gap_in_migration_chain() {
        let error = lower_source(
            r#"
            entity Counter {
                version: 3
                state count: Int = 0
                migration from 2 to 3 {
                    state => { self.count = self.count + 1 }
                }
                behavior get() { self.count }
            }
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("missing migration transition 1 -> 2"));
    }

    #[test]
    fn lowering_rejects_downgrade_transition() {
        let error = lower_source(
            r#"
            entity Counter {
                version: 2
                state count: Int = 0
                migration from 2 to 1 {
                    state => { self.count }
                }
                behavior get() { self.count }
            }
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("RFC 0008 requires W == V + 1"));
    }
}