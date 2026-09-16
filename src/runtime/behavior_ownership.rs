//! Canonical runtime actor-behavior ownership mapping.
//!
//! Bytecode behavior ids are module-global for ordinary actors but actor-local
//! for workflows. Runtime dispatch must therefore resolve every numeric/name
//! lookup through the target actor's `ActorMeta` instead of treating
//! `CodeModule::behaviors` as one globally interchangeable table.

use crate::bytecode::{ActorMeta, CodeModule};
use crate::primitives::ActorRole;

/// Resolve the canonical actor metadata represented by a runtime actor name.
///
/// Module-spawned actors preserve `ActorMeta.name` directly. Virtual actors
/// use the human-readable instance name `Type@key`; only the `Type` prefix is
/// schema identity. Synthetic/manual names such as `actor_42` deliberately do
/// not guess an owner from an unrelated module.
pub(crate) fn actor_meta_for_runtime_name<'a>(
    module: &'a CodeModule,
    runtime_name: &str,
) -> Option<&'a ActorMeta> {
    if let Some(meta) = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == runtime_name)
    {
        return Some(meta);
    }

    let (grain_type, _) = runtime_name.split_once('@')?;
    module
        .actor_metadata
        .iter()
        .find(|meta| meta.is_virtual && meta.name == grain_type)
}

pub(crate) fn actor_meta_for_schema<'a>(
    module: &'a CodeModule,
    schema_name: &str,
) -> Option<&'a ActorMeta> {
    module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == schema_name)
}

/// Translate a runtime-visible behavior id to the module-global behavior index
/// after proving that the behavior belongs to `schema_name`.
///
/// Ordinary actors use module-global ids directly. Workflows intentionally use
/// local step ids (`0..own_behavior_count`) at runtime, so those ids must be
/// translated through `ActorMeta::behavior_indices` first.
pub(crate) fn module_behavior_index_for_runtime_id(
    module: &CodeModule,
    schema_name: &str,
    runtime_behavior_idx: usize,
) -> Option<usize> {
    let meta = actor_meta_for_schema(module, schema_name)?;
    module_behavior_index_for_meta(meta, runtime_behavior_idx)
}

/// Runtime-name variant used by the actor runtime. This understands virtual
/// actor instance names while preserving the same fail-closed ownership rule.
pub(crate) fn module_behavior_index_for_actor(
    module: &CodeModule,
    runtime_name: &str,
    runtime_behavior_idx: usize,
) -> Option<usize> {
    let meta = actor_meta_for_runtime_name(module, runtime_name)?;
    module_behavior_index_for_meta(meta, runtime_behavior_idx)
}

fn module_behavior_index_for_meta(
    meta: &ActorMeta,
    runtime_behavior_idx: usize,
) -> Option<usize> {
    match meta.role().ok()? {
        ActorRole::Workflow => meta.behavior_indices.get(runtime_behavior_idx).copied(),
        _ => meta
            .behavior_indices
            .contains(&runtime_behavior_idx)
            .then_some(runtime_behavior_idx),
    }
}

/// Resolve a source/wire behavior name only inside the target actor schema.
/// Returns the runtime-visible id (local for workflows, module-global for
/// ordinary actors).
pub(crate) fn runtime_behavior_id_for_name(
    module: &CodeModule,
    schema_name: &str,
    behavior: &str,
) -> Option<usize> {
    let meta = actor_meta_for_schema(module, schema_name)?;
    runtime_behavior_id_for_meta(module, meta, behavior)
}

/// Runtime-name variant used by live actors/grains.
pub(crate) fn runtime_behavior_id_for_actor_name(
    module: &CodeModule,
    runtime_name: &str,
    behavior: &str,
) -> Option<usize> {
    let meta = actor_meta_for_runtime_name(module, runtime_name)?;
    runtime_behavior_id_for_meta(module, meta, behavior)
}

fn runtime_behavior_id_for_meta(
    module: &CodeModule,
    meta: &ActorMeta,
    behavior: &str,
) -> Option<usize> {
    let role = meta.role().ok()?;

    for (local_idx, &module_idx) in meta.behavior_indices.iter().enumerate() {
        let full_name = module.behaviors.get(module_idx)?.name.as_str();
        let short_name = full_name
            .strip_prefix(meta.name.as_str())
            .and_then(|rest| rest.strip_prefix('.'));
        if full_name == behavior || short_name == Some(behavior) {
            return Some(match role {
                ActorRole::Workflow => local_idx,
                _ => module_idx,
            });
        }
    }
    None
}

/// Return the canonical fully-qualified behavior name for a runtime-visible id
/// after proving ownership by `schema_name`.
pub(crate) fn behavior_name_for_runtime_id<'a>(
    module: &'a CodeModule,
    schema_name: &str,
    runtime_behavior_idx: usize,
) -> Option<&'a str> {
    let module_idx =
        module_behavior_index_for_runtime_id(module, schema_name, runtime_behavior_idx)?;
    module
        .behaviors
        .get(module_idx)
        .map(|behavior| behavior.name.as_str())
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
        crate::mir_codegen::compile_mir(&mut mir, "behavior_ownership")
            .expect("bytecode codegen")
    }

    #[test]
    fn ordinary_actor_ids_are_module_global_but_schema_owned() {
        let module = compile(
            r#"
            actor First {
                behavior hit() { nil }
            }

            actor Second {
                behavior hit() { nil }
                behavior only_second() { nil }
            }
            "#,
        );
        let first = actor_meta_for_schema(&module, "First").expect("First metadata");
        let second = actor_meta_for_schema(&module, "Second").expect("Second metadata");
        let first_hit = first.behavior_indices[0];
        let second_hit = second.behavior_indices[0];

        assert_ne!(first_hit, second_hit);
        assert_eq!(
            module_behavior_index_for_runtime_id(&module, "Second", second_hit),
            Some(second_hit)
        );
        assert_eq!(
            module_behavior_index_for_runtime_id(&module, "Second", first_hit),
            None,
            "an in-range behavior owned by First must not be valid for Second"
        );
        assert_eq!(
            runtime_behavior_id_for_name(&module, "Second", "hit"),
            Some(second_hit)
        );
        assert_eq!(
            behavior_name_for_runtime_id(&module, "Second", second_hit),
            Some("Second.hit")
        );
    }

    #[test]
    fn workflow_runtime_ids_translate_through_own_behavior_indices() {
        let module = compile(
            r#"
            actor Prefix {
                behavior ping() { nil }
            }

            workflow Flow {
                step first { nil }
                step second { nil }
            }
            "#,
        );
        let flow = actor_meta_for_schema(&module, "Flow").expect("Flow metadata");
        assert!(
            flow.behavior_indices[0] > 0,
            "Prefix must occupy an earlier module slot"
        );

        assert_eq!(
            module_behavior_index_for_runtime_id(&module, "Flow", 0),
            Some(flow.behavior_indices[0])
        );
        assert_eq!(
            module_behavior_index_for_runtime_id(&module, "Flow", 1),
            Some(flow.behavior_indices[1])
        );
        assert_eq!(runtime_behavior_id_for_name(&module, "Flow", "first"), Some(0));
        assert_eq!(runtime_behavior_id_for_name(&module, "Flow", "second"), Some(1));
        assert_eq!(
            behavior_name_for_runtime_id(&module, "Flow", 0),
            Some("Flow.first")
        );
    }

    #[test]
    fn virtual_actor_instance_name_resolves_only_its_virtual_schema() {
        let module = compile(
            r#"
            virtual entity User(key: String) {
                behavior hit() { nil }
            }

            actor Other {
                behavior hit() { nil }
            }
            "#,
        );
        let user = actor_meta_for_schema(&module, "User").expect("User metadata");
        let other = actor_meta_for_schema(&module, "Other").expect("Other metadata");
        let user_hit = user.behavior_indices[0];
        let other_hit = other.behavior_indices[0];

        assert_eq!(
            actor_meta_for_runtime_name(&module, "User@u-42").map(|meta| meta.name.as_str()),
            Some("User")
        );
        assert_eq!(
            module_behavior_index_for_actor(&module, "User@u-42", user_hit),
            Some(user_hit)
        );
        assert_eq!(
            module_behavior_index_for_actor(&module, "User@u-42", other_hit),
            None
        );
        assert_eq!(
            runtime_behavior_id_for_actor_name(&module, "User@u-42", "hit"),
            Some(user_hit)
        );
        assert_eq!(
            actor_meta_for_runtime_name(&module, "actor_42"),
            None,
            "synthetic instance names must not guess an actor schema"
        );
    }
}
