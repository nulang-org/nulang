//! Canonical i64-tagged value layout for Nulang.
//!
//! This module owns the single source of truth for the bit layout used by the
//! VM, JIT runtime helpers, typed compiler, WASM backend, and Python
//! marshalling layer. Keeping the constants in one place prevents tag
//! collisions and silent divergence between interpretation and compiled code.
//!
//! # Layout
//!
//! All values are represented as `u64` with the upper 16 bits encoding the
//! type tag and the lower 48 bits carrying the payload:
//!
//! ```text
//! |---- tag (16 bits) ----|-------- payload (48 bits) --------|
//! ```
//!
//! This scheme replaces the original NaN-boxing approach. The bit patterns
//! are identical to the old NaN-boxed layout (the tags occupy the IEEE 754
//! quiet-NaN range), but we now treat values as `i64`/`u64` integers rather
//! than `f64` bit patterns. This makes the representation immune to NaN
//! canonicalization in WASM engines while preserving the 8-byte value
//! footprint and the single-register property.
//!
//! # Rationale
//!
//! WASM engines normalize (canonicalize) floating-point NaN bit patterns,
//! which would silently corrupt type tags stored in NaN payload bits.
//! By treating values as `i64` integers and using integer shift/mask
//! operations for tag extraction, the representation is fully deterministic
//! across all targets: native, WASM, and any future backend.
//!
//! Floats are stored as their raw IEEE-754 bit pattern. Any bit pattern whose
//! upper 16 bits do not match a known type tag is interpreted as a float
//! (the current tag set occupies the quiet-NaN range 0x7FF6–0x7FFE, so no
//! valid non-NaN float will collide).
// ---------------------------------------------------------------------------
// Masks
// ---------------------------------------------------------------------------

/// Mask for the upper 16 tag bits.
pub const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;

/// Mask for the lower 48 payload bits.
pub const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Bit 47 of the payload, used for sign-extending 48-bit signed integers.
pub const SIGN_BIT: u64 = 0x0000_8000_0000_0000;

/// Number of bits to shift right to extract the tag (upper 16 bits → low 16).
pub const TAG_SHIFT: u32 = 48;

// ---------------------------------------------------------------------------
// Type tags (upper 16 bits of the i64 value)
// ---------------------------------------------------------------------------

/// Tag for `nil`.
pub const TAG_NIL: u64 = 0x7FF8_0000_0000_0000;
/// Tag for `unit`.
pub const TAG_UNIT: u64 = 0x7FF9_0000_0000_0000;
/// Tag for booleans. Payload bit 0: false=0, true=1.
pub const TAG_BOOL: u64 = 0x7FFA_0000_0000_0000;
/// Tag for integers. Payload is a 48-bit signed value.
pub const TAG_INT: u64 = 0x7FFB_0000_0000_0000;
/// Tag for heap pointers. Payload is a heap offset.
pub const TAG_PTR: u64 = 0x7FFC_0000_0000_0000;
/// Tag for actor references.
pub const TAG_ACTOR: u64 = 0x7FFD_0000_0000_0000;
/// Tag for interned string IDs.
pub const TAG_STRING: u64 = 0x7FFE_0000_0000_0000;
/// Tag for closure references.
pub const TAG_CLOSURE: u64 = 0x7FF7_0000_0000_0000;
/// Tag for shared object-store references (immutable `val` buffers).
pub const TAG_OBJECT: u64 = 0x7FF5_0000_0000_0000;

// ---------------------------------------------------------------------------
// Integer range
// ---------------------------------------------------------------------------

/// Largest integer representable in the 48-bit signed payload (2^47 - 1).
pub const INT48_MAX: i64 = 0x0000_7FFF_FFFF_FFFF;
/// Smallest integer representable in the 48-bit signed payload (-2^47).
pub const INT48_MIN: i64 = -0x0000_8000_0000_0000;

/// True when `n` fits in the 48-bit signed integer payload.
#[inline]
pub fn int48_in_range(n: i64) -> bool {
    (INT48_MIN..=INT48_MAX).contains(&n)
}

// ---------------------------------------------------------------------------
// Canonical NaN
// ---------------------------------------------------------------------------

/// Reserved NaN bit pattern used to store a float NaN without colliding with
/// any type tag.
///
/// Hardware operations that produce NaN (`inf - inf`, `0.0 * inf`, `sqrt(-1)`,
/// ...) yield the architecture's default quiet NaN `0x7FF8_0000_0000_0000`,
/// whose upper 16 bits are exactly `TAG_NIL`; other NaN payloads alias
/// `TAG_BOOL`/`TAG_INT`/`TAG_PTR`/... Storing such a pattern verbatim would
/// silently reinterpret the float as nil, an integer, or (memory-unsafely) a
/// heap pointer. Following the LuaJIT approach, every float-producing path
/// canonicalizes NaN results to this fixed pattern instead.
///
/// Layout requirements (verified by tests below):
/// - upper 16 bits (`0xFFF8`) must not equal any tag's upper 16 bits
///   (tags occupy `0x7FF6`–`0x7FFE`), and
/// - the mantissa must be non-zero so the pattern is a genuine NaN.
pub const CANONICAL_NAN_BITS: u64 = 0xFFF8_0000_0000_0001;

/// Return the raw bits to store for float `f`, canonicalizing any NaN result
/// to `CANONICAL_NAN_BITS` so it can never alias a type tag.
#[inline]
pub fn float_bits(f: f64) -> u64 {
    let bits = f.to_bits();
    if is_float_raw(bits) {
        bits
    } else {
        CANONICAL_NAN_BITS
    }
}

// ---------------------------------------------------------------------------
// Tag extraction helpers (i64-based — no f64 bit-casting)
// ---------------------------------------------------------------------------

/// Extract the upper 16 tag bits from a raw value.
#[inline]
pub fn tag_of(raw: u64) -> u64 {
    raw >> TAG_SHIFT
}

/// True when `raw` carries an integer tag.
#[inline]
pub fn is_int_raw(raw: u64) -> bool {
    (raw & TAG_MASK) == TAG_INT
}

/// True when `raw` carries a heap-pointer tag.
#[inline]
pub fn is_ptr_raw(raw: u64) -> bool {
    (raw & TAG_MASK) == TAG_PTR
}

/// Sign-extend a 48-bit signed payload to a full `i64`.
#[inline]
pub fn sext48(bits: u64) -> i64 {
    if bits & SIGN_BIT != 0 {
        (bits | 0xFFFF_0000_0000_0000) as i64
    } else {
        bits as i64
    }
}

/// Extract the integer payload from a tagged value (assumes `is_int_raw`).
#[inline]
pub fn as_int_raw(raw: u64) -> i64 {
    sext48(raw & PAYLOAD_MASK)
}

/// Extract the heap-pointer payload from a tagged value (assumes `is_ptr_raw`).
#[inline]
pub fn as_ptr_raw(raw: u64) -> u32 {
    (raw & 0xFFFF_FFFF) as u32
}

/// Pack a 48-bit signed integer payload into a tagged value.
#[inline]
pub fn tag_int(payload: i64) -> u64 {
    TAG_INT | ((payload as u64) & PAYLOAD_MASK)
}

/// Pack a boolean into a tagged value.
#[inline]
pub fn tag_bool(b: bool) -> u64 {
    TAG_BOOL | (b as u64)
}

/// Pack a heap offset into a tagged pointer value.
#[inline]
pub fn tag_ptr(offset: u32) -> u64 {
    TAG_PTR | (offset as u64)
}

/// Pack an object-store id into a tagged object reference value.
#[inline]
pub fn tag_object(id: u64) -> u64 {
    TAG_OBJECT | (id & PAYLOAD_MASK)
}

/// True when a **full pointer address** fits in the 48-bit payload without
/// truncation.
///
/// The modern `TAG_PTR` encoding stores a 32-bit heap *offset* (see
/// [`tag_ptr`]/[`as_ptr_raw`]), which is portable everywhere. However, the
/// legacy `Value::ptr`/`Value::as_ptr` path in `vm.rs` stores the raw
/// pointer address masked with [`PAYLOAD_MASK`]. On platforms with virtual
/// address spaces wider than 48 bits (x86-64 LA57 5-level paging, AArch64
/// with 52-bit VA, or any allocator returning high addresses), that masking
/// silently truncates the address and produces a dangling pointer.
///
/// Pointer-producing code on the legacy path should check this predicate
/// (or `debug_assert!` it) before packing an address into a value.
#[inline]
pub fn ptr_fits_payload(addr: u64) -> bool {
    addr & !PAYLOAD_MASK == 0
}

/// Pack a raw u64 bit pattern into a tagged closure value.
#[inline]
pub fn tag_closure(payload: u64) -> u64 {
    TAG_CLOSURE | (payload & PAYLOAD_MASK)
}

// ---------------------------------------------------------------------------
// Float detection
// ---------------------------------------------------------------------------

/// Mask for the IEEE 754 NaN exponent (bits 52–62). Any NaN or infinity
/// has these bits set to 0x7FF; non-NaN/non-infinity floats do not.
const EXPONENT_MASK: u64 = 0x7FF0_0000_0000_0000;
/// Mantissa bits (0–51). A NaN has exponent = 0x7FF and non-zero mantissa;
/// infinity has exponent = 0x7FF and zero mantissa.
const MANTISSA_MASK: u64 = 0x000F_FFFF_FFFF_FFFF;

/// True when `raw` represents a real IEEE-754 float. Infinity is a valid
/// float, so it returns true. NaN patterns are NOT floats — except the single
/// reserved `CANONICAL_NAN_BITS` pattern, which `float_bits()` substitutes for
/// any NaN result so that float NaNs survive in the boxed representation.
///
/// All tagged values (0x7FF6–0x7FFE) occupy the quiet-NaN range, so this
/// integer bitmask test is equivalent to `!f64::from_bits(raw).is_nan()` (plus
/// the canonical-NaN exception) but avoids the FPU domain-crossing penalty of
/// `vmovq` + `ucomisd`.
#[inline]
pub fn is_float_raw(raw: u64) -> bool {
    // NaN: exponent all 1s AND mantissa non-zero.
    // Infinity: exponent all 1s AND mantissa zero → it IS a float.
    // The canonical NaN pattern is the sole NaN accepted as a float.
    (raw & EXPONENT_MASK) != EXPONENT_MASK
        || (raw & MANTISSA_MASK) == 0
        || raw == CANONICAL_NAN_BITS
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tags_are_unique() {
        let tags = [
            TAG_NIL,
            TAG_UNIT,
            TAG_BOOL,
            TAG_INT,
            TAG_PTR,
            TAG_ACTOR,
            TAG_STRING,
            TAG_CLOSURE,
            TAG_OBJECT,
        ];
        for i in 0..tags.len() {
            for j in (i + 1)..tags.len() {
                assert_ne!(tags[i], tags[j], "tags {} and {} collide", i, j);
            }
        }
    }

    #[test]
    fn test_tags_distinct_upper_bits() {
        // Each tag must have a unique value in the upper 16 bits so that
        // `tag_of()` can discriminate types. No tag may collide with another
        // or with the float range (0x0000–0x7FF5, 0x7FFF).
        let tags: &[(u64, &str)] = &[
            (TAG_NIL, "nil"),
            (TAG_UNIT, "unit"),
            (TAG_BOOL, "bool"),
            (TAG_INT, "int"),
            (TAG_PTR, "ptr"),
            (TAG_ACTOR, "actor"),
            (TAG_STRING, "string"),
            (TAG_CLOSURE, "closure"),
            (TAG_OBJECT, "object"),
        ];
        let mut seen = std::collections::HashSet::new();
        for &(tag, name) in tags {
            let upper = tag >> TAG_SHIFT;
            assert!(
                seen.insert(upper),
                "tag {} ({:#018x}) upper 16 bits {:#06x} collide with another tag",
                name,
                tag,
                upper
            );
        }
    }

    #[test]
    fn test_tag_of() {
        assert_eq!(tag_of(TAG_INT), TAG_INT >> TAG_SHIFT);
        assert_eq!(tag_of(TAG_NIL), TAG_NIL >> TAG_SHIFT);
        assert_eq!(tag_of(TAG_PTR | 0x1234), TAG_PTR >> TAG_SHIFT);
    }

    #[test]
    fn test_is_int_raw() {
        assert!(is_int_raw(tag_int(42)));
        assert!(is_int_raw(tag_int(-1)));
        assert!(!is_int_raw(TAG_NIL));
        assert!(!is_int_raw(TAG_PTR | 0x100));
    }

    #[test]
    fn test_is_ptr_raw() {
        assert!(is_ptr_raw(TAG_PTR | 0x1000));
        assert!(!is_ptr_raw(tag_int(0)));
        assert!(!is_ptr_raw(TAG_NIL));
    }

    #[test]
    fn test_as_int_raw() {
        assert_eq!(as_int_raw(tag_int(42)), 42);
        assert_eq!(as_int_raw(tag_int(-1)), -1);
        assert_eq!(as_int_raw(tag_int(0)), 0);
    }

    #[test]
    fn test_as_ptr_raw() {
        assert_eq!(as_ptr_raw(TAG_PTR | 0xDEAD_BEEF), 0xDEAD_BEEF);
        assert_eq!(as_ptr_raw(TAG_PTR), 0);
    }

    #[test]
    fn test_tag_ptr() {
        let raw = tag_ptr(0xABCD);
        assert!(is_ptr_raw(raw));
        assert_eq!(as_ptr_raw(raw), 0xABCD);
    }

    #[test]
    fn test_ptr_fits_payload() {
        assert!(ptr_fits_payload(0));
        assert!(ptr_fits_payload(0x0000_7FFF_FFFF_FFF8)); // typical x86-64 user VA
        assert!(ptr_fits_payload(PAYLOAD_MASK));
        // LA57 / AArch64-52 style addresses above 2^48 would be truncated
        // by the legacy `Value::ptr` masking path.
        assert!(!ptr_fits_payload(0x0001_0000_0000_0000));
        assert!(!ptr_fits_payload(0x00FF_FFFF_FFFF_FFFF));
    }

    #[test]
    fn test_tag_closure() {
        let raw = tag_closure(0x5555);
        assert_eq!(raw & TAG_MASK, TAG_CLOSURE);
        assert_eq!(raw & PAYLOAD_MASK, 0x5555);
    }

    #[test]
    fn test_tag_object() {
        let raw = tag_object(0x1234_5678_9ABC);
        assert_eq!(raw & TAG_MASK, TAG_OBJECT);
        assert_eq!(raw & PAYLOAD_MASK, 0x1234_5678_9ABC); // already fits in 48 bits
        let raw2 = tag_object(0xABCD_1234_5678_9ABC);
        assert_eq!(raw2 & PAYLOAD_MASK, 0x1234_5678_9ABC); // truncated to 48 bits
    }

    #[test]
    fn test_is_float_raw() {
        // Real floats (non-NaN) should be detected.
        assert!(is_float_raw(0u64)); // +0.0
        assert!(is_float_raw(0x3FF0_0000_0000_0000)); // 1.0
        assert!(is_float_raw(0x4000_0000_0000_0000)); // 2.0
        assert!(is_float_raw(0x7FF0_0000_0000_0000)); // +inf
                                                      // Tagged values should NOT be detected as floats.
        assert!(!is_float_raw(tag_int(1)));
        assert!(!is_float_raw(TAG_NIL));
        assert!(!is_float_raw(TAG_PTR | 0x1000));
        assert!(!is_float_raw(TAG_CLOSURE | 0x10));
        // NaN values (even with upper bits outside the known tag range) are NOT floats.
        assert!(!is_float_raw(0x7FF5_0000_0000_0000)); // NaN, not a tag, still NaN
        assert!(!is_float_raw(0x7FF8_0000_0000_0000)); // hardware quiet NaN == TAG_NIL
        assert!(!is_float_raw(0xFFF9_DEAD_BEEF_0000)); // arbitrary negative NaN
                                                       // The single reserved canonical NaN pattern IS a float.
        assert!(is_float_raw(CANONICAL_NAN_BITS));
    }

    #[test]
    fn test_canonical_nan_is_tag_free() {
        // The canonical NaN must not alias any tag: its upper 16 bits must be
        // distinct from every tag's upper 16 bits.
        let upper = CANONICAL_NAN_BITS >> TAG_SHIFT;
        for tag in [
            TAG_NIL,
            TAG_UNIT,
            TAG_BOOL,
            TAG_INT,
            TAG_PTR,
            TAG_ACTOR,
            TAG_STRING,
            TAG_CLOSURE,
            TAG_OBJECT,
        ] {
            assert_ne!(upper, tag >> TAG_SHIFT, "canonical NaN aliases a tag");
        }
        // It must be a genuine NaN (exponent all 1s, non-zero mantissa).
        assert_eq!(CANONICAL_NAN_BITS & EXPONENT_MASK, EXPONENT_MASK);
        assert_ne!(CANONICAL_NAN_BITS & MANTISSA_MASK, 0);
        assert!(f64::from_bits(CANONICAL_NAN_BITS).is_nan());
        // And it must round-trip through as a float.
        assert!(is_float_raw(CANONICAL_NAN_BITS));
    }

    #[test]
    fn test_float_bits_canonicalizes_nan() {
        // Non-NaN floats pass through unchanged.
        for f in [0.0f64, -0.0, 1.5, -2.25, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(float_bits(f), f.to_bits());
        }
        // Every NaN pattern — including the hardware default quiet NaN whose
        // upper bits are TAG_NIL — canonicalizes to CANONICAL_NAN_BITS.
        let hw_nan = f64::INFINITY - f64::INFINITY;
        assert!(hw_nan.is_nan());
        assert_eq!(float_bits(hw_nan), CANONICAL_NAN_BITS);
        assert_eq!(float_bits(0.0 * f64::INFINITY), CANONICAL_NAN_BITS);
        assert_eq!(
            float_bits(f64::from_bits(0x7FF8_0000_0000_0000)),
            CANONICAL_NAN_BITS
        );
        assert_eq!(
            float_bits(f64::from_bits(0x7FFC_0000_0000_0042)),
            CANONICAL_NAN_BITS
        );
    }

    #[test]
    fn test_int48_range() {
        assert!(int48_in_range(0));
        assert!(int48_in_range(INT48_MAX));
        assert!(int48_in_range(INT48_MIN));
        assert!(!int48_in_range(INT48_MAX + 1));
        assert!(!int48_in_range(INT48_MIN - 1));
        // tag_int/sext48 round-trip at the boundaries.
        assert_eq!(as_int_raw(tag_int(INT48_MAX)), INT48_MAX);
        assert_eq!(as_int_raw(tag_int(INT48_MIN)), INT48_MIN);
    }

    #[test]
    fn test_sext48_positive() {
        assert_eq!(sext48(42), 42);
        assert_eq!(sext48(0), 0);
    }

    #[test]
    fn test_sext48_negative() {
        let bits: u64 = 0x0000_FFFF_FFFF_FFFF; // -1 in 48 bits
        assert_eq!(sext48(bits), -1);
    }

    #[test]
    fn test_tag_int_roundtrip() {
        for n in [0, 1, -1, i16::MAX as i64, i16::MIN as i64] {
            let raw = tag_int(n);
            let payload = raw & PAYLOAD_MASK;
            assert_eq!(sext48(payload), n);
            assert_eq!(raw & TAG_MASK, TAG_INT);
        }
    }
}
