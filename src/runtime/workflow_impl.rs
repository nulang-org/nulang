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

/// Return whether this actor has already entered RFC 0022 atomic history.
///
/// Unsupported backends are intentionally treated as legacy-only. Any other
/// storage error is propagated so callers do not silently cross persistence
/// modes after an atomic tail has begun.
pub(crate) fn workflow_has_atomic_tail(rt: &Runtime, actor_id: u64) -> std::io::Result<bool> {
    match rt.persistence.load_durable_tail_position(actor_id) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => Ok(false),
        Err(error) => Err(error),
    }
}

fn current_workflow_replay_id(rt: &mut Runtime, actor_id: u64) -> Option<WorkflowReplayEventId> {
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

fn advance_workflow_replay_id(rt: &mut Runtime, actor_id: u64, committed: WorkflowReplayEventId) {
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
enum ReplayDisposition {
    Append,
    Consume,
    Conflict,
}

fn committed_replay_event(
    rt: &Runtime,
    actor_id: u64,
    replay_id: WorkflowReplayEventId,
) -> Result<Option<WorkflowEvent>, ()> {
    let mut committed = None;
    for workflow_event in rt.persistence.read_workflow_events(actor_id) {
        if workflow_event.replay_id() != Some(replay_id) {
            continue;
        }
        if committed.is_some() {
            return Err(());
        }
        committed = Some(workflow_event);
    }
    Ok(committed)
}

fn custom_event_replay_disposition(
    rt: &Runtime,
    actor_id: u64,
    replay_id: WorkflowReplayEventId,
    event: &str,
    payload: &[PersistedValue],
) -> ReplayDisposition {
    match committed_replay_event(rt, actor_id, replay_id) {
        Ok(None) => ReplayDisposition::Append,
        Ok(Some(WorkflowEvent::Custom { name, args, .. }))
            if name == event && args.as_slice() == payload =>
        {
            ReplayDisposition::Consume
        }
        Ok(Some(_)) | Err(()) => ReplayDisposition::Conflict,
    }
}

fn timer_set_replay_disposition(
    rt: &Runtime,
    actor_id: u64,
    replay_id: WorkflowReplayEventId,
    name: &str,
    duration_ms: u64,
) -> ReplayDisposition {
    match committed_replay_event(rt, actor_id, replay_id) {
        Ok(None) => ReplayDisposition::Append,
        Ok(Some(WorkflowEvent::TimerSet {
            name: committed_name,
            duration_ms: committed_duration_ms,
            ..
        })) if committed_name == name && committed_duration_ms == duration_ms => {
            ReplayDisposition::Consume
        }
        Ok(Some(_)) | Err(()) => ReplayDisposition::Conflict,
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

/// Commit exactly one workflow event as the next RFC 0022 transition.
///
/// Intermediate activation records pass `snapshot_state = false` so the last
/// completed-state snapshot remains replay-safe. Terminal/post-terminal records
/// that establish a safe state boundary pass `true`.
fn commit_workflow_event_transition(
    rt: &mut Runtime,
    actor_id: u64,
    snapshot_state: bool,
    build_event: impl FnOnce(u64) -> WorkflowEvent,
) -> std::io::Result<()> {
    let expected_previous_sequence = rt.persistence.latest_sequence(actor_id);
    let sequence = expected_previous_sequence.checked_add(1).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workflow transition sequence overflow",
        )
    })?;
    let snapshot = if snapshot_state {
        Some(build_actor_snapshot_at_sequence(rt, actor_id, sequence)?.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "workflow transition requires a live persistent actor",
            )
        })?)
    } else {
        None
    };
    let activation_epoch = snapshot
        .as_ref()
        .map(|snapshot| snapshot.activation_epoch)
        .or_else(|| {
            rt.actors
                .get(&actor_id)
                .filter(|actor| actor.persistent)
                .map(|actor| actor.activation_epoch)
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "workflow transition requires a live persistent actor",
            )
        })?;

    rt.persistence.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch,
        sequence,
        expected_previous_sequence,
        command: None,
        snapshot: snapshot.clone(),
        workflow_events: vec![build_event(sequence)],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    })?;

    if let Some(snapshot) = snapshot.as_ref() {
        rt.maybe_shadow_replicate(actor_id, snapshot);
    }
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = sequence;
        if snapshot_state {
            actor.dirty_fields.clear();
        }
    }
    Ok(())
}

/// Persist one checkpoint for a durable actor.
///
/// Unlike the compatibility wrapper below, this function is fallible. Callers
/// that gate externally visible durable transitions (workflow creation, timer
/// commits, signals, compensation) must use this path so storage failure cannot
/// be mistaken for a committed transition.
pub(crate) fn try_checkpoint_actor(rt: &mut Runtime, actor_id: u64) -> std::io::Result<()> {
    if actor_is_workflow(rt, actor_id) && workflow_has_atomic_tail(rt, actor_id)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "legacy workflow checkpoint is forbidden after the RFC 0022 atomic tail begins",
        ));
    }

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
pub(crate) fn commit_step_completed(
    rt: &mut Runtime,
    actor_id: u64,
    activation: Option<WorkflowActivationId>,
    step_name: String,
) -> std::io::Result<()> {
    commit_workflow_event_transition(rt, actor_id, true, |sequence| {
        WorkflowEvent::StepCompleted {
            sequence,
            activation,
            step_name,
        }
    })
}

/// Atomically close a failed workflow activation together with the durable
/// state visible at the failure boundary.
pub(crate) fn commit_step_failed(
    rt: &mut Runtime,
    actor_id: u64,
    activation: Option<WorkflowActivationId>,
    step_name: String,
    error: String,
) -> std::io::Result<()> {
    commit_workflow_event_transition(rt, actor_id, true, |sequence| {
        WorkflowEvent::StepFailed {
            sequence,
            activation,
            step_name,
            error,
        }
    })
}

/// Persist successful workflow completion without crossing persistence modes.
pub(crate) fn persist_step_completed(
    rt: &mut Runtime,
    actor_id: u64,
    activation: Option<WorkflowActivationId>,
    step_name: String,
) -> std::io::Result<()> {
    if workflow_has_atomic_tail(rt, actor_id)? {
        return commit_step_completed(rt, actor_id, activation, step_name);
    }

    let sequence = next_sequence(rt, actor_id);
    rt.persistence.append_workflow_event(
        actor_id,
        WorkflowEvent::StepCompleted {
            sequence,
            activation,
            step_name,
        },
    )?;
    try_checkpoint_actor(rt, actor_id)
}

/// Persist failed workflow completion without crossing persistence modes.
pub(crate) fn persist_step_failed(
    rt: &mut Runtime,
    actor_id: u64,
    activation: Option<WorkflowActivationId>,
    step_name: String,
    error: String,
) -> std::io::Result<()> {
    if workflow_has_atomic_tail(rt, actor_id)? {
        return commit_step_failed(rt, actor_id, activation, step_name, error);
    }

    let sequence = next_sequence(rt, actor_id);
    rt.persistence.append_workflow_event(
        actor_id,
        WorkflowEvent::StepFailed {
            sequence,
            activation,
            step_name,
            error,
        },
    )?;
    try_checkpoint_actor(rt, actor_id)
}

/// Commit a nonterminal workflow event without moving the completed-state
/// snapshot. Once an atomic tail exists, legacy append APIs are forbidden.
fn commit_intermediate_workflow_event(
    rt: &mut Runtime,
    actor_id: u64,
    build_event: impl FnOnce(u64) -> WorkflowEvent,
) -> std::io::Result<()> {
    if workflow_has_atomic_tail(rt, actor_id)? {
        commit_workflow_event_transition(rt, actor_id, false, build_event)
    } else {
        let sequence = next_sequence(rt, actor_id);
        rt.persistence
            .append_workflow_event(actor_id, build_event(sequence))
    }
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
            let module = actor.bytecode_module.as_deref();
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
            let atomic_tail = match workflow_has_atomic_tail(rt, actor_id) {
                Ok(value) => value,
                Err(error) => {
                    tracing::error!(
                        actor_id,
                        %error,
                        "nulang-workflow: refusing parallel progress after atomic-tail read failed"
                    );
                    return;
                }
            };
            let committed = commit_intermediate_workflow_event(rt, actor_id, |sequence| {
                WorkflowEvent::ParallelBranchCompleted {
                    sequence,
                    parallel_step_name,
                    branch_name,
                }
            })
            .is_ok();
            if committed {
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    let current = actor
                        .get_state_field("parallel_progress")
                        .and_then(|v| v.as_int())
                        .unwrap_or(0);
                    actor.set_state_field("parallel_progress", Value::int(current + 1));
                }
                if !atomic_tail {
                    checkpoint_actor(rt, actor_id);
                }
            }
        } else {
            let module = rt
                .actors
                .get(&actor_id)
                .and_then(|a| a.bytecode_module.as_deref());
            let payload: Vec<PersistedValue> = args
                .iter()
                .map(|v| PersistedValue::from_value_resolved(v, module))
                .collect();
            let replay_id = current_workflow_replay_id(rt, actor_id);
            let mut should_checkpoint = false;
            if let Some(replay_id) = replay_id {
                match custom_event_replay_disposition(rt, actor_id, replay_id, event, &payload) {
                    ReplayDisposition::Consume => {
                        advance_workflow_replay_id(rt, actor_id, replay_id);
                    }
                    ReplayDisposition::Conflict => {
                        tracing::error!(
                            actor_id,
                            activation_actor_id = replay_id.activation.actor_id,
                            activation_command_sequence = replay_id.activation.command_sequence,
                            ordinal = replay_id.ordinal,
                            event,
                            "nulang-workflow: replay identity conflicts with committed custom event; refusing durable mutation"
                        );
                    }
                    ReplayDisposition::Append => {
                        let appended = commit_intermediate_workflow_event(
                            rt,
                            actor_id,
                            |sequence| WorkflowEvent::Custom {
                                sequence,
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
                            advance_workflow_replay_id(rt, actor_id, replay_id);
                        }
                    }
                }
            } else {
                let atomic_tail = match workflow_has_atomic_tail(rt, actor_id) {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::error!(
                            actor_id,
                            %error,
                            "nulang-workflow: refusing custom event after atomic-tail read failed"
                        );
                        return;
                    }
                };
                should_checkpoint = commit_intermediate_workflow_event(
                    rt,
                    actor_id,
                    |sequence| WorkflowEvent::Custom {
                        sequence,
                        replay_id: None,
                        name: event.to_string(),
                        args: payload,
                    },
                )
                .is_ok()
                    && !atomic_tail;
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
) -> std::io::Result<bool> {
    if let Some(replay_id) = current_workflow_replay_id(rt, actor_id) {
        match timer_set_replay_disposition(rt, actor_id, replay_id, name, duration_ms) {
            ReplayDisposition::Consume => {
                advance_workflow_replay_id(rt, actor_id, replay_id);
                return Ok(false);
            }
            ReplayDisposition::Conflict => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "workflow timer replay identity conflicts with committed event: activation {}:{} ordinal {}",
                        replay_id.activation.actor_id,
                        replay_id.activation.command_sequence,
                        replay_id.ordinal
                    ),
                ));
            }
            ReplayDisposition::Append => {
                commit_intermediate_workflow_event(rt, actor_id, |sequence| {
                    WorkflowEvent::TimerSet {
                        sequence,
                        replay_id: Some(replay_id),
                        name: name.to_string(),
                        duration_ms,
                    }
                })?;
                // TimerSet is an intermediate record for an open activation.
                // Keep the last completed snapshot unchanged so crash recovery
                // re-executes the command and consumes this exact identity.
                advance_workflow_replay_id(rt, actor_id, replay_id);
                return Ok(true);
            }
        }
    }

    if workflow_has_atomic_tail(rt, actor_id)? {
        commit_workflow_event_transition(rt, actor_id, false, |sequence| {
            WorkflowEvent::TimerSet {
                sequence,
                replay_id: None,
                name: name.to_string(),
                duration_ms,
            }
        })?;
        return Ok(true);
    }

    let seq = next_sequence(rt, actor_id);
    rt.persistence
        .append_timer_set(actor_id, seq, name.to_string(), duration_ms)?;
    try_checkpoint_actor(rt, actor_id)?;
    Ok(true)
}

pub(crate) fn append_timer_fired(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
) -> std::io::Result<()> {
    if workflow_has_atomic_tail(rt, actor_id)? {
        return commit_workflow_event_transition(rt, actor_id, false, |sequence| {
            WorkflowEvent::TimerFired {
                sequence,
                name: name.to_string(),
            }
        });
    }

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
    if workflow_has_atomic_tail(rt, actor_id)? {
        // SignalReceived is replay history, not a completed-state boundary.
        // The suspended activation will either resume and close atomically or
        // recover from the prior safe snapshot and consume the signal event.
        return commit_workflow_event_transition(rt, actor_id, false, |sequence| {
            WorkflowEvent::SignalReceived {
                sequence,
                name: name.to_string(),
                payload,
            }
        });
    }

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
    if workflow_has_atomic_tail(rt, actor_id)? {
        // Compensation runs after the failed activation is terminal. Its state
        // mutation and SagaCompensated marker therefore form a new safe
        // post-terminal boundary and may advance the snapshot atomically.
        return commit_workflow_event_transition(rt, actor_id, true, |sequence| {
            WorkflowEvent::SagaCompensated {
                sequence,
                step_name: step_name.to_string(),
            }
        });
    }

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
        (handler, actor.bytecode_module.as_deref().cloned()?)
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
    let should_arm = if actor_is_workflow(rt, actor_id) {
        // Never arm a live timer if its durable preparation failed. During
        // activation replay, a matching committed TimerSet is consumed and
        // recovery already owns the live timer reconstructed from history.
        append_timer_set(rt, actor_id, name, duration_ms)?
    } else {
        true
    };
    if should_arm {
        rt.rearm_timer(actor_id, name, duration_ms);
    }
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
