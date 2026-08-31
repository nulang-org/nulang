//! Actor spawn subsystem: creates new actors with state, bytecode handlers,
//! and recovery metadata. These free functions take `&mut Runtime` to access
//! the runtime's public fields.

use std::collections::HashMap;

use crate::runtime::actor::{Actor, ActorBackend, BehaviorEntry};
use crate::runtime::persistence::{PersistedValue, StateModel, WorkflowEvent};
use crate::runtime::timer_fired_handler;
use crate::runtime::Runtime;
use crate::runtime::{bytecode_step_placeholder, fresh_actor_id, map_ast_state_model};
use crate::vm::Value;

/// Core spawn logic shared by all spawn entry points.
pub(crate) fn spawn_actor_with_models(
    rt: &mut Runtime,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
) -> u64 {
    spawn_actor_with_id(
        rt,
        fresh_actor_id(),
        init,
        state_models,
        persistent,
        workflow,
    )
}

/// Spawn an actor with a pre-assigned id. `Runtime::spawn_actor_near` uses
/// this to co-locate a new actor on the shard of a reference actor by drawing
/// an id that maps to that shard.
pub(crate) fn spawn_actor_with_id(
    rt: &mut Runtime,
    id: u64,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
) -> u64 {
    let mut actor = Actor::new(id, format!("actor_{}", id), 0);
    let state_fields = init();
    for (name, value) in state_fields {
        actor.set_state_field(name, value);
    }
    actor.state_models = state_models;
    // Register CRDT-backed fields with the CrdtManager.
    for (field_name, model) in &actor.state_models {
        if let StateModel::Crdt(crdt_type) = model {
            let initial = actor
                .state_data
                .get(field_name)
                .copied()
                .unwrap_or(Value::nil());
            if let Some(ref mut mgr) = rt.crdt_manager {
                mgr.register_actor_field(id, field_name, *crdt_type, initial);
            }
        }
    }
    actor.persistent = persistent;
    let workflow_name = workflow.map(|n| n.to_string());
    if let Some(name) = workflow {
        actor.is_workflow = true;
        actor.name = name.to_string();
        actor.register_behavior("__timer_fired", timer_fired_handler);
    }
    actor.state = crate::runtime::ActorState::Running;
    // Restart recovery (CLI durability): when a persistent actor is spawned
    // with an id that already has durable state in the persistence store
    // (e.g. a previous `nula run` wrote `.nulang/store/actor_<id>/`), overlay
    // the snapshot and replay the event log instead of keeping the declared
    // defaults. Actor ids are drawn from a process-local counter that starts
    // at the same value on every run, so a deterministic program re-spawns
    // the same entities with the same ids after a restart. Fresh ids (new
    // actors, or the default in-memory store used by tests) find no snapshot
    // and are unaffected. Workflow actors have their own journal-based
    // recovery path (`recover_actor`), so they are skipped here.
    if persistent && workflow.is_none() {
        restore_persistent_state(rt, &mut actor);
    }
    rt.actors.insert(id, actor);
    if workflow.is_some() {
        let seq = crate::runtime::workflow::next_sequence(rt, id);
        let state = {
            let actor = rt.actors.get(&id).unwrap();
            let mut state = Vec::new();
            for (field_name, value) in &actor.state_data {
                let model = actor
                    .state_models
                    .get(field_name)
                    .copied()
                    .unwrap_or(StateModel::Local);
                if model.is_persistent() {
                    state.push(PersistedValue::from_value_resolved(
                        value,
                        actor.bytecode_module.as_ref(),
                    ));
                }
            }
            state
        };
        let _ = rt.persistence.append_workflow_event(
            id,
            WorkflowEvent::WorkflowStarted {
                sequence: seq,
                name: workflow_name.as_ref().unwrap().clone(),
                state,
            },
        );
        crate::runtime::workflow::checkpoint_actor(rt, id);
    }
    rt.enqueue_actor(id);
    id
}

/// Overlay previously persisted state onto a freshly spawned persistent
/// actor: restore the durable-field snapshot, then replay the event-sourced
/// event log (mirroring the restore portion of `Runtime::recover_actor`).
/// A no-op when the store holds no snapshot for this actor id.
fn restore_persistent_state(rt: &Runtime, actor: &mut Actor) {
    // Event-sourced-only actors may have an event log but no snapshot
    // (EventSourced fields are excluded from snapshots by design), so both
    // halves run independently.
    if let Some(snapshot) = rt.persistence.load_snapshot(actor.id) {
        actor.sequence = snapshot.sequence;
        actor.waiting_signal = snapshot.waiting_signal;
        for (name, value) in snapshot.state {
            let v = value.to_value_on_heap(actor);
            actor.set_state_field(name, v);
        }
    }
    for entry in rt.persistence.read_events(actor.id) {
        let v = entry.value.to_value_on_heap(actor);
        actor.set_state_field(&entry.field_name, v);
        let current = actor
            .event_sourced_sequences
            .get(&entry.field_name)
            .copied()
            .unwrap_or(0);
        if entry.sequence > current {
            actor
                .event_sourced_sequences
                .insert(entry.field_name.clone(), entry.sequence);
        }
    }
}

/// Spawn an actor for `module`'s behavior `behavior_idx`, seeded with the
/// `init` state fields, and wire up its bytecode handlers. Shared body of
/// Build a bytecode actor's `bytecode_offsets` vector.
///
/// Ordinary bytecode actors are dispatched by WHOLE-MODULE behavior id
/// (`bytecode_offsets` indexes the module's full behavior list). Workflow
/// actors are the exception: `layout_workflow_behavior_table` assigns
/// steps LOCAL ids 0..step_count-1 (internal behaviors like
/// `__timer_fired` come after), so a workflow's offsets must be its OWN
/// behaviors compressed to local order — a plain actor declared before
/// the workflow would otherwise shift every step (SPEC2 §10 known-issue
/// #2, also seen at recover/migrate/hot-reload).
pub(crate) fn bytecode_offsets_for(
    module: &crate::bytecode::CodeModule,
    is_workflow: bool,
) -> Vec<usize> {
    if is_workflow {
        module
            .actor_metadata
            .iter()
            .find(|m| m.is_workflow)
            .map(|meta| {
                meta.behavior_indices
                    .iter()
                    .map(|&i| module.behaviors[i].code_offset)
                    .collect()
            })
            .unwrap_or_else(|| module.behaviors.iter().map(|b| b.code_offset).collect())
    } else {
        module.behaviors.iter().map(|b| b.code_offset).collect()
    }
}

/// both VM-callback `spawn_actor` impls.
pub(crate) fn spawn_from_module(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    behavior_idx: usize,
    init: Vec<(String, Value)>,
) -> Value {
    rt.register_module_grains(module);
    let meta = module
        .actor_metadata
        .iter()
        .find(|m| m.behavior_indices.contains(&behavior_idx));
    let id = if let Some(meta) = meta {
        let state_models: HashMap<String, StateModel> = meta
            .state_models
            .iter()
            .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
            .collect();
        let defaults = meta.state_defaults.clone();
        spawn_actor_with_models(
            rt,
            Box::new(move || {
                let mut fields: Vec<(String, Value)> = defaults
                    .iter()
                    .map(|(name, c)| (name.clone(), crate::vm::constant_to_value(c)))
                    .collect();
                fields.extend(init);
                fields
            }),
            state_models,
            meta.persistent,
            if meta.is_workflow {
                Some(meta.name.as_str())
            } else {
                None
            },
        )
    } else {
        spawn_actor_with_models(rt, Box::new(move || init), HashMap::new(), false, None)
    };
    let offsets: Vec<usize> =
        bytecode_offsets_for(module, meta.map(|m| m.is_workflow).unwrap_or(false));
    // compensation_offsets filtered to this actor's own behaviors so
    // step-local indices in run_saga_compensation match.
    let compensation_offsets: Vec<Option<usize>> = if let Some(meta) = meta {
        meta.behavior_indices
            .iter()
            .map(|&i| module.behaviors[i].compensate_offset)
            .collect()
    } else {
        module
            .behaviors
            .iter()
            .map(|b| b.compensate_offset)
            .collect()
    };
    if let Some(actor) = rt.actors.get_mut(&id) {
        actor.bytecode_module = Some(module.clone());
        actor.bytecode_offsets = offsets.clone();
        actor.compensation_offsets = compensation_offsets.clone();
        if let Some(meta) = meta {
            if meta.is_agent {
                actor.is_agent = true;
                for (name, c) in &meta.state_defaults {
                    if let crate::bytecode::Constant::String(json) = c {
                        if name == "retry_config" {
                            actor.retry_config = serde_json::from_str(json).ok();
                        } else if name == "fallback_config" {
                            actor.fallback_config = serde_json::from_str(json).unwrap_or_default();
                        }
                    }
                }
            }
            for (name, c) in &meta.state_defaults {
                if let crate::bytecode::Constant::String(s) = c {
                    let ptr = actor.allocate_string(s);
                    actor.set_state_field(name, ptr);
                }
            }
            actor.backend = match meta.backend {
                crate::ast::ActorBackendKind::Native => ActorBackend::Native,
                crate::ast::ActorBackendKind::WasmComponent => ActorBackend::WasmComponent {
                    component_path: String::new(),
                },
            };
        }
    }
    // Wire AOT-native dispatch: if an AOT module is registered for this actor
    // type, register the adapter for each behavior it compiles so the
    // scheduler dispatches them natively (bytecode falls back for the rest).
    if let Some(meta) = meta.as_ref() {
        if !meta.is_workflow {
            let module_ptr = rt.aot_modules.get(&meta.name).copied();
            if let Some(module_ptr) = module_ptr {
                let aot_module = unsafe { &*module_ptr };
                let runtime_ptr = rt as *mut Runtime;
                if let Some(actor) = rt.actors.get_mut(&id) {
                    for &gidx in &meta.behavior_indices {
                        // CodeModule behavior names are fully-qualified
                        // `"{Actor}.{behavior}"` (see mir_lower), which is
                        // exactly what `fn_ptr_for_behavior` expects.
                        let fq = module
                            .behaviors
                            .get(gidx)
                            .map(|b| b.name.clone())
                            .unwrap_or_default();
                        let short = fq
                            .strip_prefix(&format!("{}.", meta.name))
                            .map(str::to_string)
                            .unwrap_or_else(|| fq.clone());
                        if let Some(fn_ptr) = aot_module.fn_ptr_for_behavior(&fq) {
                            actor.register_behavior(short, crate::aot::aot_behavior_adapter);
                            actor.aot_targets.push(Some(crate::aot::AotDispatchTarget {
                                fn_ptr,
                                module: module_ptr,
                                runtime: runtime_ptr,
                            }));
                        } else {
                            actor.register_behavior(String::new(), bytecode_step_placeholder);
                            actor.aot_targets.push(None);
                        }
                    }
                }
            }
        }
    }
    if meta.map(|m| m.is_workflow).unwrap_or(false) {
        layout_workflow_behavior_table(rt, id);
    }
    register_recovery_module(rt, id, module.clone(), offsets, compensation_offsets);
    Value::actor_ref(id)
}

/// Populate a workflow actor's behavior table with placeholder entries for
/// each bytecode step plus the internal `__timer_fired` handler.
pub(crate) fn layout_workflow_behavior_table(rt: &mut Runtime, actor_id: u64) {
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        if !actor.is_workflow {
            return;
        }
        let step_count = actor.bytecode_offsets.len();
        actor
            .behavior_table
            .retain(|e| !e.name.is_empty() && e.name != "__timer_fired");
        for _ in 0..step_count {
            actor.behavior_table.push(BehaviorEntry {
                name: String::new(),
                handler_fn: bytecode_step_placeholder,
            });
        }
        actor.register_behavior("__timer_fired", timer_fired_handler);
    }
}

/// Register bytecode metadata so that a persistent actor can be recovered
/// after a runtime restart.
pub(crate) fn register_recovery_module(
    rt: &mut Runtime,
    actor_id: u64,
    module: crate::bytecode::CodeModule,
    offsets: Vec<usize>,
    compensation_offsets: Vec<Option<usize>>,
) {
    rt.recovery_modules
        .insert(actor_id, (module, offsets, compensation_offsets));
}
