//! Durable workflow facade.
//!
//! The replay-aware implementation lives in `workflow_impl.rs`. Keep this
//! facade so existing runtime callers retain their established signatures while
//! timer scheduling can use the implementation's internal arm/consume result.

#[path = "workflow_impl.rs"]
mod workflow_impl;

pub(crate) use workflow_impl::{
    actor_is_workflow, append_saga_compensated, append_signal_received, append_timer_fired,
    checkpoint_actor, commit_step_completed, commit_workflow_command, emit_event,
    finish_resumed_workflow_step, next_sequence,
    query_workflow, register_workflow_query, schedule_workflow_timer, signal_workflow,
    try_checkpoint_actor,
};

#[allow(dead_code)]
pub(crate) fn vm_value_to_string_in_actor(
    value: &crate::vm::Value,
    actor: &crate::runtime::actor::Actor,
) -> Option<String> {
    workflow_impl::vm_value_to_string_in_actor(value, actor)
}

/// Append a durable `TimerSet` record while preserving the established runtime
/// API. The replay-aware implementation additionally reports whether a live
/// timer should be armed; only `schedule_workflow_timer` consumes that signal.
pub(crate) fn append_timer_set(
    rt: &mut crate::runtime::Runtime,
    actor_id: u64,
    name: &str,
    duration_ms: u64,
) -> std::io::Result<()> {
    workflow_impl::append_timer_set(rt, actor_id, name, duration_ms).map(|_| ())
}
