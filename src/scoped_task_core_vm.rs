//! Finite one-shot CoreVM host for the experimental scoped-task coordinator.
//!
//! CoreVM stays executor-independent (including the browser playground).
//! This adapter translates a bounded, terminated VM run into a task poll.
//! A cancelled run or exhausted instruction budget is NOT resumable: callers
//! must discard that VM instance, never invoke it again as a continuation.
//! No parallelism, child scheduling or durable workflow semantics are implied.

use crate::bytecode::CodeModule;
use crate::core_vm::{CoreVM, Value};
use crate::runtime::CancellationToken;
use crate::scoped_task_reference::{ReferenceExecutor, TaskPoll};
use crate::scoped_tasks::{ScopeError, TaskCancellation, TaskOutcome};

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

/// Borrowed bytecode for a finite child executed inside a lexical scope.
///
/// A child uses a fresh, private CoreVM. Module and entrypoint references are
/// valid throughout the synchronous join; no worker thread is spawned.
#[derive(Clone, Copy)]
pub struct ScopedCoreVmTask<'a> {
    pub module: &'a CodeModule,
    pub entry_pc: usize,
}

/// Execute all submitted children and join before this function can return.
///
/// This is a bounded, sequential reference path. At most `max_in_flight`
/// children are admitted per tick, and the entire submitted backlog is capped
/// by `max_pending`. Every admitted child runs one CoreVM invocation with
/// finite `instruction_budget` fuel. A failing child cancels not-yet-started
/// siblings; all accepted child outcomes are returned in submission order.
///
/// An admission failure returns before executing any child. A successful
/// return proves all accepted children are terminal and their private VMs
/// have been dropped. This does NOT implement concurrent `par` semantics:
/// a blocking host instruction still cannot be preempted by the fuel limit.
pub fn run_bounded_core_vm_scope(
    tasks: &[ScopedCoreVmTask<'_>],
    max_in_flight: usize,
    max_pending: usize,
    instruction_budget: u64,
    parent: &CancellationToken,
) -> Result<Vec<TaskOutcome<Value, String>>, ScopeError> {
    let mut runner =
        ReferenceExecutor::<Value, String>::with_parent(max_in_flight, max_pending, parent)?;
    if tasks.len() > max_pending {
        return Err(ScopeError::QueueFull);
    }

    // Register the entire bounded task set before allowing any execution.
    // A canceled parent therefore cannot cause partial task execution during
    // the admission phase.
    for _ in tasks {
        runner.try_submit()?;
    }

    while !runner.is_quiescent() {
        let stats = runner.tick(|id, cancellation| {
            let child = &tasks[id.0];
            let mut vm = CoreVM::new();
            match vm.load_module_from_code(child.module) {
                Ok(module_idx) => poll_core_vm_once(
                    &mut vm,
                    module_idx,
                    child.entry_pc,
                    instruction_budget,
                    cancellation,
                ),
                Err(error) => TaskPoll::Ready(Err(error)),
            }
        });

        // The one-shot CoreVM adapter always returns Ready or Cancelled.
        // Never mistake a future Pending implementation for termination.
        if stats.completed == 0 && !runner.is_quiescent() {
            return Err(ScopeError::NotQuiescent);
        }
    }

    runner.try_join()
}
