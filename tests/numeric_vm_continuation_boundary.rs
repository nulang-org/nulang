//! Production VM continuity contract for **inline-numeric-only** checkpoints.
//! This is NOT yet wide-Int64 continuation execution or a durable wire format.

use nulang::vm::{
    Frame, HandlerFrame, NumericContinuationError, SuspendedVmState, Value, VM,
};

#[test]
fn capture_and_restore_preserves_two_real_frames_without_raw_pointer_aliases() {
    let mut vm = VM::new_without_jit();
    let mut caller = Frame::new(None, 2);
    caller.pc = 7;
    caller.regs[0] = Value::int(-13);
    let mut callee = Frame::new(Some(0), 3);
    callee.pc = 41;
    callee.return_dst = 5;
    callee.regs[8] = Value::int((1_i64 << 47) - 1);
    vm.restore_suspended_state(SuspendedVmState {
        frames: vec![caller, callee],
        current_frame_idx: Some(1),
        handler_stack: Vec::new(),
        step_count: 123,
    });

    let checkpoint = vm.try_checkpoint_inline_numeric_continuation().unwrap();
    assert_eq!(vm.frames().len(), 2, "capture must not consume active frames");

    let mut restored = VM::new_without_jit();
    restored.restore_inline_numeric_continuation(&checkpoint).unwrap();
    assert_eq!(restored.current_frame_index(), Some(1));
    assert_eq!(restored.current_pc(), Some(41));
    assert_eq!(restored.frames()[0].pc, 7);
    assert_eq!(restored.frames()[0].module_idx, 2);
    assert_eq!(restored.frames()[0].regs[0].as_int(), Some(-13));
    assert_eq!(restored.frames()[1].caller_idx, Some(0));
    assert_eq!(restored.frames()[1].return_dst, 5);
    assert_eq!(restored.frames()[1].regs[8].as_int(), Some((1_i64 << 47) - 1));

    let mut second_restore = VM::new_without_jit();
    second_restore
        .restore_inline_numeric_continuation(&checkpoint)
        .unwrap();
    assert_eq!(second_restore.frames()[1].regs[8].as_int(), Some((1_i64 << 47) - 1));
}

#[test]
fn capture_refuses_foreign_or_pointer_bearing_registers_without_dereferencing_them() {
    let mut vm = VM::new_without_jit();
    let mut frame = Frame::new(None, 1);
    frame.regs[17] = Value::bool(true);
    vm.set_current_frame(frame);
    assert_eq!(
        vm.try_checkpoint_inline_numeric_continuation().unwrap_err(),
        NumericContinuationError::UnsupportedValue { frame: 0, register: 17 }
    );
    assert!(vm.frames()[0].regs[17].as_bool().unwrap());
}

#[test]
fn capture_refuses_handlers_and_does_not_change_active_state() {
    let mut vm = VM::new_without_jit();
    let mut frame = Frame::new(None, 0);
    frame.pc = 19;
    vm.restore_suspended_state(SuspendedVmState {
        frames: vec![frame],
        current_frame_idx: Some(0),
        handler_stack: vec![HandlerFrame::new(0, 0, 10, 3)],
        step_count: 2,
    });
    assert_eq!(
        vm.try_checkpoint_inline_numeric_continuation().unwrap_err(),
        NumericContinuationError::EffectHandlersUnsupported
    );
    assert_eq!(vm.frames()[0].pc, 19);
    let state = vm.take_suspended_state().unwrap();
    assert_eq!(state.handler_stack.len(), 1);
}

#[test]
fn capture_refuses_spills_closure_captures_and_invalid_caller_topology() {
    let mut vm = VM::new_without_jit();
    let mut frame = Frame::new(None, 0);
    frame.spilled.push(Value::int(7));
    vm.set_current_frame(frame);
    assert_eq!(
        vm.try_checkpoint_inline_numeric_continuation().unwrap_err(),
        NumericContinuationError::SpillsUnsupported { frame: 0 }
    );

    let mut frame = Frame::new(None, 0);
    frame.closure_env = Some(Value::nil());
    vm.set_current_frame(frame);
    assert_eq!(
        vm.try_checkpoint_inline_numeric_continuation().unwrap_err(),
        NumericContinuationError::ClosureEnvironmentUnsupported { frame: 0 }
    );

    let frame = Frame::new(Some(9), 0);
    vm.set_current_frame(frame);
    assert_eq!(
        vm.try_checkpoint_inline_numeric_continuation().unwrap_err(),
        NumericContinuationError::InvalidCaller { frame: 0 }
    );
}

#[test]
fn restore_into_nonempty_vm_fails_before_mutating_existing_frame() {
    let mut src = VM::new_without_jit();
    let mut frame = Frame::new(None, 0);
    frame.regs[0] = Value::int(88);
    src.set_current_frame(frame);
    let snapshot = src.try_checkpoint_inline_numeric_continuation().unwrap();

    let mut target = VM::new_without_jit();
    let mut previous = Frame::new(None, 9);
    previous.pc = 55;
    previous.regs[0] = Value::int(-7);
    target.set_current_frame(previous);
    assert_eq!(
        target.restore_inline_numeric_continuation(&snapshot),
        Err(NumericContinuationError::DestinationNotEmpty)
    );
    assert_eq!(target.frames()[0].pc, 55);
    assert_eq!(target.frames()[0].module_idx, 9);
    assert_eq!(target.frames()[0].regs[0].as_int(), Some(-7));
}

#[test]
fn empty_vm_has_no_numeric_continuation() {
    let vm = VM::new_without_jit();
    assert_eq!(
        vm.try_checkpoint_inline_numeric_continuation().unwrap_err(),
        NumericContinuationError::NoCurrentFrame
    );
}
