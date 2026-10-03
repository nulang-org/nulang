//! Backend-neutral compiler intrinsic contract.
//!
//! Intrinsics live above Cranelift/WASM/native lowering. They describe
//! operations the optimizer can reason about without exposing target-specific
//! assembly in Nulang source. Backends are expected to lower these semantic
//! operations to the cheapest target instruction sequence available.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegerIntrinsic {
    Popcount,
    LeadingZeros,
    TrailingZeros,
    RotateLeft,
    RotateRight,
    ByteSwap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntrinsicEffect {
    Pure,
}

impl IntegerIntrinsic {
    /// Stable compiler-owned name. These names are intentionally independent
    /// of any backend opcode spelling so Cranelift, WASM, and future codegens
    /// can share the same semantic contract.
    pub const fn stable_name(self) -> &'static str {
        match self {
            Self::Popcount => "int.popcount",
            Self::LeadingZeros => "int.leading_zeros",
            Self::TrailingZeros => "int.trailing_zeros",
            Self::RotateLeft => "int.rotate_left",
            Self::RotateRight => "int.rotate_right",
            Self::ByteSwap => "int.byte_swap",
        }
    }

    pub const fn arity(self) -> usize {
        match self {
            Self::RotateLeft | Self::RotateRight => 2,
            Self::Popcount | Self::LeadingZeros | Self::TrailingZeros | Self::ByteSwap => 1,
        }
    }

    /// All first-wave integer intrinsics are referentially transparent. This
    /// lets MIR optimizers fold, CSE, reorder, or discard them when their
    /// result is provably unused.
    pub const fn effect(self) -> IntrinsicEffect {
        let _ = self;
        IntrinsicEffect::Pure
    }

    /// These bit operations are total over every i64 bit pattern. Rotation
    /// counts are masked to 0..63, giving every backend one explicit semantic
    /// rule rather than inheriting ISA-specific shift behavior.
    pub const fn may_trap(self) -> bool {
        let _ = self;
        false
    }

    /// Evaluate an intrinsic at compile time when every operand is constant.
    /// Returns `None` only when the caller supplied the wrong arity.
    pub fn fold_i64(self, args: &[i64]) -> Option<i64> {
        if args.len() != self.arity() {
            return None;
        }

        let value = args[0];
        Some(match self {
            Self::Popcount => i64::from(value.count_ones()),
            Self::LeadingZeros => i64::from(value.leading_zeros()),
            Self::TrailingZeros => i64::from(value.trailing_zeros()),
            Self::RotateLeft => {
                let amount = (args[1] as u64 & 63) as u32;
                (value as u64).rotate_left(amount) as i64
            }
            Self::RotateRight => {
                let amount = (args[1] as u64 & 63) as u32;
                (value as u64).rotate_right(amount) as i64
            }
            Self::ByteSwap => value.swap_bytes(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_arity_are_stable() {
        assert_eq!(IntegerIntrinsic::Popcount.stable_name(), "int.popcount");
        assert_eq!(
            IntegerIntrinsic::RotateLeft.stable_name(),
            "int.rotate_left"
        );
        assert_eq!(IntegerIntrinsic::Popcount.arity(), 1);
        assert_eq!(IntegerIntrinsic::RotateLeft.arity(), 2);
    }

    #[test]
    fn integer_intrinsics_are_pure_and_non_trapping() {
        for intrinsic in [
            IntegerIntrinsic::Popcount,
            IntegerIntrinsic::LeadingZeros,
            IntegerIntrinsic::TrailingZeros,
            IntegerIntrinsic::RotateLeft,
            IntegerIntrinsic::RotateRight,
            IntegerIntrinsic::ByteSwap,
        ] {
            assert_eq!(intrinsic.effect(), IntrinsicEffect::Pure);
            assert!(!intrinsic.may_trap());
        }
    }

    #[test]
    fn constant_folding_matches_integer_bit_semantics() {
        assert_eq!(IntegerIntrinsic::Popcount.fold_i64(&[0b1011]), Some(3));
        assert_eq!(IntegerIntrinsic::LeadingZeros.fold_i64(&[1]), Some(63));
        assert_eq!(IntegerIntrinsic::TrailingZeros.fold_i64(&[8]), Some(3));
        assert_eq!(IntegerIntrinsic::RotateLeft.fold_i64(&[1, 1]), Some(2));
        assert_eq!(IntegerIntrinsic::RotateRight.fold_i64(&[2, 1]), Some(1));
        assert_eq!(
            IntegerIntrinsic::ByteSwap.fold_i64(&[0x0102_0304_0506_0708]),
            Some(0x0807_0605_0403_0201)
        );
    }

    #[test]
    fn rotation_counts_have_explicit_modulo_64_semantics() {
        assert_eq!(IntegerIntrinsic::RotateLeft.fold_i64(&[1, 64]), Some(1));
        assert_eq!(IntegerIntrinsic::RotateLeft.fold_i64(&[1, 65]), Some(2));
        assert_eq!(IntegerIntrinsic::RotateRight.fold_i64(&[2, -1]), Some(4));
    }

    #[test]
    fn constant_folding_rejects_wrong_arity() {
        assert_eq!(IntegerIntrinsic::Popcount.fold_i64(&[]), None);
        assert_eq!(IntegerIntrinsic::Popcount.fold_i64(&[1, 2]), None);
        assert_eq!(IntegerIntrinsic::RotateLeft.fold_i64(&[1]), None);
    }
}
