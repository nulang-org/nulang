//! Durable workflow execution: event journaling, checkpointing, recovery,
//! signal routing, and timer scheduling.
//!
//! All functions in this module take `&Runtime` or `&mut Runtime` to access
//! the runtime's public fields. They live here instead of on `impl Runtime`
//! to keep the god-object at a manageable size.

use crate::bytecode::Constant;
use crate::primitives::ActorRole;
use crate::runtime::actor::Actor;
use crate::runtime::persistence::{
    ActorSnapshot, DurableTransition, EventEntry, JournalEntry, PersistedValue,
    WorkflowActivationId, WorkflowEvent, WorkflowReplayEventId, DURABLE_TRANSITION_VERSION,
};
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

fn current_custom_event_replay_id(
    rt: &mut Runtime,
    actor_id: u64,
) -> Option<WorkflowReplayEventId> {
    let actor = rt.actors.get_mut(&actor_id)?;
    let activation = actor.current_workflow_activation?;

    if actor.workflow_replay_activation != Some(activation) {
        actor.workflow_replay_activation = Some(activation);
        actor.workflow_replay_event_ordinal = 0;
    }

    Some(WorkflowReplayEventId::new(
        activation,
        actor.workflow_replay_event_ordinal,
    ))
}

fn advance_custom_event_replay_id(
    rt: &mut Runtime,
    actor_id: u64,
    committed: WorkflowReplayEventId,
) {
    let Some(actor) = rt.actors.get_mut(&actor_id) else {
        return;
    };
    if actor.workflow_replay_activation != Some(committed.activation)
        || actor.workflow_replay_event_ordinal != committed.ordinal
    {
        return;
    }

    actor.workflow_replay_event_ordinal = committed
        .ordinal
        .checked_add(1)
        .expect("workflow custom-event ordinal exhausted within one activation");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CustomEventReplayDisposition {
    Append,
    Consume,
    Conflict,
}

fn custom_event_replay_disposition(
    rt: &Runtime,
    actor_id: u64,
    replay_id: WorkflowReplayEventId,
    event: &str,
    payload: &[PersistedValue],
) -> CustomEventReplayDisposition {
    let mut committed: Option<(String, Vec<PersistedValue>)> = None;

    for workflow_event in rt.persistence.read_workflow_events(actor_id) {
        let WorkflowEvent::Custom {
            replay_id: Some(existing_id),
            name,
            args,
            ..
        } = workflow_event
        else {
            continue;
        };
        if existing_id != replay_id {
            continue;
        }
        if committed.is_some() {
            return CustomEventReplayDisposition::Conflict;
        }
        committed = Some((name, args));
    }

    match committed {
        None => CustomEventReplayDisposition::Append,
        Some((name, args)) if name == event && args.as_slice() == payload => {
            CustomEventReplayDisposition::Consume
        }
        Some(_) => CustomEventReplayDisposition::Conflict,
    }
}
// ---------------------------------------------------------------------------
// Checkpoint
// ---------------------------------------------------------------------------

/// Build the durable snapshot that belongs to one exact transition sequence.
///
/// This is pure staging: callers decide whether the snapshot is committed as a
/// legacy checkpoint or inside an atomic durable transition.
fn build_actor_snapshot_at_sequence(
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
            state.insert(name.clone(), actor.persist_value(value));
        }
    }

    let authority_tokens = actor
        .authority_manifest()
        .map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid actor authority manifest: {err}"),
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
    let schema_name = actor
        .bytecode_module
        .as_ref()
        .and_then(|module| {
            crate::runtime::schema_identity::canonical_schema_name_for_runtime_actor(
                module,
                &actor.name,
            )
        })
        .map(str::to_owned);

    Ok(Some(ActorSnapshot {
        actor_id,
        sequence,
        activation_epoch: actor.activation_epoch,
        state,
        waiting_signal: actor.waiting_signal.clone(),
        crdt_snapshot,
        crdt_field_map,
        schema_name,
        authority_tokens,
    }))
}

/// Persist one checkpoint for a durable actor.
///
/// Unlike the compatibility wrapper below, this function is fallible. Callers
/// that gate externally visible durable transitions (workflow creation, timer
/// commits, signals, compensation) must use this path so storage failure cannot
/// be mistaken for a committed transition.
pub(crate) fn try_checkpoint_actor(rt: &mut Runtime, actor_id: u64) -> std::io::Result<()> {
    let sequence = next_sequence(rt, actor_id);
    let Some(snapshot) = build_actor_snapshot_at_sequence(rt, actor_id, sequence)? else {
        return Ok(());
    };

    // The local persistence store is authoritative. Publish a shadow replica
    // only after the local snapshot commit succeeds; otherwise a failed local
    // checkpoint could leave a remote replica for an actor/transition that was
    // never durably committed at home.
    rt.persistence.save_snapshot(snapshot.clone())?;
    rt.maybe_shadow_replicate(actor_id, &snapshot);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = sequence;
        actor.dirty_fields.clear();
    }
    Ok(())
}

/// Atomically admit one workflow command before executing user code.
///
/// Command acceptance is itself a durable transition so every later workflow
/// write can extend one fenced atomic tail. The returned activation identity is
/// stable across retries and is attached to the terminal event that closes the
/// command.
pub(crate) fn commit_workflow_command(
    rt: &mut Runtime,
    actor_id: u64,
    behavior_id: u16,
    payload: Vec<PersistedValue>,
) -> std::io::Result<WorkflowActivationId> {
    let activation_epoch = rt
        .actors
        .get(&actor_id)
        .filter(|actor| actor.persistent)
        .map(|actor| actor.activation_epoch)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "workflow command admission requires a live persistent actor",
            )
        })?;
    let expected_previous_sequence = rt.persistence.latest_sequence(actor_id);
    let sequence = expected_previous_sequence.checked_add(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workflow command transition sequence overflow",
        )
    })?;
    let activation = WorkflowActivationId::new(actor_id, sequence);

    rt.persistence.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch,
        sequence,
        expected_previous_sequence,
        command: Some(JournalEntry {
            sequence,
            behavior_id,
            payload,
        }),
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    })?;

    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = sequence;
        actor.current_workflow_activation = Some(activation);
    }
    Ok(activation)
}

/// Atomically commit the durable state produced by one successful workflow
/// step together with its terminal StepCompleted event.
///
/// The command has already been atomically admitted by
/// `commit_workflow_command`; this transition closes the same activation with
/// one sequence shared by the resulting snapshot and terminal event.
pub(crate) fn commit_step_completed(
    rt: &mut Runtime,
    actor_id: u64,
    activation: Option<WorkflowActivationId>,
    step_name: String,
) -> std::io::Result<()> {
    let expected_previous_sequence = rt.persistence.latest_sequence(actor_id);
    let sequence = expected_previous_sequence.checked_add(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workflow terminal transition sequence overflow",
        )
    })?;
    let snapshot = build_actor_snapshot_at_sequence(rt, actor_id, sequence)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "workflow terminal transition requires a live persistent actor",
        )
    })?;
    let activation_epoch = snapshot.activation_epoch;

    rt.persistence.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch,
        sequence,
        expected_previous_sequence,
        command: None,
        snapshot: Some(snapshot.clone()),
        workflow_events: vec![WorkflowEvent::StepCompleted {
            sequence,
            activation,
            step_name,
        }],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    })?;

    rt.maybe_shadow_replicate(actor_id, &snapshot);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = sequence;
        actor.dirty_fields.clear();
    }
    Ok(())
}

/// Snapshot the durable and CRDT state of a persistent actor.
///
/// This wrapper intentionally preserves the legacy best-effort API for call
/// sites where checkpoint failure is diagnostic rather than a commit boundary.
/// New durable transitions should call `try_checkpoint_actor` and propagate
/// the error.
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
        if event == "ParallelBranchCompleted" && args.len() == 2 {
            let parallel_step_name =
                resolve_string_constant(rt, actor_id, &args[0]).unwrap_or_default();
            let branch_name = resolve_string_constant(rt, actor_id, &args[1]).unwrap_or_default();
            let _ = rt.persistence.append_parallel_branch_completed(
                actor_id,
                seq,
                parallel_step_name,
                branch_name,
            );
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                let current = actor
                    .get_state_field("parallel_progress")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                actor.set_state_field("parallel_progress", Value::int(current + 1));
            }
            checkpoint_actor(rt, actor_id);
        } else {
            let module = rt
                .actors
                .get(&actor_id)
                .and_then(|a| a.bytecode_module.as_ref());
            let payload: Vec<PersistedValue> = args
                .iter()
                .map(|v| PersistedValue::from_value_resolved(v, module))
                .collect();
            let replay_id = current_custom_event_replay_id(rt, actor_id);
            let mut should_checkpoint = false;
            if let Some(replay_id) = replay_id {
                match custom_event_replay_disposition(rt, actor_id, replay_id, event, &payload) {
                    CustomEventReplayDisposition::Consume => {
                        advance_custom_event_replay_id(rt, actor_id, replay_id);
                    }
                    CustomEventReplayDisposition::Conflict => {
                        tracing::error!(
                            actor_id,
                            activation_actor_id = replay_id.activation.actor_id,
                            activation_command_sequence = replay_id.activation.command_sequence,
                            ordinal = replay_id.ordinal,
                            event,
                            "nulang-workflow: replay identity conflicts with committed custom event; refusing durable mutation"
                        );
                    }
                    CustomEventReplayDisposition::Append => {
                        let appended = rt
                            .persistence
                            .append_workflow_event(
                                actor_id,
                                WorkflowEvent::Custom {
                                    sequence: seq,
                                    replay_id: Some(replay_id),
                                    name: event.to_string(),
                                    args: payload,
                                },
                            )
                            .is_ok();
                        if appended {
                            // This event belongs to an open activation. Keep the
                            // last completed snapshot unchanged so recovery can
                            // re-execute the command and consume this exact
                            // replay identity instead of treating partial state
                            // as completed progress.
                            advance_custom_event_replay_id(rt, actor_id, replay_id);
                        }
                    }
                }
            } else {
                should_checkpoint = rt
                    .persistence
                    .append_workflow_event(
                        actor_id,
                        WorkflowEvent::Custom {
                            sequence: seq,
                            replay_id: None,
                            name: event.to_string(),
                            args: payload,
                        },
                    )
                    .is_ok();
            }
            if should_checkpoint {
                checkpoint_actor(rt, actor_id);
            }
        }
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
    let seq = next_sequence(rt, actor_id);
    rt.persistence
        .append_timer_set(actor_id, seq, name.to_string(), duration_ms)?;
    try_checkpoint_actor(rt, actor_id)?;
    Ok(())
}

pub(crate) fn append_timer_fired(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
) -> std::io::Result<()> {
    let seq = next_sequence(rt, actor_id);
    rt.persistence
        .append_timer_fired(actor_id, seq, name.to_string())?;
    try_checkpoint_actor(rt, actor_id)?;
    Ok(())
}

pub(crate) fn append_signal_received(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    payload: Option<String>,
) -> std::io::Result<()> {
    let seq = next_sequence(rt, actor_id);
    rt.persistence
        .append_signal_received(actor_id, seq, name.to_string(), payload)?;
    try_checkpoint_actor(rt, actor_id)?;
    Ok(())
}

pub(crate) fn append_saga_compensated(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: &str,
) -> std::io::Result<()> {
    let seq = next_sequence(rt, actor_id);
    rt.persistence
        .append_saga_compensated(actor_id, seq, step_name.to_string())?;
    try_checkpoint_actor(rt, actor_id)?;
    Ok(())
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
) -> std::io::Result<()> {
    // A signal must not become visible in memory or resume execution unless
    // its durable journal write and checkpoint both succeeded.
    append_signal_received(rt, actor_id, name, payload.clone())?;

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
    Ok(())
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
) -> std::io::Result<()> {
    if actor_is_workflow(rt, actor_id) {
        // Never arm a live timer if its durable TimerSet/checkpoint failed.
        // Recovery can only reason about timers that were durably recorded.
        append_timer_set(rt, actor_id, name, duration_ms)?;
    }
    rt.rearm_timer(actor_id, name, duration_ms);
    Ok(())
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
