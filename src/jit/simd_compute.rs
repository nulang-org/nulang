//! Adapter between the bytecode SIMD analyzer and backend-neutral compute IR.
//!
//! The bytecode analyzer still owns register-level pattern metadata while the
//! portable compute IR owns the canonical scalar/vector type vocabulary. This
//! bridge lets native lowering adopt that shared representation incrementally
//! without changing vectorization heuristics or runtime trip-count bindings.

use std::fmt;

use cranelift::prelude::*;
use cranelift_frontend::FunctionBuilderContext;
use cranelift_jit::JITModule;

use crate::bytecode::Instruction;
use crate::compute_ir::{ScalarType, VectorType, VectorWidth};
use crate::jit::simd_analyzer::{SimdElemType, SimdRegion, SimdWidth};
use crate::jit::simd_compiler;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SimdComputePlan {
    pub(crate) vector_type: VectorType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimdComputePlanError {
    NotImplemented,
    WidthMismatch {
        analyzer_lanes: u16,
        element_lanes: u16,
    },
}

impl fmt::Display for SimdComputePlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented => write!(f, "SIMD compute IR normalization is not implemented"),
            Self::WidthMismatch {
                analyzer_lanes,
                element_lanes,
            } => write!(
                f,
                "SIMD analyzer width {analyzer_lanes} does not match element-derived width {element_lanes}"
            ),
        }
    }
}

impl std::error::Error for SimdComputePlanError {}

impl SimdComputePlan {
    pub(crate) fn from_region(_region: &SimdRegion) -> Result<Self, SimdComputePlanError> {
        Err(SimdComputePlanError::NotImplemented)
    }
}

pub(crate) fn compile_simd_region(
    module: &mut JITModule,
    builder_context: &mut FunctionBuilderContext,
    ctx: &mut codegen::Context,
    func_name: &str,
    instructions: &[Instruction],
    region: &SimdRegion,
) -> Result<*const u8, String> {
    let _plan = SimdComputePlan::from_region(region).map_err(|error| error.to_string())?;
    simd_compiler::compile_simd_region(
        module,
        builder_context,
        ctx,
        func_name,
        instructions,
        region,
    )
    .map_err(|error| format!("{error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::simd_analyzer::{BinopKind, VectorizablePattern};

    fn region(elem_type: SimdElemType, width: SimdWidth) -> SimdRegion {
        SimdRegion {
            start_offset: 0,
            num_instrs: 6,
            pattern: VectorizablePattern::ElementWiseBinop {
                op: BinopKind::IAdd,
                lhs_arr_reg: 0,
                rhs_arr_reg: 1,
                dst_arr_reg: 2,
                lhs_elem_reg: 4,
                rhs_elem_reg: 5,
                result_reg: 6,
            },
            width,
            elem_type,
            induction_var_reg: 3,
            array_regs: vec![0, 1, 2],
            trip_count_hint: Some(16),
            arr_len_reg: None,
        }
    }

    #[test]
    fn normalizes_i64x2_to_compute_ir_vector_type() {
        let plan = SimdComputePlan::from_region(&region(SimdElemType::Int64, SimdWidth::Width2))
            .expect("i64x2 should normalize");

        assert_eq!(plan.vector_type.element, ScalarType::I64);
        assert_eq!(plan.vector_type.width, VectorWidth::fixed(2).unwrap());
    }

    #[test]
    fn normalizes_f32x4_to_compute_ir_vector_type() {
        let plan = SimdComputePlan::from_region(&region(SimdElemType::Float32, SimdWidth::Width4))
            .expect("f32x4 should normalize");

        assert_eq!(plan.vector_type.element, ScalarType::F32);
        assert_eq!(plan.vector_type.width, VectorWidth::fixed(4).unwrap());
    }

    #[test]
    fn rejects_analyzer_width_that_disagrees_with_element_type() {
        let error = SimdComputePlan::from_region(&region(SimdElemType::Int64, SimdWidth::Width4))
            .expect_err("i64 must use two lanes in the current 128-bit JIT");

        assert_eq!(
            error,
            SimdComputePlanError::WidthMismatch {
                analyzer_lanes: 4,
                element_lanes: 2,
            }
        );
    }
}
