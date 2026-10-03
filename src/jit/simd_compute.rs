//! Adapter between the bytecode SIMD analyzer and backend-neutral compute IR.
//!
//! The bytecode analyzer still owns register-level pattern metadata while the
//! portable compute layers own canonical vector types and iteration spaces.
//! Runtime bindings stay in this adapter so bytecode register IDs never leak
//! into backend-neutral scheduling metadata.

use std::fmt;

use cranelift::prelude::*;
use cranelift_frontend::FunctionBuilderContext;
use cranelift_jit::JITModule;

use crate::bytecode::Instruction;
use crate::compute_ir::{LocalityScope, ScalarType, VectorType, VectorWidth};
use crate::compute_schedule::{
    ComputeScheduleError, DynamicExtentId, IterationSpace, LoopExtent,
};
use crate::jit::simd_analyzer::{SimdElemType, SimdRegion, SimdWidth};
use crate::jit::simd_compiler;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimdTripCountBinding {
    Static(u64),
    RuntimeArrayLen {
        extent: DynamicExtentId,
        register: u8,
    },
    /// The analyzer did not recover a trip count. Existing Cranelift lowering
    /// deliberately keeps its scalar fallback for this case.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SimdComputePlan {
    pub(crate) vector_type: VectorType,
    pub(crate) iteration: Option<IterationSpace>,
    pub(crate) trip_count: SimdTripCountBinding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimdComputePlanError {
    WidthMismatch {
        analyzer_lanes: u16,
        element_lanes: u16,
    },
    ElementTypeMismatch {
        analyzer: ScalarType,
        planned: ScalarType,
    },
    UnsupportedScalableLowering,
    UnsupportedLoweringWidth(u16),
    StaticTripCountTooLarge(u64),
    MissingRuntimeExtentBinding,
    Schedule(ComputeScheduleError),
}

impl fmt::Display for SimdComputePlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WidthMismatch {
                analyzer_lanes,
                element_lanes,
            } => write!(
                f,
                "SIMD analyzer width {analyzer_lanes} does not match element-derived width {element_lanes}"
            ),
            Self::ElementTypeMismatch { analyzer, planned } => write!(
                f,
                "SIMD analyzer element type {analyzer:?} does not match compute plan type {planned:?}"
            ),
            Self::UnsupportedScalableLowering => {
                write!(f, "Cranelift SIMD lowering does not yet support scalable vectors")
            }
            Self::UnsupportedLoweringWidth(lanes) => {
                write!(f, "Cranelift SIMD lowering does not support {lanes} lanes")
            }
            Self::StaticTripCountTooLarge(count) => {
                write!(f, "static SIMD trip count {count} does not fit usize")
            }
            Self::MissingRuntimeExtentBinding => write!(
                f,
                "SIMD region marks its trip count as runtime-derived but has no ArrLen register binding"
            ),
            Self::Schedule(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SimdComputePlanError {}

impl From<ComputeScheduleError> for SimdComputePlanError {
    fn from(error: ComputeScheduleError) -> Self {
        Self::Schedule(error)
    }
}

impl SimdComputePlan {
    pub(crate) fn from_region(region: &SimdRegion) -> Result<Self, SimdComputePlanError> {
        let analyzer_lanes = analyzer_lanes(region.width);
        let element_lanes = region.elem_type.lane_count() as u16;
        if analyzer_lanes != element_lanes {
            return Err(SimdComputePlanError::WidthMismatch {
                analyzer_lanes,
                element_lanes,
            });
        }

        // Current bytecode SIMD analysis only produces non-zero fixed widths.
        // Keep the checked compute-IR constructor as the authority so future
        // scalable/target-selected widths cannot silently bypass validation.
        let width = VectorWidth::fixed(analyzer_lanes)
            .expect("SIMD analyzer only exposes non-zero fixed widths");
        let vector_type = VectorType::new(scalar_type(region.elem_type), width);

        let (iteration, trip_count) = match (region.trip_count_hint, region.arr_len_reg) {
            (Some(0), Some(register)) => {
                let extent = DynamicExtentId(0);
                let iteration = IterationSpace::new(
                    0,
                    LoopExtent::Dynamic(extent),
                    1,
                    LocalityScope::Lane,
                )?;
                (
                    Some(iteration),
                    SimdTripCountBinding::RuntimeArrayLen { extent, register },
                )
            }
            (Some(0), None) => return Err(SimdComputePlanError::MissingRuntimeExtentBinding),
            (Some(count), _) => {
                let count = count as u64;
                let iteration = IterationSpace::new(
                    0,
                    LoopExtent::Static(count),
                    1,
                    LocalityScope::Lane,
                )?;
                (Some(iteration), SimdTripCountBinding::Static(count))
            }
            (None, _) => (None, SimdTripCountBinding::Unavailable),
        };

        Ok(Self {
            vector_type,
            iteration,
            trip_count,
        })
    }

    /// Materialize the legacy analyzer structure expected by the current
    /// Cranelift emitter, but overwrite every lowering decision already owned
    /// by the compute plan. This keeps register/pattern metadata in the legacy
    /// structure while making portable compute metadata authoritative.
    pub(crate) fn lowering_region(
        self,
        region: &SimdRegion,
    ) -> Result<SimdRegion, SimdComputePlanError> {
        let analyzer_element = scalar_type(region.elem_type);
        if analyzer_element != self.vector_type.element {
            return Err(SimdComputePlanError::ElementTypeMismatch {
                analyzer: analyzer_element,
                planned: self.vector_type.element,
            });
        }

        let width = lowering_width(self.vector_type.width)?;
        let (trip_count_hint, arr_len_reg) = match self.trip_count {
            SimdTripCountBinding::Static(count) => {
                let count = usize::try_from(count)
                    .map_err(|_| SimdComputePlanError::StaticTripCountTooLarge(count))?;
                (Some(count), None)
            }
            SimdTripCountBinding::RuntimeArrayLen { register, .. } => (Some(0), Some(register)),
            SimdTripCountBinding::Unavailable => (None, None),
        };

        let mut lowering = region.clone();
        lowering.width = width;
        lowering.trip_count_hint = trip_count_hint;
        lowering.arr_len_reg = arr_len_reg;
        Ok(lowering)
    }
}

const fn scalar_type(elem_type: SimdElemType) -> ScalarType {
    match elem_type {
        SimdElemType::Int64 => ScalarType::I64,
        SimdElemType::Float64 => ScalarType::F64,
        SimdElemType::Int32 => ScalarType::I32,
        SimdElemType::Float32 => ScalarType::F32,
    }
}

const fn analyzer_lanes(width: SimdWidth) -> u16 {
    match width {
        SimdWidth::Width2 => 2,
        SimdWidth::Width4 => 4,
        SimdWidth::Width8 => 8,
    }
}

fn lowering_width(width: VectorWidth) -> Result<SimdWidth, SimdComputePlanError> {
    if width.is_scalable() {
        return Err(SimdComputePlanError::UnsupportedScalableLowering);
    }

    match width.min_lanes() {
        2 => Ok(SimdWidth::Width2),
        4 => Ok(SimdWidth::Width4),
        8 => Ok(SimdWidth::Width8),
        lanes => Err(SimdComputePlanError::UnsupportedLoweringWidth(lanes)),
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
    let plan = SimdComputePlan::from_region(region).map_err(|error| error.to_string())?;
    let lowering_region = plan
        .lowering_region(region)
        .map_err(|error| error.to_string())?;

    simd_compiler::compile_simd_region(
        module,
        builder_context,
        ctx,
        func_name,
        instructions,
        &lowering_region,
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
    fn maps_static_trip_count_to_static_iteration_space() {
        let plan = SimdComputePlan::from_region(&region(SimdElemType::Int64, SimdWidth::Width2))
            .expect("static trip count should normalize");
        let iteration = plan.iteration.expect("static iteration space");

        assert_eq!(iteration.extent, LoopExtent::Static(16));
        assert_eq!(iteration.end_exclusive().unwrap(), Some(16));
        assert_eq!(plan.trip_count, SimdTripCountBinding::Static(16));
    }

    #[test]
    fn maps_runtime_arr_len_to_symbolic_dynamic_extent() {
        let mut input = region(SimdElemType::Float64, SimdWidth::Width2);
        input.trip_count_hint = Some(0);
        input.arr_len_reg = Some(9);

        let plan = SimdComputePlan::from_region(&input).expect("runtime extent should normalize");
        let iteration = plan.iteration.expect("dynamic iteration space");

        assert_eq!(
            iteration.extent,
            LoopExtent::Dynamic(DynamicExtentId(0))
        );
        assert_eq!(iteration.end_exclusive().unwrap(), None);
        assert_eq!(
            plan.trip_count,
            SimdTripCountBinding::RuntimeArrayLen {
                extent: DynamicExtentId(0),
                register: 9,
            }
        );
    }

    #[test]
    fn preserves_scalar_fallback_when_trip_count_is_unavailable() {
        let mut input = region(SimdElemType::Int64, SimdWidth::Width2);
        input.trip_count_hint = None;
        input.arr_len_reg = None;

        let plan = SimdComputePlan::from_region(&input).expect("unknown extent is not invalid");

        assert_eq!(plan.iteration, None);
        assert_eq!(plan.trip_count, SimdTripCountBinding::Unavailable);
    }

    #[test]
    fn rejects_runtime_trip_count_without_arr_len_binding() {
        let mut input = region(SimdElemType::Int64, SimdWidth::Width2);
        input.trip_count_hint = Some(0);
        input.arr_len_reg = None;

        assert_eq!(
            SimdComputePlan::from_region(&input),
            Err(SimdComputePlanError::MissingRuntimeExtentBinding)
        );
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

    #[test]
    fn lowering_region_uses_compute_plan_as_authority() {
        let mut input = region(SimdElemType::Int64, SimdWidth::Width4);
        input.trip_count_hint = Some(99);
        input.arr_len_reg = None;

        let extent = DynamicExtentId(3);
        let plan = SimdComputePlan {
            vector_type: VectorType::new(ScalarType::I64, VectorWidth::fixed(2).unwrap()),
            iteration: Some(
                IterationSpace::new(
                    0,
                    LoopExtent::Dynamic(extent),
                    1,
                    LocalityScope::Lane,
                )
                .unwrap(),
            ),
            trip_count: SimdTripCountBinding::RuntimeArrayLen {
                extent,
                register: 11,
            },
        };

        let lowering = plan
            .lowering_region(&input)
            .expect("compute plan should normalize the legacy lowering input");

        assert_eq!(lowering.width, SimdWidth::Width2);
        assert_eq!(lowering.trip_count_hint, Some(0));
        assert_eq!(lowering.arr_len_reg, Some(11));
    }

    #[test]
    fn lowering_region_rejects_scalable_vectors_until_backend_support_exists() {
        let input = region(SimdElemType::Int64, SimdWidth::Width2);
        let plan = SimdComputePlan {
            vector_type: VectorType::new(ScalarType::I64, VectorWidth::scalable(2).unwrap()),
            iteration: None,
            trip_count: SimdTripCountBinding::Unavailable,
        };

        assert_eq!(
            plan.lowering_region(&input),
            Err(SimdComputePlanError::UnsupportedScalableLowering)
        );
    }
}
