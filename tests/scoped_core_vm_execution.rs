//! CoreVM's opt-in execution limits, and the safe one-shot scoped adapter.
//! These tests deliberately do not claim parallel execution or resumable tasks.
use nulang::bytecode::{CodeModule, Instruction, OpCode};
use nulang::core_vm::CoreVM;
use nulang::runtime::CancellationToken;
use nulang::scoped_task_core_vm::poll_core_vm_once;
use nulang::scoped_task_reference::{ReferenceExecutor, TaskPoll};
use nulang::scoped_tasks::{CancellationCause, ScopeError, TaskOutcome, TaskScope};

fn value_module() -> CodeModule {
    let mut module = CodeModule::new("finite_scoped_child");
    module.emit(Instruction::new1(OpCode::Const1, 0));
    module.emit(Instruction::new0(OpCode::Halt));
    module
}

fn looping_module() -> CodeModule {
    let mut module = CodeModule::new("unbounded_child");
    module.emit(Instruction::new1(OpCode::Const1, 0));
    module.emit(Instruction::new0(OpCode::Jmp));
    module
}

#[test]
fn a_zero_budget_executes_no_instruction_and_halt_consumes_fuel() {
    let module = value_module();
    for fuel in [0, 1] {
        let mut vm = CoreVM::new();
        let idx = vm.load_module_from_code(&module).unwrap();
        assert!(vm
            .run_with_limits(idx, 0, fuel, || false)
            .unwrap_err()
            .contains("instruction budget exhausted"));
    }

    let mut vm = CoreVM::new();
    let idx = vm.load_module_from_code(&module).unwrap();
    let value = vm.run_with_limits(idx, 0, 2, || false).unwrap();
    assert_eq!(nulang::value_layout::as_int_raw(value), 1);
}

#[test]
fn infinite_bytecode_is_bounded_and_cancellation_is_polled_between_instructions() {
    let module = looping_module();
    let mut vm = CoreVM::new();
    let idx = vm.load_module_from_code(&module).unwrap();
    assert!(vm
        .run_with_limits(idx, 0, 15, || false)
        .unwrap_err()
        .contains("instruction budget exhausted"));

    let mut vm = CoreVM::new();
    let idx = vm.load_module_from_code(&module).unwrap();
    let mut polls = 0;
    let err = vm
        .run_with_limits(idx, 0, 30, || {
            polls += 1;
            polls == 3
        })
        .unwrap_err();
    assert_eq!(polls, 3);
    assert!(err.contains("execution cancelled"));
}

#[test]
fn the_scoped_adapter_reports_real_termination_before_acknowledgment() {
    let parent = CancellationToken::new();
    let mut scope = TaskScope::<u64, String>::with_parent(1, 1, &parent).unwrap();
    let task = scope.try_submit().unwrap();
    assert_eq!(scope.admit_next(), Some(task));

    let mut vm = CoreVM::new();
    let idx = vm.load_module_from_code(&looping_module()).unwrap();
    let child = scope.cancellation_token();
    parent.cancel();
    let poll = poll_core_vm_once(&mut vm, idx, 0, 100, &child);
    assert!(matches!(poll, TaskPoll::Cancelled));
    assert_eq!(scope.try_join(), Err(ScopeError::NotQuiescent));
    scope.acknowledge_cancelled(task).unwrap();
    assert_eq!(scope.try_join().unwrap(), vec![TaskOutcome::Cancelled]);
}

#[test]
fn finite_one_shot_children_join_in_submission_order_without_outliving_scope() {
    let mut runner = ReferenceExecutor::<u64, String>::new(2, 2).unwrap();
    let first = runner.try_submit().unwrap();
    let second = runner.try_submit().unwrap();

    let stats = runner.tick(|id, token| {
        let mut vm = CoreVM::new();
        let module = if id == first {
            looping_module()
        } else {
            value_module()
        };
        let idx = vm.load_module_from_code(&module).unwrap();
        poll_core_vm_once(&mut vm, idx, 0, if id == first { 8 } else { 2 }, token)
    });
    assert_eq!(stats.completed, 2);
    assert_eq!(
        runner.cancellation(),
        Some(&CancellationCause::ChildFailed(first))
    );
    assert_eq!(runner.in_flight(), 0);
    let outcomes = runner.try_join().unwrap();
    assert!(
        matches!(&outcomes[0], TaskOutcome::Completed(Err(s)) if s.contains("instruction budget exhausted"))
    );
    assert_eq!(outcomes[1], TaskOutcome::Cancelled);
    let _ = second; // distinct submitted sibling, cancelled after first failed
}

#[test]
fn a_finite_scoped_child_returns_a_value_and_joins_successfully() {
    let mut runner = ReferenceExecutor::<u64, String>::new(1, 1).unwrap();
    let child = runner.try_submit().unwrap();
    let stats = runner.tick(|id, cancellation| {
        assert_eq!(id, child);
        let mut vm = CoreVM::new();
        let idx = vm.load_module_from_code(&value_module()).unwrap();
        poll_core_vm_once(&mut vm, idx, 0, 2, cancellation)
    });
    assert_eq!(stats.completed, 1);
    assert!(runner.is_quiescent());
    assert_eq!(
        runner.try_join().unwrap(),
        vec![TaskOutcome::Completed(Ok(nulang::value_layout::tag_int(1)))]
    );
}

#[test]
fn interrupted_core_vm_refuses_reuse_of_partial_frames() {
    let module = value_module();
    let mut out_of_fuel = CoreVM::new();
    let idx = out_of_fuel.load_module_from_code(&module).unwrap();
    assert!(out_of_fuel
        .run_with_limits(idx, 0, 1, || false)
        .unwrap_err()
        .contains("instruction budget exhausted"));
    assert!(out_of_fuel
        .run(idx, 0)
        .unwrap_err()
        .contains("cannot be reused after bounded interruption"));

    let mut cancelled = CoreVM::new();
    let idx = cancelled.load_module_from_code(&module).unwrap();
    assert!(cancelled
        .run_with_limits(idx, 0, 2, || true)
        .unwrap_err()
        .contains("execution cancelled"));
    assert!(cancelled
        .run_with_limits(idx, 0, 2, || false)
        .unwrap_err()
        .contains("cannot be reused after bounded interruption"));
}
