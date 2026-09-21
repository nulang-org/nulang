//! Durable workflow execution: event journaling, checkpointing, recovery,
//! signal routing, and timer scheduling.
//!
//! All functions in this module take `&Runtime` or `&mut Runtime` to access
//! the runtime's public fields. They live here instead of on `impl Runtime`
//! to keep the god-object at a manageable size.

use crate::bytecode::Constant;
use crate::primitives::ActorRole;
use crate::runtime::actor::Actor;
use crate::runtime::persistence::{EventEntry, PersistedValue, WorkflowEvent};
use crate::runtime::{
    QueryDistributedCallbacks, QueryPurityGuard, Runtime, StateModel, StateReadSet,
};
use crate::vm::{Frame, Value, VM};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowQueryError {
    ActorNotFound(u64),
    NotWorkflow(u64),
    HandlerNotFound { actor_id: u64, name: String },
    MissingModule(u64),
    InvalidHandler(String),
    PurityViolation { operation: String },
    Execution(String),
}

impl fmt::Display for WorkflowQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkflowQueryError::ActorNotFound(actor_id) => {
                write!(f, "workflow actor {actor_id} not found")
            }
            WorkflowQueryError::NotWorkflow(actor_id) => {
                write!(f, "actor {actor_id} is not a workflow")
            }
            WorkflowQueryError::HandlerNotFound { actor_id, name } => {
                write!(f, "workflow actor {actor_id} has no query handler '{name}'")
            }
            WorkflowQueryError::MissingModule(actor_id) => {
                write!(f, "workflow actor {actor_id} has no bytecode module")
            }
            WorkflowQueryError::InvalidHandler(message) => {
                write!(f, "invalid workflow query handler: {message}")
            }
            WorkflowQueryError::PurityViolation { operation } => {
                write!(f, "workflow query attempted forbidden operation {operation}")
            }
            WorkflowQueryError::Execution(message) => {
                write!(f, "workflow query execution failed: {message}")
            }
        }
    }
}

impl std::error::Error for WorkflowQueryError {}

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

/// Snapshot the durable and CRDT state of a persistent actor.
pub(crate) fn checkpoint_actor(rt: &mut Runtime, actor_id: u64) {
    let actor = match rt.actors.get(&actor_id) {
        Some(a) => a,
        None => return,
    };
    if !actor.persistent {
        return;
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
    let authority_tokens = match actor.authority_manifest() {
        Ok(manifest) => manifest.canonical_token_set(),
        Err(err) => {
            tracing::warn!(
                "nulang-persist: refusing to checkpoint actor {} with invalid authority: {}",
                actor_id,
                err
            );
            return;
        }
    };
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
        crdt_snapshot,
        crdt_field_map,
        authority_tokens,
    };
    // RFC 0014 §3: re-spawn-opted actors replicate the snapshot to their
    // deterministic shadow node before the local save, so the replica is a
    // byte-identical copy of exactly what the local store will hold.
    rt.maybe_shadow_replicate(actor_id, &snapshot);
    let _ = rt.persistence.save_snapshot(snapshot);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = seq;
        actor.dirty_fields.clear();
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
) -> std::io::Result<()> {
    let seq = next_sequence(rt, actor_id);
    rt.persistence
        .append_timer_set(actor_id, seq, name.to_string(), duration_ms)?;
    checkpoint_actor(rt, actor_id);
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
    checkpoint_actor(rt, actor_id);
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
    checkpoint_actor(rt, actor_id);
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
    checkpoint_actor(rt, actor_id);
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
///
/// Compatibility API: all lookup, purity, and execution failures map to
/// `None`. Call `query_workflow_checked` when the distinction matters.
pub(crate) fn query_workflow(rt: &mut Runtime, actor_id: u64, name: &str) -> Option<Value> {
    query_workflow_checked(rt, actor_id, name).ok()
}

/// Checked workflow-query API used by subscriptions and nested pure queries.
pub(crate) fn query_workflow_checked(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
) -> Result<Value, WorkflowQueryError> {
    execute_workflow_query(rt, actor_id, name, false).map(|(value, _)| value)
}

/// Invoke a workflow query and collect field-level dependencies.
///
/// Compatibility API: failures map to `None`; use the checked variant when a
/// caller must distinguish an invalid query from an ordinary nil result.
pub(crate) fn query_workflow_with_dependencies(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
) -> Option<(Value, StateReadSet)> {
    query_workflow_with_dependencies_checked(rt, actor_id, name).ok()
}

pub(crate) fn query_workflow_with_dependencies_checked(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
) -> Result<(Value, StateReadSet), WorkflowQueryError> {
    let (value, reads) = execute_workflow_query(rt, actor_id, name, true)?;
    Ok((value, reads.unwrap_or_default()))
}

fn execute_workflow_query(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    track_dependencies: bool,
) -> Result<(Value, Option<StateReadSet>), WorkflowQueryError> {
    let (handler, module) = {
        let actor = rt
            .actors
            .get(&actor_id)
            .ok_or(WorkflowQueryError::ActorNotFound(actor_id))?;
        if !matches!(actor.role(), Ok(ActorRole::Workflow)) {
            return Err(WorkflowQueryError::NotWorkflow(actor_id));
        }
        let handler = actor.query_handlers.get(name).copied().ok_or_else(|| {
            WorkflowQueryError::HandlerNotFound {
                actor_id,
                name: name.to_string(),
            }
        })?;
        let module = actor
            .bytecode_module
            .clone()
            .ok_or(WorkflowQueryError::MissingModule(actor_id))?;
        (handler, module)
    };

    let self_ptr: *mut Runtime = rt;
    let mut vm = VM::new();
    vm.load_module(module);
    let offset = vm
        .function_offset_for_value(0, handler)
        .map_err(|error| WorkflowQueryError::InvalidHandler(error.to_string()))?;

    let guard = QueryPurityGuard::default();
    vm.set_actor_callbacks(Box::new(crate::runtime::BytecodeRuntimeCallbacks::new_query(
        self_ptr,
        actor_id,
        guard.clone(),
    )));
    vm.set_distributed_callbacks(Box::new(QueryDistributedCallbacks::new(guard.clone())));

    let mut frame = Frame::new(None, 0);
    frame.pc = offset;
    vm.set_current_frame(frame);

    if track_dependencies {
        rt.begin_reactive_query_tracking();
    }
    let result = vm.run_from(0, offset);
    let reads = track_dependencies
        .then(|| rt.finish_reactive_query_tracking().unwrap_or_default());

    if let Some(operation) = guard.take() {
        return Err(WorkflowQueryError::PurityViolation { operation });
    }

    let value = result.map_err(|error| WorkflowQueryError::Execution(error.to_string()))?;
    Ok((value, reads))
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


#[cfg(test)]
mod query_purity_tests {
    use super::*;
    use crate::bytecode::{CodeModule, Instruction, OpCode};
    use std::collections::HashMap;

    fn query_module() -> CodeModule {
        let mut module = CodeModule::new("query-purity-test");
        let field_idx = module.add_string_constant("count");
        let payload_idx = module.add_string_constant("payload");

        // function 0: read_count() -> self.count
        module.function_table.push(module.current_offset());
        module.function_local_counts.push(2);
        module.emit(Instruction::new3(
            OpCode::StateGet,
            ((field_idx >> 8) & 0xFF) as u8,
            (field_idx & 0xFF) as u8,
            1,
        ));
        module.emit(Instruction::new1(OpCode::RetVal, 1));

        // function 1: mutate_count() -> attempts self.count = nil, then reads.
        // Query callbacks must reject the StateSet before actor state changes.
        module.function_table.push(module.current_offset());
        module.function_local_counts.push(2);
        module.emit(Instruction::new3(
            OpCode::StateSet,
            ((field_idx >> 8) & 0xFF) as u8,
            (field_idx & 0xFF) as u8,
            0,
        ));
        module.emit(Instruction::new3(
            OpCode::StateGet,
            ((field_idx >> 8) & 0xFF) as u8,
            (field_idx & 0xFF) as u8,
            1,
        ));
        module.emit(Instruction::new1(OpCode::RetVal, 1));

        // function 2: mutate_array() -> attempts to mutate pointer-backed
        // actor state through the direct ArrStore opcode.
        module.function_table.push(module.current_offset());
        module.function_local_counts.push(3);
        module.emit(Instruction::new3(
            OpCode::StateGet,
            ((payload_idx >> 8) & 0xFF) as u8,
            (payload_idx & 0xFF) as u8,
            1,
        ));
        module.emit(Instruction::new1(OpCode::Const0, 2));
        module.emit(Instruction::new3(OpCode::ArrStore, 1, 2, 2));
        module.emit(Instruction::new1(OpCode::RetVal, 1));

        module
    }

    fn workflow_with_queries() -> (Runtime, u64) {
        let mut rt = Runtime::new();
        let mut models = HashMap::new();
        models.insert("count".to_string(), StateModel::Durable);
        models.insert("payload".to_string(), StateModel::Durable);
        let actor_id = rt.spawn_workflow_actor(
            "CounterWorkflow",
            Box::new(|| vec![("count".to_string(), Value::int(7))]),
            models,
        );
        {
            let actor = rt.actors.get_mut(&actor_id).unwrap();
            let payload = actor.allocate_array(vec![Value::int(99)]);
            actor.set_state_field("payload", payload);
            actor.bytecode_module = Some(query_module());
        }
        rt.register_workflow_query(actor_id, "read", Value::int(0));
        rt.register_workflow_query(actor_id, "mutate", Value::int(1));
        rt.register_workflow_query(actor_id, "mutate_array", Value::int(2));
        (rt, actor_id)
    }

    #[test]
    fn query_handler_reads_state_and_reports_dependencies() {
        let (mut rt, actor_id) = workflow_with_queries();

        let (value, reads) = rt
            .query_workflow_with_dependencies_checked(actor_id, "read")
            .expect("read-only query should succeed");

        assert_eq!(value.as_int(), Some(7));
        assert_eq!(reads.len(), 1);
        assert!(reads.revision(actor_id, "count").is_some());
        assert!(rt.state_read_set_is_current(&reads));
    }

    #[test]
    fn query_handler_state_write_is_rejected_without_mutating_actor() {
        let (mut rt, actor_id) = workflow_with_queries();

        let error = rt
            .query_workflow_checked(actor_id, "mutate")
            .expect_err("query state mutation must fail closed");

        assert_eq!(
            error,
            WorkflowQueryError::PurityViolation {
                operation: "State.set(count)".to_string(),
            }
        );
        assert_eq!(
            rt.actors
                .get(&actor_id)
                .and_then(|actor| actor.get_state_field("count"))
                .and_then(|value| value.as_int()),
            Some(7)
        );

        // Compatibility API preserves its historic failure-as-None shape.
        assert_eq!(rt.query_workflow(actor_id, "mutate"), None);
    }

    #[test]
    fn query_cannot_mutate_pointer_backed_actor_state_with_direct_opcode() {
        let (mut rt, actor_id) = workflow_with_queries();

        let error = rt
            .query_workflow_checked(actor_id, "mutate_array")
            .expect_err("direct heap mutation of actor state must fail closed");
        assert_eq!(
            error,
            WorkflowQueryError::PurityViolation {
                operation: "Array.store".to_string(),
            }
        );

        let payload = rt
            .actors
            .get(&actor_id)
            .unwrap()
            .get_state_field("payload")
            .and_then(|value| value.as_ptr())
            .expect("payload should remain an array pointer");
        let first = unsafe { *(payload as *const Value) };
        assert_eq!(first.as_int(), Some(99));
    }

    #[test]
    fn checked_query_distinguishes_missing_handler_from_nil_result() {
        let (mut rt, actor_id) = workflow_with_queries();

        assert_eq!(
            rt.query_workflow_checked(actor_id, "missing"),
            Err(WorkflowQueryError::HandlerNotFound {
                actor_id,
                name: "missing".to_string(),
            })
        );
        assert_eq!(rt.query_workflow(actor_id, "missing"), None);
    }
}
