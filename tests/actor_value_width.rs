//! Regression tests for actor-reference identity preservation at the 48-bit value boundary.
//! These intentionally exercise the public Value API rather than its bit layout.

use nulang::vm::Value;

#[test]
fn actor_reference_preserves_largest_inline_id() {
    let id = (1_u64 << 48) - 1;
    let value = Value::try_actor_ref(id).expect("largest inline actor id must fit");
    assert_eq!(value.as_actor_id(), Some(id));
}

#[test]
fn actor_reference_rejects_high_bits_instead_of_aliasing_another_actor() {
    let id = (1_u64 << 48) + 42;
    assert!(Value::try_actor_ref(id).is_none());
    // Infallible legacy construction must fail closed, not manufacture actor #42.
    assert!(std::panic::catch_unwind(|| Value::actor_ref(id)).is_err());
}

#[test]
fn actor_reference_never_confuses_distinct_supported_ids() {
    let first = Value::try_actor_ref(42).unwrap();
    let second = Value::try_actor_ref((1_u64 << 48) - 42).unwrap();
    assert_ne!(first, second);
}
