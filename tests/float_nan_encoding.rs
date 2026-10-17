//! Public runtime-value encoding invariants.
//! IEEE-754 NaN bits must never be mistaken for immediate Nulang tags.

use nulang::vm::Value;

#[test]
fn float_nan_remains_a_float_instead_of_aliasing_nil() {
    let value = Value::float(f64::NAN);
    assert!(value.is_float(), "NaN must retain its Float type");
    assert!(
        !value.is_nil(),
        "hardware canonical NaN must not alias TAG_NIL"
    );
    assert!(value.as_float().expect("float payload").is_nan());
}

#[test]
fn float_nan_payloads_that_match_type_tags_are_canonicalized() {
    let fake_int_nan = f64::from_bits(0x7FFB_0000_0000_002A);
    assert!(fake_int_nan.is_nan());
    let value = Value::float(fake_int_nan);
    assert!(value.is_float());
    assert!(!value.is_int());
    assert!(value.as_float().expect("float payload").is_nan());
}
