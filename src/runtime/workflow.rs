//! Durable workflow execution: event journaling, checkpointing, recovery,
//! signal routing, and timer scheduling.
//!
//! All functions in this module take `&Runtime` or `&mut Runtime` to access
//! the runtime's public fields. They live here instead of on `impl Runtime`
//! to keep the god-object at a manageable size.

use crate::bytecode::Constant;
use crate::primitives::ActorRole;
use crate::runtime::actor::{Actor, WorkflowActivationContext};
use crate::runtime::persistence::{
    EventEntry, JournalEntry, PersistedValue, WorkflowActivationId, WorkflowEvent,
    WorkflowOperationId,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowActivationTerminal {
    Completed { event_sequence: u64 },
    Failed { event_sequence: u64 },
}

#[derive(Debug, Clone)]
pub struct WorkflowActivationRecord {
    pub id: WorkflowActivationId,
    pub command: JournalEntry,
    pub terminal: Option<WorkflowActivationTerminal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowActivationAnalysisError {
    InvalidCommandIdentity {
        actor_id: u64,
        command_sequence: u64,
        tagged_actor_id: u64,
        tagged_command_sequence: u64,
    },
    ForeignTerminalIdentity {
        event_sequence: u64,
        actor_id: u64,
    },
    DuplicateTerminal {
        command_sequence: u64,
    },
    UntaggedTerminalAfterActivationUpgrade {
        event_sequence: u64,
    },
}

impl std::fmt::Display for WorkflowActivationAnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCommandIdentity {
                actor_id,
                command_sequence,
                tagged_actor_id,
                tagged_command_sequence,
            } => write!(
                f,
                "workflow actor {actor_id} journal command {command_sequence} carries invalid activation id ({tagged_actor_id}, {tagged_command_sequence})"
            ),
            Self::ForeignTerminalIdentity {
                event_sequence,
                actor_id,
            } => write!(
                f,
                "workflow terminal event {event_sequence} refers to foreign actor {actor_id}"
            ),
            Self::DuplicateTerminal { command_sequence } => write!(
                f,
                "workflow activation command {command_sequence} has more than one terminal event"
            ),
            Self::UntaggedTerminalAfterActivationUpgrade { event_sequence } => write!(
                f,
                "workflow terminal event {event_sequence} has no activation id after activation-aware commands begin"
            ),
        }
    }
}

impl std::error::Error for WorkflowActivationAnalysisError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowOperationAnalysisError {
    ActorMismatch {
        actor_id: u64,
        operation_id: WorkflowOperationId,
    },
    ForeignOperationIdentity {
        event_sequence: u64,
        operation_id: WorkflowOperationId,
    },
    DuplicateOperationIdentity {
        operation_id: WorkflowOperationId,
    },
}

impl std::fmt::Display for WorkflowOperationAnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ActorMismatch {
                actor_id,
                operation_id,
            } => write!(
                f,
                "workflow operation {:?} does not belong to actor {actor_id}",
                operation_id
            ),
            Self::ForeignOperationIdentity {
                event_sequence,
                operation_id,
            } => write!(
                f,
                "workflow event {event_sequence} carries foreign operation identity {:?}",
                operation_id
            ),
            Self::DuplicateOperationIdentity { operation_id } => write!(
                f,
                "workflow operation {:?} appears more than once in durable event history",
                operation_id
            ),
        }
    }
}

impl std::error::Error for WorkflowOperationAnalysisError {}

/// Find the durable event recorded for one activation-local operation.
///
/// Recovery uses this lookup instead of relying on workflow-event sequence
/// position. Duplicate identities are rejected because replay could otherwise
/// consume an arbitrary record and silently diverge.
pub fn find_workflow_event_for_operation(
    actor_id: u64,
    operation_id: WorkflowOperationId,
    events: &[WorkflowEvent],
) -> Result<Option<WorkflowEvent>, WorkflowOperationAnalysisError> {
    if operation_id.activation.actor_id != actor_id {
        return Err(WorkflowOperationAnalysisError::ActorMismatch {
            actor_id,
            operation_id,
        });
    }

    let mut found = None;
    for event in events {
        let Some(candidate) = event.operation_id() else {
            continue;
        };
        if candidate.activation.actor_id != actor_id {
            return Err(WorkflowOperationAnalysisError::ForeignOperationIdentity {
                event_sequence: event.sequence(),
                operation_id: candidate,
            });
        }
        if candidate != operation_id {
            continue;
        }
        if found.is_some() {
            return Err(WorkflowOperationAnalysisError::DuplicateOperationIdentity {
                operation_id,
            });
        }
        found = Some(event.clone());
    }
    Ok(found)
}

/// Build an activation index from durable command and workflow journals.
///
/// Only journal entries explicitly tagged with an activation id are considered
/// activation-opening user commands. This excludes legacy entries and internal
/// runtime messages. Once activation-aware commands begin, an untagged terminal
/// event is ambiguous and therefore rejected instead of guessed.
pub fn analyze_workflow_activations(
    actor_id: u64,
    journal: &[JournalEntry],
    events: &[WorkflowEvent],
) -> Result<Vec<WorkflowActivationRecord>, WorkflowActivationAnalysisError> {
    let mut commands = Vec::new();
    let mut first_activation_sequence = None;

    for command in journal {
        let Some(id) = command.activation_id else {
            continue;
        };
        if id.actor_id != actor_id || id.command_sequence != command.sequence {
            return Err(WorkflowActivationAnalysisError::InvalidCommandIdentity {
                actor_id,
                command_sequence: command.sequence,
                tagged_actor_id: id.actor_id,
                tagged_command_sequence: id.command_sequence,
            });
        }
        first_activation_sequence = Some(
            first_activation_sequence
                .map(|current: u64| current.min(command.sequence))
                .unwrap_or(command.sequence),
        );
        commands.push(command.clone());
    }

    let Some(first_activation_sequence) = first_activation_sequence else {
        return Ok(Vec::new());
    };

    let mut terminals = std::collections::BTreeMap::new();
    for event in events {
        if !event.is_terminal() {
            continue;
        }
        let Some(id) = event.terminal_activation_id() else {
            if event.sequence() >= first_activation_sequence {
                return Err(
                    WorkflowActivationAnalysisError::UntaggedTerminalAfterActivationUpgrade {
                        event_sequence: event.sequence(),
                    },
                );
            }
            continue;
        };
        if id.actor_id != actor_id {
            return Err(WorkflowActivationAnalysisError::ForeignTerminalIdentity {
                event_sequence: event.sequence(),
                actor_id: id.actor_id,
            });
        }
        let terminal = match event {
            WorkflowEvent::StepCompleted { .. } => WorkflowActivationTerminal::Completed {
                event_sequence: event.sequence(),
            },
            WorkflowEvent::StepFailed { .. } => WorkflowActivationTerminal::Failed {
                event_sequence: event.sequence(),
            },
            _ => unreachable!("terminal predicate only matches terminal workflow events"),
        };
        if terminals.insert(id.command_sequence, terminal).is_some() {
            return Err(WorkflowActivationAnalysisError::DuplicateTerminal {
                command_sequence: id.command_sequence,
            });
        }
    }

    commands.sort_by_key(|command| command.sequence);
    Ok(commands
        .into_iter()
        .map(|command| {
            let id = command
                .activation_id
                .expect("activation commands were filtered above");
            WorkflowActivationRecord {
                id,
                terminal: terminals.get(&id.command_sequence).copied(),
                command,
            }
        })
        .collect())
}

pub(crate) fn begin_workflow_activation(
    rt: &mut Runtime,
    id: WorkflowActivationId,
    replaying: bool,
) {
    if let Some(actor) = rt.actors.get_mut(&id.actor_id) {
        actor.workflow_activation = Some(WorkflowActivationContext::new(id, replaying));
    }
}

pub(crate) fn current_workflow_activation_id(
    rt: &Runtime,
    actor_id: u64,
) -> Option<WorkflowActivationId> {
    rt.actors
        .get(&actor_id)
        .and_then(|actor| actor.workflow_activation.map(|context| context.id))
}

pub(crate) fn next_workflow_operation_id(
    rt: &mut Runtime,
    actor_id: u64,
) -> Option<WorkflowOperationId> {
    rt.actors
        .get_mut(&actor_id)
        .and_then(|actor| actor.workflow_activation.as_mut())
        .map(WorkflowActivationContext::next_operation_id)
}


/// Begin or re-enter one suspended signal wait.
///
/// A re-execution of the same pending wait reuses its identity; only a new
/// logical wait consumes the next activation-local operation ordinal.
pub(crate) fn begin_workflow_signal_wait(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
) -> Option<WorkflowOperationId> {
    if let Some(actor) = rt.actors.get(&actor_id) {
        if actor.waiting_signal.as_deref() == Some(name) {
            if let Some(operation_id) = actor.waiting_signal_operation {
                return Some(operation_id);
            }
        }
    }

    let operation_id = next_workflow_operation_id(rt, actor_id);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.waiting_signal = Some(name.to_string());
        actor.waiting_signal_operation = operation_id;
    }
    operation_id
}

fn close_workflow_activation(rt: &mut Runtime, actor_id: u64, id: Option<WorkflowActivationId>) {
    let Some(id) = id else {
        return;
    };
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        if actor
            .workflow_activation
            .map(|context| context.id == id)
            .unwrap_or(false)
        {
            actor.workflow_activation = None;
        }
    }
}

pub(crate) fn append_step_completed(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: String,
) -> std::io::Result<()> {
    let activation_id = current_workflow_activation_id(rt, actor_id);
    let sequence = next_sequence(rt, actor_id);
    rt.persistence.append_workflow_event(
        actor_id,
        WorkflowEvent::StepCompleted {
            sequence,
            activation_id,
            step_name,
        },
    )?;
    close_workflow_activation(rt, actor_id, activation_id);
    Ok(())
}

pub(crate) fn append_step_failed(
    rt: &mut Runtime,
    actor_id: u64,
    step_name: String,
    error: String,
) -> std::io::Result<()> {
    let activation_id = current_workflow_activation_id(rt, actor_id);
    let sequence = next_sequence(rt, actor_id);
    rt.persistence.append_workflow_event(
        actor_id,
        WorkflowEvent::StepFailed {
            sequence,
            activation_id,
            step_name,
            error,
        },
    )?;
    close_workflow_activation(rt, actor_id, activation_id);
    Ok(())
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
        Some(a) => a,
        None => return Ok(()),
    };
    if !actor.persistent {
        return Ok(());
    }
    let seq = next_sequence(rt, actor_id);
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
    // Snapshot the global CRDT state alongside durable actor fields.
    let crdt_snapshot = rt.crdt_manager.as_ref().map(|m| {
        m.snapshot()
            .into_iter()
            .map(|(id, (ty, bytes))| (id.0, ty.to_u8(), bytes))
            .collect()
    });
    let crdt_field_map = rt.crdt_manager.as_ref().map(|m| {
        m.field_map
            .iter()
            .filter(|((aid, _), _)| *aid == actor_id)
            .map(|((_, name), id)| (name.clone(), id.0))
            .collect()
    });
    let snapshot = crate::runtime::persistence::ActorSnapshot {
        actor_id,
        sequence: seq,
        state,
        waiting_signal: actor.waiting_signal.clone(),
        waiting_signal_operation: actor.waiting_signal_operation,
        crdt_snapshot,
        crdt_field_map,
        authority_tokens,
    };
    // The local persistence store is authoritative. Publish a shadow replica
    // only after the local snapshot commit succeeds; otherwise a failed local
    // checkpoint could leave a remote replica for an actor/transition that was
    // never durably committed at home.
    rt.persistence.save_snapshot(snapshot.clone())?;
    rt.maybe_shadow_replicate(actor_id, &snapshot);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = seq;
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
    let operation_id = if is_workflow {
        next_workflow_operation_id(rt, actor_id)
    } else {
        None
    };
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
            let _ = rt.persistence.append_workflow_event(
                actor_id,
                WorkflowEvent::ParallelBranchCompleted {
                    sequence: seq,
                    operation_id,
                    parallel_step_name,
                    branch_name,
                },
            );
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                let current = actor
                    .get_state_field("parallel_progress")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                actor.set_state_field("parallel_progress", Value::int(current + 1));
            }
        } else {
            let module = rt
                .actors
                .get(&actor_id)
                .and_then(|a| a.bytecode_module.as_ref());
            let payload: Vec<PersistedValue> = args
                .iter()
                .map(|v| PersistedValue::from_value_resolved(v, module))
                .collect();
            let _ = rt.persistence.append_workflow_event(
                actor_id,
                WorkflowEvent::Custom {
                    sequence: seq,
                    operation_id,
                    name: event.to_string(),
                    args: payload,
                },
            );
        }
        checkpoint_actor(rt, actor_id);
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
) -> std::io::Result<Option<WorkflowOperationId>> {
    let operation_id = next_workflow_operation_id(rt, actor_id);
    let seq = next_sequence(rt, actor_id);
    rt.persistence.append_workflow_event(
        actor_id,
        WorkflowEvent::TimerSet {
            sequence: seq,
            operation_id,
            name: name.to_string(),
            duration_ms,
        },
    )?;
    try_checkpoint_actor(rt, actor_id)?;
    Ok(operation_id)
}

pub(crate) fn append_timer_fired(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    operation_id: Option<WorkflowOperationId>,
) -> std::io::Result<()> {
    let seq = next_sequence(rt, actor_id);
    rt.persistence.append_workflow_event(
        actor_id,
        WorkflowEvent::TimerFired {
            sequence: seq,
            operation_id,
            name: name.to_string(),
        },
    )?;
    try_checkpoint_actor(rt, actor_id)?;
    Ok(())
}

pub(crate) fn append_signal_received(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    payload: Option<String>,
    operation_id: Option<WorkflowOperationId>,
) -> std::io::Result<()> {
    let seq = next_sequence(rt, actor_id);
    rt.persistence.append_workflow_event(
        actor_id,
        WorkflowEvent::SignalReceived {
            sequence: seq,
            operation_id,
            name: name.to_string(),
            payload,
        },
    )?;
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
    // its durable journal write and checkpoint both succeeded. This remains
    // the legacy two-write path until activation replay (#836) lets workflow
    // events move safely onto RFC 0022's atomic transition tail.
    let operation_id = rt.actors.get(&actor_id).and_then(|actor| {
        if actor.waiting_signal.as_deref() == Some(name) {
            actor.waiting_signal_operation
        } else {
            None
        }
    });
    append_signal_received(rt, actor_id, name, payload.clone(), operation_id)?;

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
    let operation_id = if actor_is_workflow(rt, actor_id) {
        // Never arm a live timer if its durable TimerSet/checkpoint failed.
        // Recovery can only reason about timers that were durably recorded.
        append_timer_set(rt, actor_id, name, duration_ms)?
    } else {
        None
    };
    rt.rearm_timer(actor_id, name, duration_ms, operation_id);
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
