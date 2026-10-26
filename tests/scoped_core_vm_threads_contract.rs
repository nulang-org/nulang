//! Native-only opt-in, bounded CoreVM worker integration.
//! These tests do not change source language `par` lowering.
#![cfg(not(target_arch = "wasm32"))]

use nulang::bytecode::{CodeModule, Instruction, OpCode};
use nulang::runtime::CancellationToken;
use nulang::scoped_core_vm_threads::run_parallel_core_vm_scope;
use nulang::scoped_task_core_vm::ScopedCoreVmTask;
use nulang::scoped_tasks::{ScopeError, TaskOutcome};
use nulang::scoped_thread_host::ScopedThreadError;
use nulang::value_layout::as_int_raw;

fn one() -> CodeModule {
    let mut module = CodeModule::new("one");
    module.emit(Instruction::new1(OpCode::Const1, 0));
    module.emit(Instruction::new0(OpCode::Halt));
    module
}

fn two() -> CodeModule {
    let mut module = CodeModule::new("two");
    module.emit(Instruction::new1(OpCode::Const2, 0));
    module.emit(Instruction::new0(OpCode::Halt));
    module
}

fn infinite() -> CodeModule {
    let mut module = CodeModule::new("bounded_loop");
    module.emit(Instruction::new1(OpCode::Const1, 0));
    module.emit(Instruction::new0(OpCode::Jmp));
    module
}

#[test]
fn isolated_core_vm_children_join_in_submission_order() {
    let modules = [one(), two(), one()];
    let tasks: Vec<_> = modules
        .iter()
        .map(|module| ScopedCoreVmTask {
            module,
            entry_pc: 0,
        })
        .collect();
    let outcomes = run_parallel_core_vm_scope(&tasks, 2, 3, 2, &CancellationToken::new()).unwrap();
    let values: Vec<_> = outcomes
        .into_iter()
        .map(|outcome| match outcome {
            TaskOutcome::Completed(Ok(raw)) => as_int_raw(raw),
            other => panic!("unexpected result: {other:?}"),
        })
        .collect();
    assert_eq!(values, [1, 2, 1]);
}

#[test]
fn exhausting_a_child_budget_cancels_unstarted_siblings() {
    let modules = [infinite(), one(), two()];
    let tasks: Vec<_> = modules
        .iter()
        .map(|module| ScopedCoreVmTask {
            module,
            entry_pc: 0,
        })
        .collect();
    let outcomes = run_parallel_core_vm_scope(&tasks, 1, 3, 10, &CancellationToken::new()).unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(
        matches!(&outcomes[0], TaskOutcome::Completed(Err(error)) if error.contains("instruction budget exhausted"))
    );
    assert_eq!(outcomes[1], TaskOutcome::Cancelled);
    assert_eq!(outcomes[2], TaskOutcome::Cancelled);
}

#[test]
fn pre_cancelled_parent_never_launches_a_vm() {
    let module = one();
    let parent = CancellationToken::new();
    parent.cancel();
    let tasks = [ScopedCoreVmTask {
        module: &module,
        entry_pc: 0,
    }];
    assert!(matches!(
        run_parallel_core_vm_scope(&tasks, 1, 1, 2, &parent),
        Err(ScopedThreadError::Scope(ScopeError::Cancelled))
    ));
}

#[test]
fn capacity_errors_are_detected_before_vm_execution() {
    let modules = [one(), two()];
    let tasks: Vec<_> = modules
        .iter()
        .map(|module| ScopedCoreVmTask {
            module,
            entry_pc: 0,
        })
        .collect();
    let parent = CancellationToken::new();
    assert!(matches!(
        run_parallel_core_vm_scope(&tasks, 1, 1, 2, &parent),
        Err(ScopedThreadError::Scope(ScopeError::QueueFull))
    ));
    assert!(matches!(
        run_parallel_core_vm_scope(&tasks, 0, 2, 2, &parent),
        Err(ScopedThreadError::Scope(ScopeError::ZeroLimit))
    ));
}

#[test]
fn zero_instruction_fuel_is_terminal_and_cancels_siblings() {
    let modules = [one(), two()];
    let tasks: Vec<_> = modules
        .iter()
        .map(|module| ScopedCoreVmTask {
            module,
            entry_pc: 0,
        })
        .collect();
    let outcomes = run_parallel_core_vm_scope(&tasks, 1, 2, 0, &CancellationToken::new()).unwrap();
    assert!(
        matches!(&outcomes[0], TaskOutcome::Completed(Err(error)) if error.contains("instruction budget exhausted"))
    );
    assert_eq!(outcomes[1], TaskOutcome::Cancelled);
}

#[test]
fn empty_scope_returns_without_worker_launch() {
    let tasks: [ScopedCoreVmTask<'_>; 0] = [];
    let result = run_parallel_core_vm_scope(&tasks, 2, 2, 10, &CancellationToken::new()).unwrap();
    assert!(result.is_empty());
}
