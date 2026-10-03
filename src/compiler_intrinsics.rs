//! Backend-neutral compiler intrinsic contract.
//!
//! Intrinsics live above Cranelift/WASM/native lowering. They describe
//! operations the optimizer can reason about without exposing target-specific
//! assembly in Nulang source.

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
    pub fn stable_name(self) -> &'static str {
        todo!("implemented after the RED tests")
    }

    pub fn arity(self) -> usize {
        todo!("implemented after the RED tests")
    }

    pub fn effect(self) -> IntrinsicEffect {
        todo!("implemented after the RED tests")
    }

    pub fn may_trap(self) -> bool {
        todo!("implemented after the RED tests")
    }

    pub fn fold_i64(self, _args: &[i64]) -> Option<i64> {
        todo!("implemented after the RED tests")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_arity_are_stable() {
        assert_eq!(IntegerIntrinsic::Popcount.stable_name(), "int.popcount");
        assert_eq!(IntegerIntrinsic::RotateLeft.stable_name(), "int.rotate_left");
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
    fn constant_folding_rejects_wrong_arity() {
        assert_eq!(IntegerIntrinsic::Popcount.fold_i64(&[]), None);
        assert_eq!(IntegerIntrinsic::Popcount.fold_i64(&[1, 2]), None);
        assert_eq!(IntegerIntrinsic::RotateLeft.fold_i64(&[1]), None);
    }
}
