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
use crate::runtime::turn::{commit_turn, StagedCommand, TurnOutcome};
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
// Durable turn commit + checkpoint compatibility
// ---------------------------------------------------------------------------

fn activation_epoch(rt: &Runtime, actor_id: u64) -> std::io::Result<u64> {
    let placement_epoch = rt
        .respawn_opted
        .get(&actor_id)
        .copied()
        .filter(|epoch| *epoch != 0)
        .unwrap_or(0);
    let persisted_epoch = rt
        .persistence
        .load_durable_tail(actor_id)?
        .map(|tail| tail.activation_epoch)
        .unwrap_or(0);

    Ok(placement_epoch.max(persisted_epoch).max(1))
}

fn build_actor_snapshot(
    rt: &Runtime,
    actor_id: u64,
    sequence: u64,
) -> std::io::Result<Option<ActorSnapshot>> {
    let actor = match rt.actors.get(&actor_id) {
        Some(actor) => actor,
        None => return Ok(None),
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
        .map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid actor authority manifest: {error}"),
            )
        })?
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

fn finish_snapshot_commit(
    rt: &mut Runtime,
    actor_id: u64,
    snapshot: &ActorSnapshot,
    clear_dirty_fields: bool,
) {
    rt.maybe_shadow_replicate(actor_id, snapshot);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = snapshot.sequence;
        if clear_dirty_fields {
            actor.dirty_fields.clear();
        }
    }
}

fn commit_prepared_outcome(
    rt: &mut Runtime,
    actor_id: u64,
    expected_previous_sequence: u64,
    outcome: TurnOutcome,
    snapshot: Option<ActorSnapshot>,
    clear_dirty_fields: bool,
) -> std::io::Result<()> {
    if matches!(
        rt.actors.get(&actor_id).map(|actor| actor.state),
        Some(crate::runtime::ActorState::Suspended | crate::runtime::ActorState::Terminated)
    ) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "durable transition refused for quarantined or terminated actor",
        ));
    }

    let sequence = expected_previous_sequence.checked_add(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "durable transition sequence overflow",
        )
    })?;
    let transition = outcome.into_transition(
        actor_id,
        activation_epoch(rt, actor_id)?,
        expected_previous_sequence,
        snapshot.clone(),
    )?;

    commit_turn(rt.persistence.as_mut(), transition)?;

    if let Some(snapshot) = snapshot.as_ref() {
        finish_snapshot_commit(rt, actor_id, snapshot, clear_dirty_fields);
    }
    Ok(())
}

fn commit_outcome_at(
    rt: &mut Runtime,
    actor_id: u64,
    expected_previous_sequence: u64,
    outcome: TurnOutcome,
) -> std::io::Result<()> {
    let sequence = expected_previous_sequence.checked_add(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "durable transition sequence overflow",
        )
    })?;
    let snapshot = build_actor_snapshot(rt, actor_id, sequence)?;
    commit_prepared_outcome(
        rt,
        actor_id,
        expected_previous_sequence,
        outcome,
        snapshot,
        true,
    )
}

fn commit_outcome_from_last_snapshot(
    rt: &mut Runtime,
    actor_id: u64,
    outcome: TurnOutcome,
    waiting_signal: Option<String>,
) -> std::io::Result<()> {
    let previous = rt.persistence.latest_sequence(actor_id);
    let sequence = previous.checked_add(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "durable transition sequence overflow",
        )
    })?;
    let mut snapshot = rt.persistence.load_snapshot(actor_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "durable workflow transition requires an existing committed snapshot",
        )
    })?;
    snapshot.sequence = sequence;
    snapshot.waiting_signal = waiting_signal;

    commit_prepared_outcome(rt, actor_id, previous, outcome, Some(snapshot), false)
}

fn commit_workflow_event_with_command(
    rt: &mut Runtime,
    actor_id: u64,
    command: Option<StagedCommand>,
    make_event: impl FnOnce(u64) -> WorkflowEvent,
) -> std::io::Result<()> {
    let previous = rt.persistence.latest_sequence(actor_id);
    let sequence = previous.checked_add(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "workflow transition sequence overflow",
        )
    })?;
    let mut outcome = TurnOutcome::workflow_event(make_event(sequence));
    outcome.command = command;
    commit_outcome_at(rt, actor_id, previous, outcome)
}

fn commit_workflow_event(
    rt: &mut Runtime,
    actor_id: u64,
    make_event: impl FnOnce(u64) -> WorkflowEvent,
) -> std::io::Result<()> {
    commit_workflow_event_with_command(rt, actor_id, None, make_event)
}

pub(crate) fn stage_command(
    rt: &Runtime,
    actor_id: u64,
    behavior_id: u16,
    payload: &[Value],
) -> StagedCommand {
    let module = rt
        .actors
        .get(&actor_id)
        .and_then(|actor| actor.bytecode_module.as_ref());
    StagedCommand {
        behavior_id,
        payload: payload
            .iter()
            .map(|value| PersistedValue::from_value_resolved(value, module))
            .collect(),
    }
}

pub(crate) fn commit_workflow_started(
    rt: &mut Runtime,
    actor_id: u64,
    name: String,
    state: Vec<PersistedValue>,
) -> std::io::Result<()> {
    commit_workflow_event(rt, actor_id, move |sequence| {
        WorkflowEvent::WorkflowStarted {
            sequence,
            name,
            state,
        }
    })
}

pub(crate) fn commit_step_completed(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: String,
) -> std::io::Result<()> {
    commit_step_completed_with_command(rt, actor_id, step_name, None)
}

pub(crate) fn commit_step_completed_with_command(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: String,
    command: Option<StagedCommand>,
) -> std::io::Result<()> {
    commit_workflow_event_with_command(rt, actor_id, command, move |sequence| {
        WorkflowEvent::StepCompleted {
            sequence,
            step_name,
        }
    })
}

pub(crate) fn commit_step_failed(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: String,
    error: String,
) -> std::io::Result<()> {
    commit_step_failed_with_command(rt, actor_id, step_name, error, None)
}

pub(crate) fn commit_step_failed_with_command(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: String,
    error: String,
    command: Option<StagedCommand>,
) -> std::io::Result<()> {
    commit_workflow_event_with_command(rt, actor_id, command, move |sequence| {
        WorkflowEvent::StepFailed {
            sequence,
            step_name,
            error,
        }
    })
}

pub(crate) fn commit_suspension(
    rt: &mut Runtime,
    actor_id: u64,
    command: Option<StagedCommand>,
) -> std::io::Result<()> {
    let waiting_signal = rt
        .actors
        .get(&actor_id)
        .and_then(|actor| actor.waiting_signal.clone());

    if command.is_none()
        && rt
            .persistence
            .load_snapshot(actor_id)
            .map(|snapshot| snapshot.waiting_signal == waiting_signal)
            .unwrap_or(false)
    {
        return Ok(());
    }

    let mut outcome = TurnOutcome::default();
    outcome.command = command;
    commit_outcome_from_last_snapshot(rt, actor_id, outcome, waiting_signal)
}

fn restore_last_committed_workflow_snapshot(rt: &mut Runtime, actor_id: u64) -> bool {
    let Some(snapshot) = rt.persistence.load_snapshot(actor_id) else {
        return false;
    };
    let Some(actor) = rt.actors.get_mut(&actor_id) else {
        return false;
    };

    // A failed turn may already have mutated live durable fields. Remove every
    // snapshot-owned field first so values introduced only by the rejected
    // turn cannot survive in memory, then rebuild from the committed image.
    let snapshot_owned_fields: Vec<String> = actor
        .state_models
        .iter()
        .filter(|(_, model)| **model == StateModel::Durable || model.is_crdt())
        .map(|(name, _)| name.clone())
        .collect();
    for name in snapshot_owned_fields {
        actor.state_data.remove(&name);
    }
    for (name, value) in snapshot.state {
        let value = value.to_value_on_heap(actor);
        actor.set_state_field(name, value);
    }

    actor.sequence = snapshot.sequence;
    actor.waiting_signal = snapshot.waiting_signal;
    actor.dirty_fields.clear();
    true
}

pub(crate) fn quarantine_after_commit_failure(
    rt: &mut Runtime,
    actor_id: u64,
    error: &std::io::Error,
) {
    let restored = restore_last_committed_workflow_snapshot(rt, actor_id);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.state = crate::runtime::ActorState::Suspended;
    }
    tracing::error!(
        actor_id,
        restored_committed_snapshot = restored,
        %error,
        "nulang-persist: durable transition failed; actor restored to its last committed snapshot and suspended"
    );
}

/// Persist one legacy checkpoint for a durable actor.
///
/// New workflow transitions must use the turn commit helpers above so their
/// state and semantic event share one commit boundary.  This function remains
/// for non-workflow compatibility paths while message journaling is migrated
/// onto `TurnOutcome`.
pub(crate) fn try_checkpoint_actor(rt: &mut Runtime, actor_id: u64) -> std::io::Result<()> {
    let sequence = next_sequence(rt, actor_id);
    let Some(snapshot) = build_actor_snapshot(rt, actor_id, sequence)? else {
        return Ok(());
    };

    rt.persistence.save_snapshot(snapshot.clone())?;
    finish_snapshot_commit(rt, actor_id, &snapshot, true);
    Ok(())
}

/// Best-effort compatibility checkpoint used by non-transition call sites.
pub(crate) fn checkpoint_actor(rt: &mut Runtime, actor_id: u64) {
    if let Err(error) = try_checkpoint_actor(rt, actor_id) {
        tracing::warn!(
            actor_id,
            %error,
            "nulang-persist: durable checkpoint failed"
        );
    }
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
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.event_log.push((event.to_string(), args.to_vec()));
        let event_sourced_names: Vec<String> = actor
            .state_models
            .iter()
            .filter(|(_, model)| **model == StateModel::EventSourced)
            .map(|(name, _)| name.clone())
            .collect();
        for name in &event_sourced_names {
            if let Some(n) = actor.get_state_field(name).and_then(|v| v.as_int()) {
                actor.set_state_field(name, Value::int(n + 1));
            }
        }
        // Persist events for EventSourced fields (non-workflow actors).
        if !is_workflow && !event_sourced_names.is_empty() {
            let module = actor.bytecode_module.as_ref();
            let persisted_args: Vec<PersistedValue> = args
                .iter()
                .map(|v| PersistedValue::from_value_resolved(v, module))
                .collect();
            for name in &event_sourced_names {
                // Capture the field's current value AFTER the apply
                // handler has run and the +1 has been applied.  This
                // snapshot lets recovery reconstruct the exact post-
                // apply value without re-executing bytecode.
                let current_val = actor.get_state_field(name).unwrap_or(Value::nil());
                let entry = EventEntry {
                    sequence: seq,
                    field_name: name.clone(),
                    event_name: event.to_string(),
                    args: persisted_args.clone(),
                    value: PersistedValue::from_value_resolved(&current_val, module),
                };
                let _ = rt.persistence.append_event(actor_id, entry);
            }
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                for name in &event_sourced_names {
                    actor.event_sourced_sequences.insert(name.clone(), seq);
                }
                actor.sequence = seq;
            }
        }
    }
    if is_workflow {
        let commit = if event == "ParallelBranchCompleted" && args.len() == 2 {
            let parallel_step_name =
                resolve_string_constant(rt, actor_id, &args[0]).unwrap_or_default();
            let branch_name = resolve_string_constant(rt, actor_id, &args[1]).unwrap_or_default();
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                let current = actor
                    .get_state_field("parallel_progress")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                actor.set_state_field("parallel_progress", Value::int(current + 1));
            }
            commit_workflow_event(rt, actor_id, move |sequence| {
                WorkflowEvent::ParallelBranchCompleted {
                    sequence,
                    parallel_step_name,
                    branch_name,
                }
            })
        } else {
            let module = rt
                .actors
                .get(&actor_id)
                .and_then(|a| a.bytecode_module.as_ref());
            let payload: Vec<PersistedValue> = args
                .iter()
                .map(|v| PersistedValue::from_value_resolved(v, module))
                .collect();
            let name = event.to_string();
            commit_workflow_event(rt, actor_id, move |sequence| WorkflowEvent::Custom {
                sequence,
                name,
                args: payload,
            })
        };

        if let Err(error) = commit {
            quarantine_after_commit_failure(rt, actor_id, &error);
        }
    }
}

// ---------------------------------------------------------------------------
// Workflow transition wrappers
// ---------------------------------------------------------------------------

pub(crate) fn append_timer_set(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    duration_ms: u64,
) -> std::io::Result<()> {
    let name = name.to_string();
    commit_workflow_event(rt, actor_id, move |sequence| WorkflowEvent::TimerSet {
        sequence,
        name,
        duration_ms,
    })
}

pub(crate) fn append_timer_fired(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
) -> std::io::Result<()> {
    let name = name.to_string();
    commit_workflow_event(rt, actor_id, move |sequence| WorkflowEvent::TimerFired {
        sequence,
        name,
    })
}

pub(crate) fn append_signal_received(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    payload: Option<String>,
) -> std::io::Result<()> {
    let preserve_committed_state = rt
        .actors
        .get(&actor_id)
        .map(|actor| actor.suspended_execution.is_some() || actor.waiting_signal.is_some())
        .unwrap_or(false);
    let waiting_signal = rt
        .actors
        .get(&actor_id)
        .and_then(|actor| actor.waiting_signal.clone());
    let previous = rt.persistence.latest_sequence(actor_id);
    let sequence = previous.checked_add(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "workflow transition sequence overflow",
        )
    })?;
    let event = WorkflowEvent::SignalReceived {
        sequence,
        name: name.to_string(),
        payload,
    };
    let outcome = TurnOutcome::workflow_event(event);

    if preserve_committed_state {
        commit_outcome_from_last_snapshot(rt, actor_id, outcome, waiting_signal)
    } else {
        commit_outcome_at(rt, actor_id, previous, outcome)
    }
}

pub(crate) fn append_saga_compensated(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: &str,
) -> std::io::Result<()> {
    let step_name = step_name.to_string();
    commit_workflow_event(rt, actor_id, move |sequence| {
        WorkflowEvent::SagaCompensated {
            sequence,
            step_name,
        }
    })
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
    if let Err(error) = append_signal_received(rt, actor_id, name, payload.clone()) {
        quarantine_after_commit_failure(rt, actor_id, &error);
        return;
    }

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
        if !matches!(actor.role(), Ok(ActorRole::Workflow))
            || matches!(
                actor.state,
                crate::runtime::ActorState::Suspended | crate::runtime::ActorState::Terminated
            )
        {
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
        if let Err(error) = append_timer_set(rt, actor_id, name, duration_ms) {
            quarantine_after_commit_failure(rt, actor_id, &error);
            return;
        }
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
