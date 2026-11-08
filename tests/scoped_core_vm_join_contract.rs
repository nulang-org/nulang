//! A lexical, bounded, one-shot CoreVM scope must join all of its children.
//! This is sequential reference behavior, not parallel source-level `par`.
use nulang::bytecode::{CodeModule, Instruction, OpCode};
use nulang::runtime::CancellationToken;
use nulang::scoped_task_core_vm::{run_bounded_core_vm_scope, ScopedCoreVmTask};
use nulang::scoped_tasks::{ScopeError, TaskOutcome};
use nulang::value_layout::as_int_raw;

fn one() -> CodeModule {
    let mut m = CodeModule::new("one");
    m.emit(Instruction::new1(OpCode::Const1, 0));
    m.emit(Instruction::new0(OpCode::Halt));
    m
}

fn two() -> CodeModule {
    let mut m = CodeModule::new("two");
    m.emit(Instruction::new1(OpCode::Const2, 0));
    m.emit(Instruction::new0(OpCode::Halt));
    m
}

fn infinite() -> CodeModule {
    let mut m = CodeModule::new("loop");
    m.emit(Instruction::new1(OpCode::Const1, 0));
    m.emit(Instruction::new0(OpCode::Jmp));
    m
}

#[test]
fn bounded_scope_joins_more_children_than_concurrency_cap_in_submission_order() {
    let m1 = one();
    let m2 = two();
    let tasks = [
        ScopedCoreVmTask {
            module: &m1,
            entry_pc: 0,
        },
        ScopedCoreVmTask {
            module: &m2,
            entry_pc: 0,
        },
        ScopedCoreVmTask {
            module: &m1,
            entry_pc: 0,
        },
    ];
    let result = run_bounded_core_vm_scope(&tasks, 1, 3, 2, &CancellationToken::new()).unwrap();
    let ints: Vec<_> = result
        .into_iter()
        .map(|o| match o {
            TaskOutcome::Completed(Ok(raw)) => as_int_raw(raw),
            other => panic!("unexpected result: {other:?}"),
        })
        .collect();
    assert_eq!(ints, [1, 2, 1]);
}

#[test]
fn first_child_failure_cancels_both_running_and_queued_siblings_before_join() {
    let looping = infinite();
    let finite = one();
    let tasks = [
        ScopedCoreVmTask {
            module: &looping,
            entry_pc: 0,
        },
        ScopedCoreVmTask {
            module: &finite,
            entry_pc: 0,
        },
        ScopedCoreVmTask {
            module: &finite,
            entry_pc: 0,
        },
    ];
    let outcomes = run_bounded_core_vm_scope(&tasks, 2, 3, 7, &CancellationToken::new()).unwrap();
    assert_eq!(outcomes.len(), 3);
    assert!(
        matches!(&outcomes[0], TaskOutcome::Completed(Err(e)) if e.contains("instruction budget exhausted"))
    );
    assert_eq!(outcomes[1], TaskOutcome::Cancelled);
    assert_eq!(outcomes[2], TaskOutcome::Cancelled);
}

#[test]
fn cancellation_before_scope_entry_rejects_admission_without_executing_children() {
    let parent = CancellationToken::new();
    parent.cancel();
    let finite = one();
    let tasks = [ScopedCoreVmTask {
        module: &finite,
        entry_pc: 0,
    }];
    assert_eq!(
        run_bounded_core_vm_scope(&tasks, 1, 1, 2, &parent),
        Err(ScopeError::Cancelled)
    );
}

#[test]
fn invalid_limits_and_excessive_pending_tasks_are_rejected_without_partial_execution() {
    let finite = one();
    let tasks = [
        ScopedCoreVmTask {
            module: &finite,
            entry_pc: 0,
        },
        ScopedCoreVmTask {
            module: &finite,
            entry_pc: 0,
        },
    ];
    let parent = CancellationToken::new();
    assert_eq!(
        run_bounded_core_vm_scope(&tasks, 0, 2, 2, &parent),
        Err(ScopeError::ZeroLimit)
    );
    assert_eq!(
        run_bounded_core_vm_scope(&tasks, 1, 0, 2, &parent),
        Err(ScopeError::ZeroLimit)
    );
    assert_eq!(
        run_bounded_core_vm_scope(&tasks, 1, 1, 2, &parent),
        Err(ScopeError::QueueFull)
    );
    assert!(!parent.is_cancelled());
}

#[test]
fn empty_scope_joins_without_execution() {
    let tasks: [ScopedCoreVmTask<'_>; 0] = [];
    let outcomes = run_bounded_core_vm_scope(&tasks, 1, 1, 1, &CancellationToken::new()).unwrap();
    assert!(outcomes.is_empty());
}
