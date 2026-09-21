//! Durable workflow execution: event journaling, checkpointing, recovery,
//! signal routing, and timer scheduling.
//!
//! All functions in this module take `&Runtime` or `&mut Runtime` to access
//! the runtime's public fields. They live here instead of on `impl Runtime`
//! to keep the god-object at a manageable size.

use crate::bytecode::Constant;
use crate::primitives::ActorRole;
use crate::runtime::actor::Actor;
use crate::runtime::persistence::{ActorSnapshot, EventEntry, PersistedValue, WorkflowEvent};
use crate::runtime::{BytecodeDistributedCallbacks, BytecodeRuntimeCallbacks, Runtime, StateModel};
use crate::vm::{Frame, Value, VM};

// ---------------------------------------------------------------------------
// Utility predicates
// ---------------------------------------------------------------------------

pub(crate) fn next_sequence(rt: &Runtime, actor_id: u64) -> u64 {
    rt.persistence.latest_sequence(actor_id) + 1
}

pub(crate) fn actor_is_workflow(rt: &Runtime, actor_id: u64) -> bool {
    rt.actors
        .get(&actor_id)
        .map(|a| matches!(a.role(), Ok(ActorRole::Workflow)))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Checkpoint
// ---------------------------------------------------------------------------

/// Build a durable actor snapshot at an explicitly supplied commit sequence.
fn snapshot_actor_at(
    rt: &Runtime,
    actor_id: u64,
    sequence: u64,
) -> std::io::Result<Option<ActorSnapshot>> {
    let Some(actor) = rt.actors.get(&actor_id) else {
        return Ok(None);
    };
    if !actor.persistent {
        return Ok(None);
    }

    let mut state = std::collections::HashMap::new();
    for (name, value) in &actor.state_data {
        let model = actor
            .state_models
            .get(name)
            .copied()
            .unwrap_or(StateModel::Local);
        if model == StateModel::Durable || model.is_crdt() {
            let persisted = if name == "semantic_memory" || name == "procedural_memory" {
                vm_value_to_string_in_actor(value, actor)
                    .map(PersistedValue::String)
                    .unwrap_or_else(|| {
                        PersistedValue::from_value_resolved(value, actor.bytecode_module.as_ref())
                    })
            } else {
                PersistedValue::from_value_resolved(value, actor.bytecode_module.as_ref())
            };
            state.insert(name.clone(), persisted);
        }
    }

    let authority_tokens = actor
        .authority_manifest()
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?
        .canonical_token_set();

    let crdt_snapshot = rt.crdt_manager.as_ref().map(|manager| {
        manager
            .snapshot()
            .into_iter()
            .map(|(id, (ty, bytes))| (id.0, ty.to_u8(), bytes))
            .collect()
    });
    let crdt_field_map = rt.crdt_manager.as_ref().map(|manager| {
        manager
            .field_map
            .iter()
            .filter(|((aid, _), _)| *aid == actor_id)
            .map(|((_, name), id)| (name.clone(), id.0))
            .collect()
    });

    Ok(Some(ActorSnapshot {
        actor_id,
        sequence,
        state,
        waiting_signal: actor.waiting_signal.clone(),
        crdt_snapshot,
        crdt_field_map,
        authority_tokens,
    }))
}

fn mark_checkpoint_committed(rt: &mut Runtime, actor_id: u64, sequence: u64) {
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = sequence;
        actor.dirty_fields.clear();
    }
}

/// Snapshot durable state as its own commit.
pub(crate) fn checkpoint_actor(rt: &mut Runtime, actor_id: u64) {
    let sequence = next_sequence(rt, actor_id);
    let snapshot = match snapshot_actor_at(rt, actor_id, sequence) {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(
                "nulang-persist: refusing to checkpoint actor {}: {}",
                actor_id,
                error
            );
            return;
        }
    };

    match rt.persistence.save_snapshot(snapshot.clone()) {
        Ok(()) => {
            rt.maybe_shadow_replicate(actor_id, &snapshot);
            mark_checkpoint_committed(rt, actor_id, sequence);
        }
        Err(error) => {
            tracing::warn!(
                "nulang-persist: checkpoint failed for actor {} at sequence {}: {}",
                actor_id,
                sequence,
                error
            );
        }
    }
}

/// Publish a workflow event and the state checkpoint produced by the same
/// transition as one persistence commit. Both records use one sequence.
pub(crate) fn commit_workflow_event(
    rt: &mut Runtime,
    actor_id: u64,
    event: WorkflowEvent,
) -> std::io::Result<()> {
    let sequence = event.sequence();
    let snapshot = snapshot_actor_at(rt, actor_id, sequence)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("persistent workflow actor {actor_id} not found"),
        )
    })?;

    rt.persistence
        .commit_workflow_event_and_snapshot(actor_id, event, snapshot.clone())?;
    rt.maybe_shadow_replicate(actor_id, &snapshot);
    mark_checkpoint_committed(rt, actor_id, sequence);
    Ok(())
}

// ---------------------------------------------------------------------------
// Event emission
// ---------------------------------------------------------------------------

/// Resolve a string-id value to the original string using the actor's
/// bytecode module constant pool.
fn resolve_string_constant(rt: &Runtime, actor_id: u64, value: &Value) -> Option<String> {
    let string_id = value.as_string_id()?;
    let actor = rt.actors.get(&actor_id)?;
    let module = actor.bytecode_module.as_ref()?;
    module
        .constants
        .get(string_id as usize)
        .and_then(|c| match c {
            Constant::String(s) => Some(s.clone()),
            _ => None,
        })
}

/// Emit a durable event for a workflow or event-sourced actor. For workflow
/// actors this appends to the durable journal and forces a checkpoint. For
/// event-sourced (non-workflow) actors the event is persisted to the event
/// journal and a checkpoint is forced.
pub(crate) fn emit_event(rt: &mut Runtime, actor_id: u64, event: &str, args: &[Value]) {
    let is_workflow = actor_is_workflow(rt, actor_id);
    let seq = next_sequence(rt, actor_id);

    if !is_workflow {
        let Some(actor) = rt.actors.get_mut(&actor_id) else {
            return;
        };

        actor.event_log.push((event.to_string(), args.to_vec()));
        let mut event_sourced_names: Vec<String> = actor
            .state_models
            .iter()
            .filter(|(_, model)| **model == StateModel::EventSourced)
            .map(|(name, _)| name.clone())
            .collect();
        event_sourced_names.sort();

        if event_sourced_names.is_empty() {
            return;
        }

        let old_values: Vec<(String, Value)> = event_sourced_names
            .iter()
            .filter_map(|name| {
                actor
                    .get_state_field(name)
                    .map(|value| (name.clone(), value))
            })
            .collect();

        for name in &event_sourced_names {
            if let Some(n) = actor.get_state_field(name).and_then(|value| value.as_int()) {
                actor.set_state_field(name, Value::int(n + 1));
            }
        }

        let module = actor.bytecode_module.as_ref();
        let persisted_args: Vec<PersistedValue> = args
            .iter()
            .map(|value| PersistedValue::from_value_resolved(value, module))
            .collect();
        let entries: Vec<EventEntry> = event_sourced_names
            .iter()
            .map(|name| {
                let current_value = actor.get_state_field(name).unwrap_or(Value::nil());
                EventEntry {
                    sequence: seq,
                    field_name: name.clone(),
                    event_name: event.to_string(),
                    args: persisted_args.clone(),
                    value: PersistedValue::from_value_resolved(&current_value, module),
                }
            })
            .collect();

        match rt.persistence.append_events(actor_id, &entries) {
            Ok(()) => {
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    for name in &event_sourced_names {
                        actor.event_sourced_sequences.insert(name.clone(), seq);
                    }
                    actor.sequence = seq;
                }
            }
            Err(err) => {
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    for (name, value) in old_values {
                        actor.set_state_field(&name, value);
                    }
                    actor.event_log.pop();
                }
                tracing::warn!(
                    "nulang-persist: rolled back event-sourced mutation for actor {} at sequence {}: {}",
                    actor_id,
                    seq,
                    err
                );
            }
        }
        return;
    }

    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.event_log.push((event.to_string(), args.to_vec()));
        let mut event_sourced_names: Vec<String> = actor
            .state_models
            .iter()
            .filter(|(_, model)| **model == StateModel::EventSourced)
            .map(|(name, _)| name.clone())
            .collect();
        event_sourced_names.sort();
        for name in &event_sourced_names {
            if let Some(n) = actor.get_state_field(name).and_then(|value| value.as_int()) {
                actor.set_state_field(name, Value::int(n + 1));
            }
        }
    }

    let workflow_event = if event == "ParallelBranchCompleted" && args.len() == 2 {
        let parallel_step_name =
            resolve_string_constant(rt, actor_id, &args[0]).unwrap_or_default();
        let branch_name = resolve_string_constant(rt, actor_id, &args[1]).unwrap_or_default();
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            let current = actor
                .get_state_field("parallel_progress")
                .and_then(|value| value.as_int())
                .unwrap_or(0);
            actor.set_state_field("parallel_progress", Value::int(current + 1));
        }
        WorkflowEvent::ParallelBranchCompleted {
            sequence: seq,
            parallel_step_name,
            branch_name,
        }
    } else {
        let module = rt
            .actors
            .get(&actor_id)
            .and_then(|actor| actor.bytecode_module.as_ref());
        let payload: Vec<PersistedValue> = args
            .iter()
            .map(|value| PersistedValue::from_value_resolved(value, module))
            .collect();
        WorkflowEvent::Custom {
            sequence: seq,
            name: event.to_string(),
            args: payload,
        }
    };

    if let Err(error) = commit_workflow_event(rt, actor_id, workflow_event) {
        tracing::warn!(
            "nulang-persist: workflow event commit failed for actor {} at sequence {}: {}",
            actor_id,
            seq,
            error
        );
    }
}

// ---------------------------------------------------------------------------
// Append wrappers
// ---------------------------------------------------------------------------

pub(crate) fn append_timer_set(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    duration_ms: u64,
) -> std::io::Result<()> {
    let sequence = next_sequence(rt, actor_id);
    commit_workflow_event(
        rt,
        actor_id,
        WorkflowEvent::TimerSet {
            sequence,
            name: name.to_string(),
            duration_ms,
        },
    )
}

pub(crate) fn append_timer_fired(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
) -> std::io::Result<()> {
    let sequence = next_sequence(rt, actor_id);
    commit_workflow_event(
        rt,
        actor_id,
        WorkflowEvent::TimerFired {
            sequence,
            name: name.to_string(),
        },
    )
}

pub(crate) fn append_signal_received(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    payload: Option<String>,
) -> std::io::Result<()> {
    let sequence = next_sequence(rt, actor_id);
    commit_workflow_event(
        rt,
        actor_id,
        WorkflowEvent::SignalReceived {
            sequence,
            name: name.to_string(),
            payload,
        },
    )
}

pub(crate) fn append_saga_compensated(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: &str,
) -> std::io::Result<()> {
    let sequence = next_sequence(rt, actor_id);
    commit_workflow_event(
        rt,
        actor_id,
        WorkflowEvent::SagaCompensated {
            sequence,
            step_name: step_name.to_string(),
        },
    )
}

// ---------------------------------------------------------------------------
// Signal delivery
// ---------------------------------------------------------------------------

/// Deliver a signal to a workflow actor. If the actor is currently suspended
/// waiting for this signal, its execution is resumed.
pub(crate) fn signal_workflow(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    payload: Option<String>,
) {
    let _ = append_signal_received(rt, actor_id, name, payload.clone());

    let should_resume = {
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            actor.received_signals.push((name.to_string(), payload));
            actor
                .waiting_signal
                .as_ref()
                .map(|s| s == name)
                .unwrap_or(false)
        } else {
            false
        }
    };

    if should_resume {
        rt.resume_suspended_workflow_step(actor_id);
    }
}

/// Register a read-only query handler on a workflow actor.
pub(crate) fn register_workflow_query(rt: &mut Runtime, actor_id: u64, name: &str, handler: Value) {
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        if matches!(actor.role(), Ok(ActorRole::Workflow)) {
            actor.query_handlers.insert(name.to_string(), handler);
        }
    }
}

/// Invoke a registered query handler on a workflow actor and return its result.
pub(crate) fn query_workflow(rt: &mut Runtime, actor_id: u64, name: &str) -> Option<Value> {
    let (handler, module) = {
        let actor = rt.actors.get(&actor_id)?;
        if !matches!(actor.role(), Ok(ActorRole::Workflow)) {
            return None;
        }
        let handler = *actor.query_handlers.get(name)?;
        (handler, actor.bytecode_module.clone()?)
    };

    let self_ptr: *mut Runtime = rt;
    let mut vm = VM::new();
    vm.load_module(module);
    let offset = vm.function_offset_for_value(0, handler).ok()?;
    vm.set_actor_callbacks(Box::new(BytecodeRuntimeCallbacks::new(self_ptr, actor_id)));
    vm.set_distributed_callbacks(Box::new(BytecodeDistributedCallbacks { runtime: self_ptr }));
    let mut frame = Frame::new(None, 0);
    frame.pc = offset;
    vm.set_current_frame(frame);
    vm.run_from(0, offset).ok()
}

// ---------------------------------------------------------------------------
// Timer scheduling
// ---------------------------------------------------------------------------

/// Schedule a durable timer for a workflow actor.
pub(crate) fn schedule_workflow_timer(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    duration_ms: u64,
) {
    if actor_is_workflow(rt, actor_id) {
        let _ = append_timer_set(rt, actor_id, name, duration_ms);
    }
    rt.rearm_timer(actor_id, name, duration_ms);
}

// ---------------------------------------------------------------------------
// Helpers (re-exported from mod.rs; kept here for cohesion)
// ---------------------------------------------------------------------------

/// Convert a VM value into a Rust string, reading pointer payloads as
/// null-terminated UTF-8 and string-id values via the actor's bytecode module.
pub(crate) fn vm_value_to_string_in_actor(value: &Value, actor: &Actor) -> Option<String> {
    if let Some(id) = value.as_string_id() {
        actor
            .bytecode_module
            .as_ref()
            .and_then(|m| m.constants.get(id as usize))
            .and_then(|c| match c {
                Constant::String(s) => Some(s.clone()),
                _ => None,
            })
    } else if let Some(ptr) = value.as_ptr() {
        if ptr.is_null() {
            Some(String::new())
        } else {
            Some(unsafe {
                std::ffi::CStr::from_ptr(ptr as *const std::ffi::c_char)
                    .to_string_lossy()
                    .into_owned()
            })
        }
    } else {
        None
    }
}
