//! Durable workflow execution: event journaling, checkpointing, recovery,
//! signal routing, and timer scheduling.
//!
//! All functions in this module take `&Runtime` or `&mut Runtime` to access
//! the runtime's public fields. They live here instead of on `impl Runtime`
//! to keep the god-object at a manageable size.

use crate::bytecode::Constant;
use crate::primitives::ActorRole;
use crate::runtime::actor::Actor;
use crate::runtime::persistence::{\n    ActorSnapshot, DurableTransition, EventEntry, JournalEntry, PersistedValue, WorkflowEvent,\n    DURABLE_TRANSITION_VERSION,\n};
use crate::runtime::{BytecodeDistributedCallbacks, BytecodeRuntimeCallbacks, Runtime, StateModel};
use crate::vm::{Frame, Value, VM};

// ---------------------------------------------------------------------------
// Utility predicates
// ---------------------------------------------------------------------------

pub(crate) fn next_sequence(rt: &Runtime, actor_id: u64) -> u64 {
    rt.workflow_transitions
        .get(&actor_id)
        .map(|stage| stage.sequence)
        .unwrap_or_else(|| rt.persistence.latest_sequence(actor_id) + 1)
}

pub(crate) fn actor_is_workflow(rt: &Runtime, actor_id: u64) -> bool {
    rt.actors
        .get(&actor_id)
        .map(|a| matches!(a.role(), Ok(ActorRole::Workflow)))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Atomic workflow transitions (RFC 0022 Phase B)
// ---------------------------------------------------------------------------

/// In-memory staging area for one logical workflow transition.
///
/// Nothing in this object is durable until `commit_workflow_transition`
/// succeeds. Timer-wheel publication is deferred for the same reason.
#[derive(Debug, Clone)]
pub(crate) struct WorkflowTransitionStage {
    pub(crate) activation_epoch: u64,
    pub(crate) sequence: u64,
    pub(crate) expected_previous_sequence: u64,
    pub(crate) command: Option<JournalEntry>,
    pub(crate) workflow_events: Vec<WorkflowEvent>,
    pub(crate) base_snapshot: Option<ActorSnapshot>,
    pub(crate) timers_to_arm: Vec<(String, u64)>,
    pub(crate) received_signals_len: usize,
    pub(crate) compensated_steps_len: usize,
    pub(crate) event_log_len: usize,
}

/// Snapshot durable actor-owned state at an explicitly supplied transition
/// sequence. The caller decides whether this snapshot is committed alone
/// (legacy checkpoint) or as part of a `DurableTransition`.
fn build_actor_snapshot(
    rt: &Runtime,
    actor_id: u64,
    sequence: u64,
) -> std::io::Result<Option<ActorSnapshot>> {
    let actor = match rt.actors.get(&actor_id) {
        Some(actor) if actor.persistent => actor,
        _ => return Ok(None),
    };

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

/// Begin one workflow turn. Re-entrant calls reuse the already-active stage,
/// which lets timer/signal/event callbacks participate in their caller's
/// transaction rather than opening nested commits.
pub(crate) fn begin_workflow_transition(
    rt: &mut Runtime,
    actor_id: u64,
    command: Option<(u16, Vec<PersistedValue>)>,
) -> std::io::Result<bool> {
    let persistent_workflow = rt
        .actors
        .get(&actor_id)
        .map(|actor| actor.persistent && matches!(actor.role(), Ok(ActorRole::Workflow)))
        .unwrap_or(false);
    if !persistent_workflow {
        return Ok(false);
    }
    if rt.workflow_transitions.contains_key(&actor_id) {
        return Ok(false);
    }

    let expected_previous_sequence = rt.persistence.latest_sequence(actor_id);
    let sequence = expected_previous_sequence.checked_add(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "workflow transition sequence overflow",
        )
    })?;
    let activation_epoch = rt.respawn_opted.get(&actor_id).copied().unwrap_or(1);
    let (received_signals_len, compensated_steps_len, event_log_len) = rt
        .actors
        .get(&actor_id)
        .map(|actor| {
            (
                actor.received_signals.len(),
                actor.compensated_steps.len(),
                actor.event_log.len(),
            )
        })
        .unwrap_or((0, 0, 0));
    let command = command.map(|(behavior_id, payload)| JournalEntry {
        sequence,
        behavior_id,
        payload,
    });

    rt.workflow_transitions.insert(
        actor_id,
        WorkflowTransitionStage {
            activation_epoch,
            sequence,
            expected_previous_sequence,
            command,
            workflow_events: Vec::new(),
            base_snapshot: rt.persistence.load_snapshot(actor_id),
            timers_to_arm: Vec::new(),
            received_signals_len,
            compensated_steps_len,
            event_log_len,
        },
    );
    Ok(true)
}

pub(crate) fn has_workflow_transition(rt: &Runtime, actor_id: u64) -> bool {
    rt.workflow_transitions.contains_key(&actor_id)
}

fn stage_existing_workflow_event(
    rt: &mut Runtime,
    actor_id: u64,
    event: WorkflowEvent,
) -> std::io::Result<()> {
    let stage = rt.workflow_transitions.get_mut(&actor_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "workflow event staged without an active durable transition",
        )
    })?;
    if event.sequence() != stage.sequence {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "workflow event sequence {} does not match active transition {}",
                event.sequence(),
                stage.sequence
            ),
        ));
    }
    stage.workflow_events.push(event);
    Ok(())
}

/// Stage an event into the current turn. Outside a running workflow turn this
/// creates and commits a one-event transition, preserving the public runtime
/// helpers while removing their historical append/checkpoint split.
pub(crate) fn stage_or_commit_workflow_event(
    rt: &mut Runtime,
    actor_id: u64,
    make_event: impl FnOnce(u64) -> WorkflowEvent,
) -> std::io::Result<()> {
    let started = begin_workflow_transition(rt, actor_id, None)?;
    if !has_workflow_transition(rt, actor_id) {
        return Ok(());
    }
    let sequence = next_sequence(rt, actor_id);
    stage_existing_workflow_event(rt, actor_id, make_event(sequence))?;
    if started {
        if let Err(error) = commit_workflow_transition(rt, actor_id, false) {
            rollback_workflow_transition(rt, actor_id);
            return Err(error);
        }
    }
    Ok(())
}

/// Commit the active workflow transition.
///
/// When `preserve_pre_step_state` is true (a suspended workflow), the
/// transition commits the pre-step snapshot plus the new suspension marker.
/// This preserves the existing re-drive-on-recovery semantics without making
/// partially executed step mutations durable.
pub(crate) fn commit_workflow_transition(
    rt: &mut Runtime,
    actor_id: u64,
    preserve_pre_step_state: bool,
) -> std::io::Result<()> {
    let stage = match rt.workflow_transitions.get(&actor_id).cloned() {
        Some(stage) => stage,
        None => return Ok(()),
    };

    let snapshot = if preserve_pre_step_state {
        match stage.base_snapshot.clone() {
            Some(mut snapshot) => {
                snapshot.sequence = stage.sequence;
                snapshot.waiting_signal = rt
                    .actors
                    .get(&actor_id)
                    .and_then(|actor| actor.waiting_signal.clone());
                snapshot
            }
            None => build_actor_snapshot(rt, actor_id, stage.sequence)?.ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "workflow actor disappeared before durable transition commit",
                )
            })?,
        }
    } else {
        build_actor_snapshot(rt, actor_id, stage.sequence)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "workflow actor disappeared before durable transition commit",
            )
        })?
    };

    let transition = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: stage.activation_epoch,
        sequence: stage.sequence,
        expected_previous_sequence: stage.expected_previous_sequence,
        command: stage.command.clone(),
        snapshot: Some(snapshot.clone()),
        workflow_events: stage.workflow_events.clone(),
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    };

    rt.persistence.commit_transition(transition)?;

    // Only publish consequences after the atomic store commit is durable.
    rt.workflow_transitions.remove(&actor_id);
    rt.maybe_shadow_replicate(actor_id, &snapshot);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = stage.sequence;
        if !preserve_pre_step_state {
            actor.dirty_fields.clear();
        }
    }
    for (name, duration_ms) in stage.timers_to_arm {
        rt.rearm_timer(actor_id, &name, duration_ms);
    }
    Ok(())
}

/// Discard an uncommitted workflow turn and restore the last durable image.
///
/// This is the runtime half of RFC 0022's COMMIT-NOTHING branch: no deferred
/// timers are published, durable fields and CRDT replicas are restored, and
/// suspended VM state produced by the failed turn is dropped so execution
/// cannot continue from mutations the store rejected.
pub(crate) fn rollback_workflow_transition(rt: &mut Runtime, actor_id: u64) {
    let Some(stage) = rt.workflow_transitions.remove(&actor_id) else {
        return;
    };
    let Some(snapshot) = stage.base_snapshot else {
        // A workflow without a prior durable image cannot be safely resumed
        // after a rejected first transition. Drop any live suspension; the
        // caller may re-drive from workflow creation.
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            actor.suspended_execution = None;
            actor.waiting_signal = None;
            actor.dirty_fields.clear();
            actor.received_signals.truncate(stage.received_signals_len);
            actor.compensated_steps.truncate(stage.compensated_steps_len);
            actor.event_log.truncate(stage.event_log_len);
        }
        return;
    };

    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        for (name, persisted) in &snapshot.state {
            let restored = persisted.to_value_on_heap(actor);
            actor.state_data.insert(name.clone(), restored);
        }
        actor.sequence = snapshot.sequence;
        actor.waiting_signal = snapshot.waiting_signal.clone();
        actor.suspended_execution = None;
        actor.dirty_fields.clear();
        actor.received_signals.truncate(stage.received_signals_len);
        actor.compensated_steps.truncate(stage.compensated_steps_len);
        actor.event_log.truncate(stage.event_log_len);
    }

    if let (Some(manager), Some(crdt_snapshot), Some(field_map)) = (
        rt.crdt_manager.as_mut(),
        snapshot.crdt_snapshot.as_ref(),
        snapshot.crdt_field_map.as_ref(),
    ) {
        let owned: std::collections::HashSet<u64> = field_map.values().copied().collect();
        let restored = crdt_snapshot
            .iter()
            .filter(|(id, _, _)| owned.contains(id))
            .filter_map(|(id, ty, bytes)| {
                crate::ast::CrdtType::from_u8(*ty).map(|crdt_type| {
                    (
                        crate::runtime::crdt_manager::CrdtId(*id),
                        (crdt_type, bytes.clone()),
                    )
                })
            })
            .collect();
        manager.restore(restored);
    }
}

pub(crate) fn stage_step_completed(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: String,
) -> std::io::Result<()> {
    if !has_workflow_transition(rt, actor_id) {
        begin_workflow_transition(rt, actor_id, None)?;
    }
    if !has_workflow_transition(rt, actor_id) {
        return Ok(());
    }
    let sequence = next_sequence(rt, actor_id);
    stage_existing_workflow_event(
        rt,
        actor_id,
        WorkflowEvent::StepCompleted {
            sequence,
            step_name,
        },
    )
}

pub(crate) fn stage_step_failed(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: String,
    error: String,
) -> std::io::Result<()> {
    if !has_workflow_transition(rt, actor_id) {
        begin_workflow_transition(rt, actor_id, None)?;
    }
    if !has_workflow_transition(rt, actor_id) {
        return Ok(());
    }
    let sequence = next_sequence(rt, actor_id);
    stage_existing_workflow_event(
        rt,
        actor_id,
        WorkflowEvent::StepFailed {
            sequence,
            step_name,
            error,
        },
    )
}

// ---------------------------------------------------------------------------
// Checkpoint
// ---------------------------------------------------------------------------

/// Persist one checkpoint for a durable actor.
///
/// Unlike the compatibility wrapper below, this function is fallible. Callers
/// that gate externally visible durable transitions (workflow creation, timer
/// commits, signals, compensation) must use this path so storage failure cannot
/// be mistaken for a committed transition.
pub(crate) fn try_checkpoint_actor(rt: &mut Runtime, actor_id: u64) -> std::io::Result<()> {
    let actor = match rt.actors.get(&actor_id) {
        Some(actor) if actor.persistent => actor,
        _ => return Ok(()),
    };
    let _ = actor;

    let sequence = rt.persistence.latest_sequence(actor_id) + 1;
    let snapshot = build_actor_snapshot(rt, actor_id, sequence)?.expect("persistent actor snapshot");
    rt.persistence.save_snapshot(snapshot.clone())?;
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
        let staged = if event == "ParallelBranchCompleted" && args.len() == 2 {
            let parallel_step_name =
                resolve_string_constant(rt, actor_id, &args[0]).unwrap_or_default();
            let branch_name = resolve_string_constant(rt, actor_id, &args[1]).unwrap_or_default();
            let result = stage_or_commit_workflow_event(rt, actor_id, |sequence| {
                WorkflowEvent::ParallelBranchCompleted {
                    sequence,
                    parallel_step_name,
                    branch_name,
                }
            });
            if result.is_ok() {
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    let current = actor
                        .get_state_field("parallel_progress")
                        .and_then(|v| v.as_int())
                        .unwrap_or(0);
                    actor.set_state_field("parallel_progress", Value::int(current + 1));
                }
            }
            result
        } else {
            let module = rt
                .actors
                .get(&actor_id)
                .and_then(|a| a.bytecode_module.as_ref());
            let payload: Vec<PersistedValue> = args
                .iter()
                .map(|v| PersistedValue::from_value_resolved(v, module))
                .collect();
            stage_or_commit_workflow_event(rt, actor_id, |sequence| WorkflowEvent::Custom {
                sequence,
                name: event.to_string(),
                args: payload,
            })
        };
        if let Err(error) = staged {
            tracing::warn!(
                actor_id,
                %error,
                "nulang-persist: workflow domain event transition rejected"
            );
            rollback_workflow_transition(rt, actor_id);
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
    stage_or_commit_workflow_event(rt, actor_id, |sequence| WorkflowEvent::TimerSet {
        sequence,
        name: name.to_string(),
        duration_ms,
    })
}

pub(crate) fn append_timer_fired(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
) -> std::io::Result<()> {
    stage_or_commit_workflow_event(rt, actor_id, |sequence| WorkflowEvent::TimerFired {
        sequence,
        name: name.to_string(),
    })
}

pub(crate) fn append_signal_received(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    payload: Option<String>,
) -> std::io::Result<()> {
    stage_or_commit_workflow_event(rt, actor_id, |sequence| WorkflowEvent::SignalReceived {
        sequence,
        name: name.to_string(),
        payload,
    })
}

pub(crate) fn append_saga_compensated(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: &str,
) -> std::io::Result<()> {
    stage_or_commit_workflow_event(rt, actor_id, |sequence| WorkflowEvent::SagaCompensated {
        sequence,
        step_name: step_name.to_string(),
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
    let should_resume = rt
        .actors
        .get(&actor_id)
        .and_then(|actor| actor.waiting_signal.as_ref())
        .map(|waiting| waiting == name)
        .unwrap_or(false);

    // A matching signal and the state progress caused by resuming the step are
    // one logical transition. Non-matching signals commit as their own atomic
    // acceptance transition.
    let started = if should_resume {
        match begin_workflow_transition(rt, actor_id, None) {
            Ok(started) => started,
            Err(error) => {
                tracing::warn!(actor_id, %error, "nulang-persist: failed to begin signal transition");
                return;
            }
        }
    } else {
        false
    };

    if let Err(error) = append_signal_received(rt, actor_id, name, payload.clone()) {
        if started {
            rt.workflow_transitions.remove(&actor_id);
        }
        tracing::warn!(actor_id, %error, "nulang-persist: failed to commit workflow signal");
        return;
    }

    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.received_signals.push((name.to_string(), payload));
    }

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
    if !actor_is_workflow(rt, actor_id) {
        rt.rearm_timer(actor_id, name, duration_ms);
        return;
    }

    let started = match begin_workflow_transition(rt, actor_id, None) {
        Ok(started) => started,
        Err(error) => {
            tracing::warn!(actor_id, %error, "nulang-persist: failed to begin timer transition");
            return;
        }
    };
    let sequence = next_sequence(rt, actor_id);
    if let Err(error) = stage_existing_workflow_event(
        rt,
        actor_id,
        WorkflowEvent::TimerSet {
            sequence,
            name: name.to_string(),
            duration_ms,
        },
    ) {
        if started {
            rt.workflow_transitions.remove(&actor_id);
        }
        tracing::warn!(actor_id, %error, "nulang-persist: failed to stage workflow timer");
        return;
    }
    if let Some(stage) = rt.workflow_transitions.get_mut(&actor_id) {
        stage.timers_to_arm.push((name.to_string(), duration_ms));
    }

    // A timer created outside a running workflow turn is a one-event
    // transition. Inside a turn it stays staged until that turn commits.
    if started {
        if let Err(error) = commit_workflow_transition(rt, actor_id, false) {
            rollback_workflow_transition(rt, actor_id);
            tracing::warn!(actor_id, %error, "nulang-persist: workflow timer commit failed");
        }
    }
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
