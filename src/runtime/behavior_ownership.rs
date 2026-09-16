//! Canonical runtime actor-behavior ownership mapping.
//!
//! Bytecode behavior ids are module-global for ordinary actors but actor-local
//! for workflows. Runtime dispatch must therefore resolve every numeric/name
//! lookup through the target actor's `ActorMeta` instead of treating
//! `CodeModule::behaviors` as one globally interchangeable table.

use crate::bytecode::{ActorMeta, CodeModule};
use crate::primitives::ActorRole;

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
    let role = meta.role().ok()?;

    for (local_idx, &module_idx) in meta.behavior_indices.iter().enumerate() {
        let full_name = module.behaviors.get(module_idx)?.name.as_str();
        let short_name = full_name
            .strip_prefix(schema_name)
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
    module.behaviors.get(module_idx).map(|behavior| behavior.name.as_str())
}
