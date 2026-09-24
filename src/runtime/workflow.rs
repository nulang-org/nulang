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
    ActorSnapshot, DurableTransition, EventEntry, JournalEntry, PersistedValue, WorkflowEvent,
    DURABLE_TRANSITION_VERSION,
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

/// The command currently driving a workflow activation.
///
/// The command is held only until the first durable transition produced by
/// that activation. It is then persisted in DurableTransition::command with
/// the same sequence as the state/event commit. This closes the historical
/// window where the runtime consumed a mailbox message before any durable
/// record established which command caused the resulting workflow state.
#[derive(Debug, Clone)]
pub(crate) struct PendingWorkflowCommand {
    pub behavior_id: u16,
    pub payload: Vec<PersistedValue>,
    /// Sequence at which this command is already present in durable history.
    /// None means the next command-caused transition must persist it.
    pub persisted_sequence: Option<u64>,
}

/// Stage the mailbox command that is about to execute a workflow behavior.
///
/// A suspended workflow owns its activation until it resumes, so a second
/// command must never replace an uncommitted trigger.
pub(crate) fn begin_workflow_command(
    rt: &mut Runtime,
    actor_id: u64,
    behavior_id: u16,
    payload: &[Value],
) -> std::io::Result<()> {
    if !actor_is_workflow(rt, actor_id) {
        return Ok(());
    }
    if let Some(pending) = rt.pending_workflow_commands.get(&actor_id) {
        // Recovery re-enqueues an already-durable command through the normal
        // mailbox path. Allow that exact command to re-enter the activation.
        if pending.persisted_sequence.is_some() && pending.behavior_id == behavior_id {
            return Ok(());
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "workflow activation already has an uncommitted driving command",
        ));
    }

    let module = rt
        .actors
        .get(&actor_id)
        .and_then(|actor| actor.bytecode_module.as_ref());
    let persisted_payload = payload
        .iter()
        .map(|value| PersistedValue::from_value_resolved(value, module))
        .collect();

    rt.pending_workflow_commands.insert(
        actor_id,
        PendingWorkflowCommand {
            behavior_id,
            payload: persisted_payload,
            persisted_sequence: None,
        },
    );
    Ok(())
}

fn pending_command_at_sequence(rt: &Runtime, actor_id: u64, sequence: u64) -> Option<JournalEntry> {
    rt.pending_workflow_commands
        .get(&actor_id)
        .filter(|pending| pending.persisted_sequence.is_none())
        .map(|pending| JournalEntry {
            sequence,
            behavior_id: pending.behavior_id,
            payload: pending.payload.clone(),
        })
}

/// Restore a command that is already present in durable history so recovery
/// can re-drive it without appending a duplicate journal record.
pub(crate) fn restore_workflow_command(
    rt: &mut Runtime,
    actor_id: u64,
    entry: &JournalEntry,
) {
    rt.pending_workflow_commands.insert(
        actor_id,
        PendingWorkflowCommand {
            behavior_id: entry.behavior_id,
            payload: entry.payload.clone(),
            persisted_sequence: Some(entry.sequence),
        },
    );
}

fn mark_workflow_commit_failure(rt: &mut Runtime, actor_id: u64) {
    rt.workflow_commit_failures.insert(actor_id);
}

fn activation_epoch(rt: &Runtime, actor_id: u64) -> u64 {
    // RFC 0014 already maintains the authoritative epoch for durable actors
    // opted into node-loss respawn. Local/non-opted actors remain epoch 1.
    rt.respawn_opted.get(&actor_id).copied().unwrap_or(1)
}

// ---------------------------------------------------------------------------
// Checkpoint
// ---------------------------------------------------------------------------

/// Build a durable snapshot for an actor at an explicit transition sequence.
///
/// Keeping snapshot construction separate from persistence lets ordinary
/// checkpoints and atomic workflow transitions share exactly the same state
/// selection, authority, and CRDT semantics.
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
            .filter(|((owner, _), _)| *owner == actor_id)
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

fn publish_committed_snapshot(rt: &mut Runtime, actor_id: u64, snapshot: &ActorSnapshot) {
    rt.maybe_shadow_replicate(actor_id, snapshot);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = snapshot.sequence;
        actor.dirty_fields.clear();
    }
}

/// Persist one checkpoint for a durable actor.
///
/// This remains the compatibility path for state-only checkpoints. Workflow
/// events should use `commit_workflow_event` so the event and resulting actor
/// snapshot share one atomic persistence boundary.
pub(crate) fn try_checkpoint_actor(rt: &mut Runtime, actor_id: u64) -> std::io::Result<()> {
    let persistent = rt
        .actors
        .get(&actor_id)
        .map(|actor| actor.persistent)
        .unwrap_or(false);
    if !persistent {
        return Ok(());
    }

    // Once a workflow has an atomic durable tail, every later workflow-owned
    // persistence operation must advance that same tail. A legacy snapshot
    // write here would create history that commit_transition cannot fence.
    if actor_is_workflow(rt, actor_id) {
        return commit_workflow_transition(rt, actor_id, Vec::new(), true, true);
    }

    let sequence = next_sequence(rt, actor_id);
    let Some(snapshot) = build_actor_snapshot(rt, actor_id, sequence)? else {
        return Ok(());
    };
    rt.persistence.save_snapshot(snapshot.clone())?;
    publish_committed_snapshot(rt, actor_id, &snapshot);
    Ok(())
}

/// Commit workflow-owned durable state through the canonical atomic boundary.
///
/// An empty event vector is a state-only workflow checkpoint. Non-empty
/// vectors must all belong to the same logical sequence. Phase B can later
/// extend this helper to stage the input command, domain events, effects, and
/// outbox messages without changing callers again.
fn commit_workflow_transition(
    rt: &mut Runtime,
    actor_id: u64,
    workflow_events: Vec<WorkflowEvent>,
    include_pending_command: bool,
    include_snapshot: bool,
) -> std::io::Result<()> {
    let expected_previous_sequence = rt.persistence.latest_sequence(actor_id);
    let sequence = match workflow_events.first() {
        Some(event) => event.sequence(),
        None => expected_previous_sequence
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("durable workflow sequence overflow"))?,
    };
    let expected_sequence = expected_previous_sequence
        .checked_add(1)
        .ok_or_else(|| std::io::Error::other("durable workflow sequence overflow"))?;
    if sequence != expected_sequence {
        let error = std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "workflow transition sequence {sequence} does not follow durable tail {expected_previous_sequence}"
            ),
        );
        mark_workflow_commit_failure(rt, actor_id);
        return Err(error);
    }
    if workflow_events
        .iter()
        .any(|event| event.sequence() != sequence)
    {
        let error = std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "all workflow events in one durable transition must share a sequence",
        );
        mark_workflow_commit_failure(rt, actor_id);
        return Err(error);
    }

    let snapshot = if include_snapshot {
        match build_actor_snapshot(rt, actor_id, sequence) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                mark_workflow_commit_failure(rt, actor_id);
                return Err(error);
            }
        }
    } else {
        None
    };
    let command = include_pending_command
        .then(|| pending_command_at_sequence(rt, actor_id, sequence))
        .flatten();
    let clear_pending_command =
        include_pending_command && rt.pending_workflow_commands.contains_key(&actor_id);
    let transition = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: activation_epoch(rt, actor_id),
        sequence,
        expected_previous_sequence,
        command,
        snapshot: snapshot.clone(),
        workflow_events,
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    };

    if let Err(error) = rt.persistence.commit_transition(transition) {
        mark_workflow_commit_failure(rt, actor_id);
        return Err(error);
    }

    // A command-caused transition completes ownership of the staged command.
    // External event-only transitions deliberately leave a recovered command
    // pending so the original activation can still finish.
    if clear_pending_command {
        rt.pending_workflow_commands.remove(&actor_id);
    }

    if let Some(snapshot) = snapshot.as_ref() {
        publish_committed_snapshot(rt, actor_id, snapshot);
    } else if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = sequence;
    }
    Ok(())
}

/// Commit one workflow event and the actor state produced by the same logical
/// turn as a single durable transition.
pub(crate) fn commit_workflow_event(
    rt: &mut Runtime,
    actor_id: u64,
    event: WorkflowEvent,
) -> std::io::Result<()> {
    commit_workflow_transition(rt, actor_id, vec![event], true, true)
}

/// Commit an externally-driven workflow event without attributing a suspended
/// command to it and without snapshotting partially-mutated in-memory state.
///
/// Recovery replays these events after the last committed snapshot.
fn commit_external_workflow_event(
    rt: &mut Runtime,
    actor_id: u64,
    event: WorkflowEvent,
) -> std::io::Result<()> {
    commit_workflow_transition(rt, actor_id, vec![event], false, false)
}

/// Persist a suspended workflow boundary without committing the step's
/// partially-mutated live state.
///
/// The new snapshot is derived from the previous committed snapshot, changing
/// only its sequence and wait marker. Any pending driving command joins this
/// transition so a crash after suspension cannot forget which mailbox command
/// entered the wait.
pub(crate) fn commit_suspension_marker(rt: &mut Runtime, actor_id: u64) -> std::io::Result<()> {
    let waiting_signal = match rt.actors.get(&actor_id) {
        Some(actor) if actor.persistent => actor.waiting_signal.clone(),
        _ => return Ok(()),
    };
    let Some(mut snapshot) = rt.persistence.load_snapshot(actor_id) else {
        return Ok(());
    };

    if snapshot.waiting_signal == waiting_signal
        && !rt.pending_workflow_commands.contains_key(&actor_id)
    {
        return Ok(());
    }

    let expected_previous_sequence = rt.persistence.latest_sequence(actor_id);
    let sequence = expected_previous_sequence
        .checked_add(1)
        .ok_or_else(|| std::io::Error::other("durable workflow sequence overflow"))?;
    snapshot.sequence = sequence;
    snapshot.waiting_signal = waiting_signal;

    let command = pending_command_at_sequence(rt, actor_id, sequence);
    let committed_pending_command = command.is_some();
    let transition = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: activation_epoch(rt, actor_id),
        sequence,
        expected_previous_sequence,
        command,
        snapshot: Some(snapshot.clone()),
        workflow_events: Vec::new(),
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    };

    if let Err(error) = rt.persistence.commit_transition(transition) {
        mark_workflow_commit_failure(rt, actor_id);
        return Err(error);
    }

    if committed_pending_command {
        rt.pending_workflow_commands.remove(&actor_id);
    }

    // The persisted state is intentionally the pre-step baseline. Do not clear
    // dirty fields on the live actor: its in-memory continuation still owns
    // those partial mutations until completion or activation recovery.
    rt.maybe_shadow_replicate(actor_id, &snapshot);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = sequence;
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
        let workflow_event = if event == "ParallelBranchCompleted" && args.len() == 2 {
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
            tracing::error!(
                actor_id,
                event,
                %error,
                "nulang-workflow: durable event transition failed"
            );
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
    commit_external_workflow_event(
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
    commit_external_workflow_event(
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
    if let Err(error) = append_signal_received(rt, actor_id, name, payload.clone()) {
        tracing::error!(
            actor_id,
            signal = name,
            %error,
            "nulang-workflow: refusing to deliver signal after durable commit failure"
        );
        rt.discard_and_recover_failed_workflow(actor_id);
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
        match append_timer_set(rt, actor_id, name, duration_ms) {
            Ok(()) => rt.rearm_timer(actor_id, name, duration_ms),
            Err(error) => {
                tracing::error!(
                    actor_id,
                    timer = name,
                    %error,
                    "nulang-workflow: refusing to arm timer after durable TimerSet commit failure"
                );
            }
        }
    } else {
        rt.rearm_timer(actor_id, name, duration_ms);
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
