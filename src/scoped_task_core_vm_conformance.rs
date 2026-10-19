//! Experimental CoreVM execution through the scoped-task reference contract.
//!
//! TEST ONLY: bytecode tasks are pure, non-suspending computations executed
//! synchronously on isolated VM instances. This is not parallel par lowering,
//! a cooperative VM instruction scheduler, or a production worker runtime.

use std::collections::HashMap;

use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
use crate::core_vm::CoreVM;
use crate::scoped_task_reference::{ReferenceExecutor, TaskPoll, TickStats};
use crate::scoped_tasks::{CancellationCause, TaskId, TaskOutcome};
use crate::value_layout;

/// Test-only adapter: runs each admitted pure bytecode task to completion
/// in an isolated CoreVM, on the caller's thread. It cannot preempt the VM.
struct CoreVmTaskHost {
    executor: ReferenceExecutor<i64, String>,
    modules: HashMap<TaskId, CodeModule>,
    deferred_once: Option<TaskId>,
    executed: Vec<TaskId>,
}

impl CoreVmTaskHost {
    fn new(max_in_flight: usize, max_pending: usize) -> Self {
        Self {
            executor: ReferenceExecutor::new(max_in_flight, max_pending)
                .expect("positive test limits"),
            modules: HashMap::new(),
            deferred_once: None,
            executed: Vec::new(),
        }
    }

    fn submit_add(&mut self, left: i64, right: i64) -> TaskId {
        // Deliberately use a small, known-pure CoreVM instruction sequence.
        // No actor state, effect handlers, callbacks, or FFI cross this seam.
        let mut module = CodeModule::new("scoped_test_add");
        module.constants.push(Constant::Int(left));
        module.constants.push(Constant::Int(right));
        module.emit(Instruction::new3(OpCode::ConstU, 0, 0, 0));
        module.emit(Instruction::new3(OpCode::ConstU, 0, 1, 1));
        module.emit(Instruction::new3(OpCode::IAdd, 0, 1, 0));
        module.emit(Instruction::new0(OpCode::Halt));

        let id = self.executor.try_submit().expect("within test queue capacity");
        self.modules.insert(id, module);
        id
    }

    fn defer_once(&mut self, id: TaskId) {
        self.deferred_once = Some(id);
    }

    fn tick(&mut self) -> TickStats {
        let modules = &self.modules;
        let deferred_once = &mut self.deferred_once;
        let executed = &mut self.executed;
        self.executor.tick(|id, token| {
            // Cancellation can prevent execution only before starting a
            // non-preemptible CoreVM call, not interrupt one already running.
            if token.is_cancelled() {
                return TaskPoll::Cancelled;
            }
            if *deferred_once == Some(id) {
                *deferred_once = None;
                return TaskPoll::Pending;
            }

            let module = modules.get(&id).expect("every admitted task has bytecode");
            let mut vm = CoreVM::new();
            let result = vm
                .load_module_from_code(module)
                .and_then(|idx| vm.run(idx, module.entry_point.unwrap_or(0)))
                .and_then(|value| {
                    if value_layout::is_int_raw(value) {
                        Ok(value_layout::as_int_raw(value))
                    } else {
                        Err("expected an integer CoreVM result".to_string())
                    }
                });
            executed.push(id);
            TaskPoll::Ready(result)
        })
    }

    fn cancel(&mut self, reason: CancellationCause) {
        self.executor.cancel(reason);
    }

    fn executed(&self) -> &[TaskId] {
        &self.executed
    }

    fn into_outcomes(self) -> Vec<TaskOutcome<i64, String>> {
        self.executor
            .into_outcomes()
            .expect("all test tasks reached a terminal outcome")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoped_task_reference::TickStats;
    use crate::scoped_tasks::{CancellationCause, TaskOutcome};

    #[test]
    fn bytecode_results_join_in_submission_order_despite_delayed_first_task() {
        let mut host = CoreVmTaskHost::new(2, 2);
        let first = host.submit_add(7, 5);
        let second = host.submit_add(20, 2);
        host.defer_once(first);

        assert_eq!(
            host.tick(),
            TickStats {
                admitted: 2,
                polled: 2,
                completed: 1,
            }
        );
        assert_eq!(host.executed(), &[second]);
        assert_eq!(
            host.tick(),
            TickStats {
                admitted: 0,
                polled: 1,
                completed: 1,
            }
        );
        assert_eq!(host.executed(), &[second, first]);
        assert_eq!(
            host.into_outcomes(),
            vec![TaskOutcome::Completed(Ok(12)), TaskOutcome::Completed(Ok(22))]
        );
    }

    #[test]
    fn parent_cancellation_before_admission_prevents_any_core_vm_execution() {
        let mut host = CoreVmTaskHost::new(2, 2);
        host.submit_add(3, 4);
        host.submit_add(5, 6);

        host.cancel(CancellationCause::Parent);

        let stats = host.tick();
        assert_eq!(stats.admitted, 0);
        assert_eq!(stats.polled, 0);
        assert!(host.executed().is_empty());
        assert_eq!(
            host.into_outcomes(),
            vec![TaskOutcome::Cancelled, TaskOutcome::Cancelled]
        );
    }

    #[test]
    fn invalid_bytecode_result_cancels_active_sibling_and_queued_task() {
        let mut host = CoreVmTaskHost::new(2, 3);
        let invalid = host.submit_non_integer();
        host.submit_add(10, 1);
        host.submit_add(20, 2);

        let stats = host.tick();
        assert_eq!(stats.admitted, 2);
        assert_eq!(stats.polled, 2);
        assert_eq!(stats.completed, 2);
        // Only the first task reached its CoreVM execution point. The error
        // cancelled its admitted sibling before the sibling entered a VM.
        assert_eq!(host.executed(), &[invalid]);
        assert_eq!(
            host.into_outcomes(),
            vec![
                TaskOutcome::Completed(Err(
                    "expected an integer CoreVM result".to_string()
                )),
                TaskOutcome::Cancelled,
                TaskOutcome::Cancelled,
            ]
        );
    }

    #[test]
    fn parent_cancellation_prevents_queued_and_deferred_bytecode_from_running() {
        let mut host = CoreVmTaskHost::new(1, 2);
        let first = host.submit_add(100, 1);
        host.submit_add(200, 2);
        host.defer_once(first);

        assert_eq!(host.tick().polled, 1);
        assert!(host.executed().is_empty());

        host.cancel(CancellationCause::Parent);
        assert_eq!(host.tick().completed, 1);
        assert!(host.executed().is_empty());
        assert_eq!(
            host.into_outcomes(),
            vec![TaskOutcome::Cancelled, TaskOutcome::Cancelled]
        );
    }
}
