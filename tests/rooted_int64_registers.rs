//! Conformance tests for lifetime-scoped, ORCA-rooted VM-shaped Int64 registers.
//!
//! These tests intentionally use numeric semantics only; no wide integer may
//! escape as a bare legacy Value or enter NLCS v1 persistence.

use nulang::int64_semantics::Int64Error;
use nulang::runtime::{Actor, IntRegisterError, RootedIntRegisters};

#[test]
fn wide_and_inline_values_roundtrip_and_release_with_the_register_bank() {
    let mut actor = Actor::new(51, "int-register-owner", 0);
    {
        let mut regs = RootedIntRegisters::new(&mut actor, 4);
        let values = [i64::MIN, i64::MAX, -(1_i64 << 47), 1_i64 << 47];
        for (index, value) in values.into_iter().enumerate() {
            regs.store(index, value).unwrap();
            assert_eq!(regs.read(index).unwrap(), Some(value));
        }
    }
    assert_eq!(actor.heap.live_count(), 0, "roots must release on drop");
}

#[test]
fn copying_wide_register_retains_until_all_independent_roots_are_dropped() {
    let mut actor = Actor::new(52, "int-register-owner", 0);
    {
        let mut regs = RootedIntRegisters::new(&mut actor, 3);
        regs.store(0, i64::MAX).unwrap();
        regs.copy_into(1, 0).unwrap();
        regs.clear(0).unwrap();
        assert_eq!(regs.read(1).unwrap(), Some(i64::MAX));
        regs.copy_into(1, 1).unwrap(); // Self-copy must not release a root.
        regs.store(2, -5).unwrap();
        regs.copy_into(2, 1).unwrap(); // Replace inline with rooted wide value.
        regs.clear(1).unwrap();
        assert_eq!(regs.read(2).unwrap(), Some(i64::MAX));
        regs.clear(2).unwrap();
        assert_eq!(regs.read(2).unwrap(), None);
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn moving_a_wide_value_transfers_ownership_without_aliasing_or_leaking() {
    let mut actor = Actor::new(53, "int-register-owner", 0);
    {
        let mut regs = RootedIntRegisters::new(&mut actor, 2);
        regs.store(0, i64::MIN).unwrap();
        regs.store(1, i64::MAX).unwrap();
        regs.move_into(1, 0).unwrap();
        assert_eq!(regs.read(0).unwrap(), None);
        assert_eq!(regs.read(1).unwrap(), Some(i64::MIN));
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn checked_arithmetic_keeps_destination_unchanged_on_overflow() {
    let mut actor = Actor::new(54, "int-register-owner", 0);
    {
        let mut regs = RootedIntRegisters::new(&mut actor, 3);
        regs.store(0, i64::MAX).unwrap();
        regs.store(1, 1).unwrap();
        regs.store(2, 23).unwrap();
        assert_eq!(
            regs.checked_add_into(2, 0, 1),
            Err(IntRegisterError::Arithmetic(Int64Error::Overflow))
        );
        assert_eq!(regs.read(2).unwrap(), Some(23));
        regs.store(0, (1_i64 << 47) - 1).unwrap();
        regs.checked_add_into(2, 0, 1).unwrap();
        assert_eq!(regs.read(2).unwrap(), Some(1_i64 << 47));
    }
    assert_eq!(actor.heap.live_count(), 0);
}

#[test]
fn missing_registers_fail_closed_without_mutating_other_slots() {
    let mut actor = Actor::new(55, "int-register-owner", 0);
    {
        let mut regs = RootedIntRegisters::new(&mut actor, 1);
        regs.store(0, i64::MAX).unwrap();
        assert_eq!(regs.store(1, 3), Err(IntRegisterError::InvalidRegister));
        assert_eq!(regs.copy_into(0, 1), Err(IntRegisterError::InvalidRegister));
        assert_eq!(regs.move_into(1, 0), Err(IntRegisterError::InvalidRegister));
        assert_eq!(
            regs.checked_add_into(0, 0, 1),
            Err(IntRegisterError::InvalidRegister)
        );
        assert_eq!(regs.read(0).unwrap(), Some(i64::MAX));
    }
    assert_eq!(actor.heap.live_count(), 0);
}
