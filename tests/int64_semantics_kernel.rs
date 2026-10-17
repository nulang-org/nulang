//! Conformance prerequisites for the proposed full-width Int64 semantics.
//! This kernel is a staging API; the current bytecode VM still has 48-bit Int
//! behavior until all backends and wire/persistence formats migrate together.

use nulang::int64_semantics::{self as int64, Int64Error, Int64Storage};
use nulang::vm::Value;

#[test]
fn wide_int64_is_preserved_without_reinterpreting_48_bit_tags() {
    for value in [i64::MIN, -(1_i64 << 47) - 1, 1_i64 << 47, i64::MAX] {
        let staged = Int64Storage::from_i64(value);
        assert_eq!(staged.to_i64(), value);
        assert!(staged.inline_bits().is_none());
        assert!(Value::try_int(value).is_none());
    }
}

#[test]
fn immediate_integer_storage_remains_one_word() {
    for value in [-(1_i64 << 47), -1, 0, 1, (1_i64 << 47) - 1] {
        let staged = Int64Storage::from_i64(value);
        let raw = staged.inline_bits().expect("fits in immediate range");
        assert_eq!(nulang::value_layout::as_int_raw(raw), value);
        assert_eq!(Value::try_int(value).unwrap().as_int(), Some(value));
    }
}

#[test]
fn int64_arithmetic_uses_the_full_signed_i64_range() {
    assert_eq!(int64::add((1_i64 << 47) - 1, 1), Ok(1_i64 << 47));
    assert_eq!(int64::sub(-(1_i64 << 47), 1), Ok(-(1_i64 << 47) - 1));
    assert_eq!(int64::mul(1_i64 << 40, 1_i64 << 20), Ok(1_i64 << 60));
    assert_eq!(int64::neg(-(1_i64 << 47)), Ok(1_i64 << 47));
    assert_eq!(int64::pow(2, 62), Ok(1_i64 << 62));
}

#[test]
fn int64_overflow_and_division_errors_do_not_wrap_or_return_nil() {
    assert_eq!(int64::add(i64::MAX, 1), Err(Int64Error::Overflow));
    assert_eq!(int64::sub(i64::MIN, 1), Err(Int64Error::Overflow));
    assert_eq!(int64::mul(i64::MAX, 2), Err(Int64Error::Overflow));
    assert_eq!(int64::neg(i64::MIN), Err(Int64Error::Overflow));
    assert_eq!(int64::div(7, 0), Err(Int64Error::DivisionByZero));
    assert_eq!(int64::rem(7, 0), Err(Int64Error::DivisionByZero));
    assert_eq!(int64::div(i64::MIN, -1), Err(Int64Error::Overflow));
    assert_eq!(int64::rem(i64::MIN, -1), Err(Int64Error::Overflow));
    assert_eq!(int64::pow(2, 63), Err(Int64Error::Overflow));
    assert_eq!(int64::pow(2, -1), Err(Int64Error::NegativeExponent));
}

#[test]
fn power_boundary_cases_are_deterministic_even_for_huge_exponents() {
    assert_eq!(int64::pow(0, 0), Ok(1));
    assert_eq!(int64::pow(0, i64::MAX), Ok(0));
    assert_eq!(int64::pow(1, i64::MAX), Ok(1));
    assert_eq!(int64::pow(-1, i64::MAX), Ok(-1));
    assert_eq!(int64::pow(-1, i64::MAX - 1), Ok(1));
}

#[test]
fn checked_arithmetic_agrees_with_i128_oracle_at_sign_and_width_boundaries() {
    let values = [
        i64::MIN,
        i64::MIN + 1,
        -(1_i64 << 47) - 1,
        -(1_i64 << 47),
        -1,
        0,
        1,
        (1_i64 << 47) - 1,
        1_i64 << 47,
        i64::MAX - 1,
        i64::MAX,
    ];
    for a in values {
        for b in values {
            let oracle =
                |candidate: i128| i64::try_from(candidate).map_err(|_| Int64Error::Overflow);
            assert_eq!(int64::add(a, b), oracle(i128::from(a) + i128::from(b)));
            assert_eq!(int64::sub(a, b), oracle(i128::from(a) - i128::from(b)));
            assert_eq!(int64::mul(a, b), oracle(i128::from(a) * i128::from(b)));
            if b == 0 {
                assert_eq!(int64::div(a, b), Err(Int64Error::DivisionByZero));
                assert_eq!(int64::rem(a, b), Err(Int64Error::DivisionByZero));
            } else if a == i64::MIN && b == -1 {
                // Signed MIN / -1 is an overflow for *both* div and rem.
                // Rust's checked_rem intentionally returns None here even
                // though the mathematical remainder would be zero.
                assert_eq!(int64::div(a, b), Err(Int64Error::Overflow));
                assert_eq!(int64::rem(a, b), Err(Int64Error::Overflow));
            } else {
                assert_eq!(int64::div(a, b), oracle(i128::from(a) / i128::from(b)));
                assert_eq!(int64::rem(a, b), oracle(i128::from(a) % i128::from(b)));
            }
        }
    }
}
