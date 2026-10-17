//! Real VM Frame interchange with the scoped ORCA-rooted Int64 register bank.
//! This bridge is intentionally numeric-only. Legacy Frame is Clone and can
//! contain arbitrary pointers; wide heap pointers must never be exported.

use nulang::runtime::{Actor, FrameInt64BoundaryError, RootedIntFrame};
use nulang::vm::{Frame, Value, VM};

#[test]
fn numeric_legacy_frame_roundtrips_with_control_metadata_and_empty_slots() {
    let mut legacy = Frame::new(Some(7), 2);
    legacy.pc = 19;
    legacy.return_dst = 5;
    legacy.regs[0] = Value::int(-123);
    legacy.regs[8] = Value::int(0);

    let mut actor = Actor::new(61, "frame-owner", 0);
    {
        let rooted = RootedIntFrame::try_import(&mut actor, &legacy).unwrap();
        assert_eq!(rooted.read(0).unwrap(), Some(-123));
        assert_eq!(rooted.read(1).unwrap(), None);
        assert_eq!(rooted.read(8).unwrap(), Some(0));

        let restored = rooted.try_export().unwrap();
        assert_eq!(restored.pc, 19);
        assert_eq!(restored.module_idx, 2);
        assert_eq!(restored.return_dst, 5);
        assert_eq!(restored.caller_idx, Some(7));
        assert_eq!(restored.regs[0].as_int(), Some(-123));
        assert!(restored.regs[1].is_nil());
        assert_eq!(restored.regs[8].as_int(), Some(0));
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn wide_values_are_rooted_but_cannot_escape_into_cloneable_legacy_frame() {
    let mut actor = Actor::new(62, "frame-owner", 0);
    {
        let mut rooted = RootedIntFrame::new(&mut actor, 0);
        rooted.store(0, i64::MAX).unwrap();
        assert_eq!(rooted.read(0).unwrap(), Some(i64::MAX));
        assert_eq!(
            rooted.try_export().unwrap_err(),
            FrameInt64BoundaryError::WideIntegerRequiresNewAbi { register: 0 }
        );
        assert_eq!(rooted.read(0).unwrap(), Some(i64::MAX));
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn importing_unsupported_legacy_values_fails_without_dereferencing_pointers() {
    let mut actor = Actor::new(63, "frame-owner", 0);
    let mut frame = Frame::new(None, 0);
    frame.regs[17] = Value::bool(true);
    assert_eq!(
        RootedIntFrame::try_import(&mut actor, &frame).err(),
        Some(FrameInt64BoundaryError::UnsupportedValue { register: 17 })
    );
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn importing_frames_with_captured_closures_or_spills_is_fail_closed() {
    let mut actor = Actor::new(64, "frame-owner", 0);
    let mut frame = Frame::new(None, 0);
    frame.spilled.push(Value::int(3));
    assert_eq!(
        RootedIntFrame::try_import(&mut actor, &frame).err(),
        Some(FrameInt64BoundaryError::UnsupportedSpillSlots)
    );
    frame.spilled.clear();
    frame.closure_env = Some(Value::nil());
    assert_eq!(
        RootedIntFrame::try_import(&mut actor, &frame).err(),
        Some(FrameInt64BoundaryError::UnsupportedClosureEnvironment)
    );
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn a_semantic_snapshot_restores_wide_values_without_pointer_aliases() {
    let mut actor = Actor::new(65, "frame-owner", 0);
    let snapshot = {
        let mut frame = RootedIntFrame::new(&mut actor, 3);
        frame.store(0, i64::MIN).unwrap();
        frame.store(1, (1_i64 << 47) - 1).unwrap();
        frame.snapshot().unwrap()
    };
    assert_eq!(actor.heap.live_count(), 0);
    {
        let restored = RootedIntFrame::try_restore(&mut actor, &snapshot).unwrap();
        assert_eq!(restored.read(0).unwrap(), Some(i64::MIN));
        assert_eq!(restored.read(1).unwrap(), Some((1_i64 << 47) - 1));
        assert_eq!(
            restored.try_export().unwrap_err(),
            FrameInt64BoundaryError::WideIntegerRequiresNewAbi { register: 0 }
        );
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn arithmetic_overflow_leaves_destination_unchanged_across_frame_boundary() {
    let mut actor = Actor::new(66, "frame-owner", 0);
    {
        let mut frame = RootedIntFrame::new(&mut actor, 0);
        frame.store(0, i64::MAX).unwrap();
        frame.store(1, 1).unwrap();
        frame.store(2, -9).unwrap();
        assert!(frame.checked_add_into(2, 0, 1).is_err());
        assert_eq!(frame.read(2).unwrap(), Some(-9));
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn production_vm_installs_only_verified_inline_frames() {
    let mut actor = Actor::new(67, "frame-owner", 0);
    let mut vm = VM::new_without_jit();
    {
        let mut rooted = RootedIntFrame::new(&mut actor, 3);
        rooted.store(0, -77).unwrap();
        vm.install_numeric_rooted_frame(&rooted).unwrap();
    }
    assert_eq!(vm.frames().len(), 1);
    assert_eq!(vm.frames()[0].module_idx, 3);
    assert_eq!(vm.frames()[0].regs[0].as_int(), Some(-77));
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn production_vm_does_not_discard_active_frame_when_wide_install_is_denied() {
    let mut actor = Actor::new(68, "frame-owner", 0);
    let mut vm = VM::new_without_jit();
    let mut previous = Frame::new(None, 1);
    previous.pc = 123;
    previous.regs[0] = Value::int(19);
    vm.set_current_frame(previous);
    {
        let mut rooted = RootedIntFrame::new(&mut actor, 2);
        rooted.store(0, i64::MAX).unwrap();
        assert_eq!(
            vm.install_numeric_rooted_frame(&rooted),
            Err(FrameInt64BoundaryError::WideIntegerRequiresNewAbi { register: 0 })
        );
        assert_eq!(rooted.read(0).unwrap(), Some(i64::MAX));
    }
    assert_eq!(vm.frames().len(), 1);
    assert_eq!(vm.frames()[0].module_idx, 1);
    assert_eq!(vm.frames()[0].pc, 123);
    assert_eq!(vm.frames()[0].regs[0].as_int(), Some(19));
    assert_eq!(actor.heap.live_count(), 0);
}
