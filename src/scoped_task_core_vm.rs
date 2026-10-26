//! Finite one-shot CoreVM host for the experimental scoped-task coordinator.
//!
//! CoreVM stays executor-independent (including the browser playground).
//! This adapter translates a bounded, terminated VM run into a task poll.
//! A cancelled run or exhausted instruction budget is NOT resumable: callers
//! must discard that VM instance, never invoke it again as a continuation.
//! No parallelism, child scheduling or durable workflow semantics are implied.

use crate::core_vm::{CoreVM, Value};
use crate::scoped_task_reference::TaskPoll;
use crate::scoped_tasks::TaskCancellation;

/// Execute one finite child to termination or its instruction budget.
///
/// `Cancelled` is returned only after the VM's execution method has
/// returned (or when cancellation was already requested before entry).
/// Thus the scope's executor can acknowledge actual termination, not merely
/// cancellation intent. External blocking effects are not preemptible.
pub fn poll_core_vm_once(
    vm: &mut CoreVM,
    module_idx: usize,
    entry_pc: usize,
    instruction_budget: u64,
    cancellation: &TaskCancellation,
) -> TaskPoll<Value, String> {
    if cancellation.is_cancelled() {
        return TaskPoll::Cancelled;
    }

    match vm.run_with_limits(module_idx, entry_pc, instruction_budget, || {
        cancellation.is_cancelled()
    }) {
        Ok(value) => TaskPoll::Ready(Ok(value)),
        Err(error) if error == "CoreVM execution cancelled" && cancellation.is_cancelled() => {
            TaskPoll::Cancelled
        }
        Err(error) => TaskPoll::Ready(Err(error)),
    }
}
