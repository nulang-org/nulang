//! Proposed full-width Int64 arithmetic kernel.
//!
//! The current v1 bytecode VM has 48-bit tagged integer semantics. This
//! module specifies and implements the *target* signed 64-bit operations
//! without silently truncating intermediate results to the tagged payload.
//! A value can remain immediate when small or be represented as a wider
//! owned scalar until the heap and backend migration is complete.
//!
//! IMPORTANT: This is not yet wired into source-language `Int`, VM opcodes,
//! JIT, AOT, WASM, persistence, or FFI. Those must migrate together.

use crate::value_layout;

/// Distinguishes arithmetic failures from a successful numeric result.
/// Arithmetic overflow is checked, not wrapping, irrespective of backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Int64Error {
    Overflow,
    DivisionByZero,
    NegativeExponent,
}

impl std::fmt::Display for Int64Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overflow => write!(f, "signed 64-bit integer overflow"),
            Self::DivisionByZero => write!(f, "integer division by zero"),
            Self::NegativeExponent => write!(f, "negative integer exponent"),
        }
    }
}

impl std::error::Error for Int64Error {}

/// Staging representation that never loses any bit of a signed 64-bit Int.
/// This is *not* the eventual heap layout or cross-backend Value ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Int64Storage(Int64Repr);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Int64Repr {
    Inline(u64),
    Wide(i64),
}

impl Int64Storage {
    pub fn from_i64(value: i64) -> Self {
        match value_layout::try_tag_int(value) {
            Some(raw) => Self(Int64Repr::Inline(raw)),
            None => Self(Int64Repr::Wide(value)),
        }
    }

    pub fn to_i64(self) -> i64 {
        match self.0 {
            Int64Repr::Inline(raw) => value_layout::as_int_raw(raw),
            Int64Repr::Wide(value) => value,
        }
    }

    /// The single-word representation if this integer fits the inline range.
    /// Never mask high bits to coerce a wide value into an inline one.
    pub fn inline_bits(self) -> Option<u64> {
        match self.0 {
            Int64Repr::Inline(raw) => Some(raw),
            Int64Repr::Wide(_) => None,
        }
    }
}

#[inline]
pub fn add(a: i64, b: i64) -> Result<i64, Int64Error> {
    a.checked_add(b).ok_or(Int64Error::Overflow)
}

#[inline]
pub fn sub(a: i64, b: i64) -> Result<i64, Int64Error> {
    a.checked_sub(b).ok_or(Int64Error::Overflow)
}

#[inline]
pub fn mul(a: i64, b: i64) -> Result<i64, Int64Error> {
    a.checked_mul(b).ok_or(Int64Error::Overflow)
}

#[inline]
pub fn neg(value: i64) -> Result<i64, Int64Error> {
    value.checked_neg().ok_or(Int64Error::Overflow)
}

#[inline]
pub fn div(a: i64, b: i64) -> Result<i64, Int64Error> {
    if b == 0 {
        return Err(Int64Error::DivisionByZero);
    }
    a.checked_div(b).ok_or(Int64Error::Overflow)
}

#[inline]
pub fn rem(a: i64, b: i64) -> Result<i64, Int64Error> {
    if b == 0 {
        return Err(Int64Error::DivisionByZero);
    }
    a.checked_rem(b).ok_or(Int64Error::Overflow)
}

/// Signed integer exponentiation with checked multiplication.
///
/// Uses exponentiation by squaring rather than `i64::pow(u32)`, so even very
/// large positive exponents remain bounded to 63 squaring iterations. The
/// result is 1 for 0**0; a negative exponent is a domain error. Overflow
/// is reported explicitly rather than wrapping to an unrelated value.
pub fn pow(mut base: i64, exponent: i64) -> Result<i64, Int64Error> {
    let mut exponent = u64::try_from(exponent).map_err(|_| Int64Error::NegativeExponent)?;
    let mut result = 1_i64;
    while exponent != 0 {
        if (exponent & 1) != 0 {
            result = mul(result, base)?;
        }
        exponent >>= 1;
        if exponent != 0 {
            base = mul(base, base)?;
        }
    }
    Ok(result)
}
