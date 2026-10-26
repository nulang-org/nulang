//! Native concurrent, fuel-limited CoreVM children joined through TaskScope.
//!
//! This is an opt-in integration of two existing primitives: one fresh CoreVM
//! per child and the bounded native `run_scoped_threads` host. It does not
//! modify source-language `par` lowering or create a VM/WASM scheduler.

use crate::core_vm::{CoreVM, Value};
use crate::runtime::CancellationToken;
use crate::scoped_task_core_vm::{poll_core_vm_once, ScopedCoreVmTask};
use crate::scoped_task_reference::TaskPoll;
use crate::scoped_tasks::TaskOutcome;
use crate::scoped_thread_host::{
    run_scoped_threads, ScopedThreadError, ScopedThreadExit, ScopedThreadTask,
};

/// Run isolated bytecode children concurrently on bounded native threads.
///
/// Each callback creates and destroys its own `CoreVM`, borrows immutable
/// bytecode from the caller, and executes at most `instruction_budget`
/// instructions. Existing instruction-boundary cancellation sampling is used
/// unchanged. Completion or cancellation is acknowledged only after VM
/// execution terminates, and all launched OS threads join before return.
///
/// `max_in_flight` limits concurrent workers, and `max_pending` bounds the
/// entire initial task set. The first reported VM error cancels queued tasks
/// and signals running siblings; terminal results retain submission order.
/// Cooperative fuel/cancel checks cannot preempt a blocking host operation.
///
/// This opt-in native API is not the source-language `par` scheduler. It
/// does not use wasm32 threads, provide resumable bytecode or persistence.
pub fn run_parallel_core_vm_scope(
    tasks: &[ScopedCoreVmTask<'_>],
    max_in_flight: usize,
    max_pending: usize,
    instruction_budget: u64,
    parent: &CancellationToken,
) -> Result<Vec<TaskOutcome<Value, String>>, ScopedThreadError> {
    let children: Vec<ScopedThreadTask<'_, Value, String>> = tasks
        .iter()
        .map(|child| {
            let module = child.module;
            let entry_pc = child.entry_pc;
            Box::new(move |cancellation| {
                let mut vm = CoreVM::new();
                let result = match vm.load_module_from_code(module) {
                    Ok(module_idx) => poll_core_vm_once(
                        &mut vm,
                        module_idx,
                        entry_pc,
                        instruction_budget,
                        &cancellation,
                    ),
                    Err(error) => TaskPoll::Ready(Err(error)),
                };
                match result {
                    TaskPoll::Ready(value) => ScopedThreadExit::Completed(value),
                    TaskPoll::Cancelled => ScopedThreadExit::Cancelled,
                    TaskPoll::Pending => {
                        unreachable!("one-shot CoreVM must reach a terminal state")
                    }
                }
            }) as ScopedThreadTask<'_, Value, String>
        })
        .collect();
    run_scoped_threads(children, max_in_flight, max_pending, parent)
}
