//! Frame push/copy/unwind semantics for actor-scoped ORCA-rooted Int64 values.
//!
//! The production VM still runs the legacy Frame ABI; this actor-borrowed
//! stack proves the reference-count rules before changing continuation clones.

use nulang::int64_semantics::Int64Error;
use nulang::runtime::{Actor, IntFrameStackError, IntRegisterError, RootedIntFrameStack};

#[test]
fn cloning_a_wide_frame_retains_roots_through_original_frame_unwind() {
    let mut actor = Actor::new(71, "stack-owner", 0);
    {
        let mut stack = RootedIntFrameStack::new(&mut actor);
        let first = stack.push_empty(1);
        stack.store(first, 0, i64::MIN).unwrap();
        stack.store(first, 1, -8).unwrap();
        let cloned = stack.clone_top().unwrap();
        assert_eq!(stack.read(cloned, 0).unwrap(), Some(i64::MIN));
        stack.clear(first, 0).unwrap();
        assert_eq!(stack.read(cloned, 0).unwrap(), Some(i64::MIN));
        stack.pop().unwrap();
        assert_eq!(stack.read(first, 0).unwrap(), None);
        stack.pop().unwrap();
        assert_eq!(stack.frame_count(), 0);
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn cross_frame_move_transfers_one_root_without_dangling_or_double_free() {
    let mut actor = Actor::new(72, "stack-owner", 0);
    {
        let mut stack = RootedIntFrameStack::new(&mut actor);
        let caller = stack.push_empty(1);
        stack.store(caller, 4, i64::MAX).unwrap();
        let callee = stack.push_empty(2);
        stack.move_into(callee, 3, caller, 4).unwrap();
        assert_eq!(stack.read(caller, 4).unwrap(), None);
        assert_eq!(stack.read(callee, 3).unwrap(), Some(i64::MAX));
        stack.pop().unwrap();
        assert_eq!(stack.read(caller, 4).unwrap(), None);
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn cross_frame_copy_keeps_caller_root_alive_after_callee_return() {
    let mut actor = Actor::new(73, "stack-owner", 0);
    {
        let mut stack = RootedIntFrameStack::new(&mut actor);
        let caller = stack.push_empty(0);
        stack.store(caller, 1, i64::MAX).unwrap();
        let callee = stack.push_empty(0);
        stack.copy_into(callee, 2, caller, 1).unwrap();
        stack.clear(caller, 1).unwrap();
        assert_eq!(stack.read(callee, 2).unwrap(), Some(i64::MAX));
        stack.copy_into(callee, 2, callee, 2).unwrap();
        stack.pop().unwrap();
        assert_eq!(stack.read(caller, 1).unwrap(), None);
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn invalid_frame_and_slot_accesses_are_nonmutating() {
    let mut actor = Actor::new(74, "stack-owner", 0);
    {
        let mut stack = RootedIntFrameStack::new(&mut actor);
        assert_eq!(stack.pop(), Err(IntFrameStackError::EmptyStack));
        let root = stack.push_empty(9);
        stack.store(root, 0, i64::MIN).unwrap();
        assert_eq!(
            stack.store(root + 1, 0, 1),
            Err(IntFrameStackError::InvalidFrame)
        );
        assert_eq!(
            stack.read(root, 256),
            Err(IntFrameStackError::Register(IntRegisterError::InvalidRegister))
        );
        assert_eq!(
            stack.copy_into(root, 0, root + 1, 0),
            Err(IntFrameStackError::InvalidFrame)
        );
        assert_eq!(stack.read(root, 0).unwrap(), Some(i64::MIN));
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn signed_64_bit_arithmetic_updates_only_the_target_frame() {
    let mut actor = Actor::new(75, "stack-owner", 0);
    {
        let mut stack = RootedIntFrameStack::new(&mut actor);
        let caller = stack.push_empty(0);
        stack.store(caller, 0, i64::MAX).unwrap();
        let callee = stack.push_empty(0);
        stack.store(callee, 0, 1).unwrap();
        stack.store(callee, 1, -7).unwrap();
        assert_eq!(
            stack.checked_add_into(callee, 1, caller, 0, callee, 0),
            Err(IntFrameStackError::Register(IntRegisterError::Arithmetic(
                Int64Error::Overflow
            )))
        );
        assert_eq!(stack.read(callee, 1).unwrap(), Some(-7));
        stack.store(caller, 0, (1_i64 << 47) - 1).unwrap();
        stack.checked_add_into(callee, 1, caller, 0, callee, 0).unwrap();
        assert_eq!(stack.read(callee, 1).unwrap(), Some(1_i64 << 47));
        stack.pop().unwrap();
        assert_eq!(stack.read(caller, 0).unwrap(), Some((1_i64 << 47) - 1));
    }
    assert_eq!(actor.heap.live_count(), 0);
}
