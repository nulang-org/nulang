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
            Err(IntFrameStackError::Register(
                IntRegisterError::InvalidRegister
            ))
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
        stack
            .checked_add_into(callee, 1, caller, 0, callee, 0)
            .unwrap();
        assert_eq!(stack.read(callee, 1).unwrap(), Some(1_i64 << 47));
        stack.pop().unwrap();
        assert_eq!(stack.read(caller, 0).unwrap(), Some((1_i64 << 47) - 1));
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn suspend_into_semantic_checkpoint_releases_original_roots_and_restores_new_actor() {
    let mut original_actor = Actor::new(76, "original-owner", 0);
    let checkpoint = {
        let mut stack = RootedIntFrameStack::new(&mut original_actor);
        let caller = stack.push_empty(4);
        stack.store(caller, 0, i64::MAX).unwrap();
        let callee = stack.push_empty(9);
        stack.store(callee, 0, i64::MIN).unwrap();
        stack.store(callee, 17, 7).unwrap();
        stack.suspend().unwrap()
    };
    assert_eq!(original_actor.heap.live_count(), 0);

    let mut resumed_actor = Actor::new(77, "resumed-owner", 0);
    {
        let mut resumed = RootedIntFrameStack::resume(&mut resumed_actor, &checkpoint).unwrap();
        assert_eq!(resumed.frame_count(), 2);
        assert_eq!(resumed.read(0, 0).unwrap(), Some(i64::MAX));
        assert_eq!(resumed.read(1, 0).unwrap(), Some(i64::MIN));
        assert_eq!(resumed.read(1, 17).unwrap(), Some(7));
        resumed.pop().unwrap();
        assert_eq!(resumed.read(0, 0).unwrap(), Some(i64::MAX));
    }
    assert_eq!(resumed_actor.heap.live_count(), 0);
}

#[test]
fn resuming_same_checkpoint_creates_independently_owned_roots() {
    let mut source_actor = Actor::new(78, "source-owner", 0);
    let checkpoint = {
        let mut stack = RootedIntFrameStack::new(&mut source_actor);
        stack.push_empty(0);
        stack.store(0, 0, i64::MAX).unwrap();
        stack.suspend().unwrap()
    };
    assert_eq!(source_actor.heap.live_count(), 0);

    let mut actor_a = Actor::new(79, "first-resume-owner", 0);
    let mut actor_b = Actor::new(80, "second-resume-owner", 0);
    {
        let mut first = RootedIntFrameStack::resume(&mut actor_a, &checkpoint).unwrap();
        let second = RootedIntFrameStack::resume(&mut actor_b, &checkpoint).unwrap();
        first.clear(0, 0).unwrap();
        assert_eq!(first.read(0, 0).unwrap(), None);
        assert_eq!(second.read(0, 0).unwrap(), Some(i64::MAX));
    }
    assert_eq!(actor_a.heap.live_count(), 0);
    assert_eq!(actor_b.heap.live_count(), 0);
}

#[test]
fn an_empty_stack_can_suspend_and_resume_without_leaving_roots() {
    let mut original_actor = Actor::new(81, "empty-owner", 0);
    let checkpoint = RootedIntFrameStack::new(&mut original_actor)
        .suspend()
        .unwrap();
    assert_eq!(original_actor.heap.live_count(), 0);
    let mut restored_actor = Actor::new(82, "empty-restored-owner", 0);
    {
        let restored = RootedIntFrameStack::resume(&mut restored_actor, &checkpoint).unwrap();
        assert_eq!(restored.frame_count(), 0);
    }
    assert_eq!(restored_actor.heap.live_count(), 0);
}
