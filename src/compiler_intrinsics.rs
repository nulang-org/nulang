//! Backend-neutral compiler intrinsic contract.
//!
//! Intrinsics live above Cranelift/WASM/native lowering. They describe
//! operations the optimizer can reason about without exposing target-specific
//! assembly in Nulang source. Backends are expected to lower these semantic
//! operations to the cheapest target instruction sequence available.

use crate::bytecode::Constant;
use crate::mir::LocalId;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirIntrinsic {
    pub op: IntegerIntrinsic,
    pub args: Vec<LocalId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntrinsicError {
    WrongArity {
        intrinsic: IntegerIntrinsic,
        expected: usize,
        actual: usize,
    },
}

impl std::fmt::Display for IntrinsicError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongArity {
                intrinsic,
                expected,
                actual,
            } => write!(
                f,
                "intrinsic '{}' expects {expected} argument(s), got {actual}",
                intrinsic.stable_name()
            ),
        }
    }
}

impl std::error::Error for IntrinsicError {}

impl MirIntrinsic {
    pub fn new(op: IntegerIntrinsic, args: Vec<LocalId>) -> Result<Self, IntrinsicError> {
        if args.len() != op.arity() {
            return Err(IntrinsicError::WrongArity {
                intrinsic: op,
                expected: op.arity(),
                actual: args.len(),
            });
        }
        Ok(Self { op, args })
    }

    /// Fold the intrinsic when every MIR operand is available as an integer
    /// constant. Inputs are canonicalized through Nulang's signed 48-bit Int
    /// payload first, exactly as the VM observes a loaded `Constant::Int`.
    /// Non-integer constants deliberately decline folding rather than
    /// inventing coercion semantics.
    pub fn fold_constants(&self, args: &[Constant]) -> Option<Constant> {
        if args.len() != self.op.arity() {
            return None;
        }
        let ints = args
            .iter()
            .map(|arg| match arg {
                Constant::Int(value) => Some(crate::value_layout::sext48(
                    (*value as u64) & crate::value_layout::PAYLOAD_MASK,
                )),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        self.op.fold_i64(&ints).map(Constant::Int)
    }
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

/// Lower one raw-i64 intrinsic directly to Cranelift IR.
///
/// This function deliberately operates on raw i64 values rather than Nulang's
/// NaN-tagged runtime representation. Native/AOT callers are responsible for
/// unboxing integer operands before calling and re-boxing the result when the
/// surrounding function uses the boxed ABI. Keeping representation handling
/// outside this primitive gives every native caller the same instruction-level
/// semantics while avoiding hidden allocations or runtime calls here.
#[cfg(feature = "native-codegen")]
pub fn lower_cranelift_i64(
    builder: &mut cranelift_frontend::FunctionBuilder<'_>,
    intrinsic: IntegerIntrinsic,
    args: &[cranelift::prelude::Value],
) -> Result<cranelift::prelude::Value, IntrinsicError> {
    use cranelift::prelude::InstBuilder as _;

    if args.len() != intrinsic.arity() {
        return Err(IntrinsicError::WrongArity {
            intrinsic,
            expected: intrinsic.arity(),
            actual: args.len(),
        });
    }

    let value = args[0];
    Ok(match intrinsic {
        IntegerIntrinsic::Popcount => builder.ins().popcnt(value),
        IntegerIntrinsic::LeadingZeros => builder.ins().clz(value),
        IntegerIntrinsic::TrailingZeros => builder.ins().ctz(value),
        IntegerIntrinsic::RotateLeft => builder.ins().rotl(value, args[1]),
        IntegerIntrinsic::RotateRight => builder.ins().rotr(value, args[1]),
        IntegerIntrinsic::ByteSwap => builder.ins().bswap(value),
    })
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

    #[test]
    fn mir_intrinsic_validates_arity_and_folds_integer_constants() {
        let call =
            MirIntrinsic::new(IntegerIntrinsic::RotateLeft, vec![LocalId(0), LocalId(1)]).unwrap();
        assert_eq!(
            call.fold_constants(&[Constant::Int(1), Constant::Int(65)]),
            Some(Constant::Int(2))
        );
        assert_eq!(
            call.fold_constants(&[Constant::Int(1), Constant::Bool(true)]),
            None
        );

        let err = MirIntrinsic::new(IntegerIntrinsic::Popcount, vec![]).unwrap_err();
        assert_eq!(
            err,
            IntrinsicError::WrongArity {
                intrinsic: IntegerIntrinsic::Popcount,
                expected: 1,
                actual: 0,
            }
        );
    }
}

#[cfg(all(test, feature = "native-codegen"))]
mod native_tests {
    use super::*;
    use cranelift::codegen::ir::{types, AbiParam, Function, InstBuilder};
    use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

    fn render_lowering(intrinsic: IntegerIntrinsic) -> String {
        let mut function = Function::new();
        for _ in 0..intrinsic.arity() {
            function.signature.params.push(AbiParam::new(types::I64));
        }
        function.signature.returns.push(AbiParam::new(types::I64));

        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let block = builder.create_block();
        builder.switch_to_block(block);
        builder.append_block_params_for_function_params(block);
        let args = builder.block_params(block).to_vec();
        let result = lower_cranelift_i64(&mut builder, intrinsic, &args).unwrap();
        builder.ins().return_(&[result]);
        builder.seal_all_blocks();
        builder.finalize();
        function.display().to_string()
    }

    #[test]
    fn cranelift_lowering_uses_native_integer_instructions() {
        let cases = [
            (IntegerIntrinsic::Popcount, "popcnt"),
            (IntegerIntrinsic::LeadingZeros, "clz"),
            (IntegerIntrinsic::TrailingZeros, "ctz"),
            (IntegerIntrinsic::RotateLeft, "rotl"),
            (IntegerIntrinsic::RotateRight, "rotr"),
            (IntegerIntrinsic::ByteSwap, "bswap"),
        ];

        for (intrinsic, opcode) in cases {
            let clif = render_lowering(intrinsic);
            assert!(
                clif.contains(opcode),
                "{} should lower to Cranelift {opcode}; CLIF:\n{clif}",
                intrinsic.stable_name()
            );
        }
    }

    #[test]
    fn cranelift_lowering_rejects_wrong_arity() {
        let mut function = Function::new();
        function.signature.params.push(AbiParam::new(types::I64));
        function.signature.returns.push(AbiParam::new(types::I64));
        let mut context = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut function, &mut context);
        let block = builder.create_block();
        builder.switch_to_block(block);
        builder.append_block_params_for_function_params(block);
        let args = builder.block_params(block).to_vec();

        let err =
            lower_cranelift_i64(&mut builder, IntegerIntrinsic::RotateLeft, &args).unwrap_err();
        assert_eq!(
            err,
            IntrinsicError::WrongArity {
                intrinsic: IntegerIntrinsic::RotateLeft,
                expected: 2,
                actual: 1,
            }
        );
    }
}
