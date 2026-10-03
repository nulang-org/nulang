//! Backend-neutral planning for layout-aware vector loops.
//!
//! This module decides whether an iteration space can be represented as
//! contiguous vector accesses over a physical `Layout`. It deliberately stops
//! before target-specific instruction selection: fixed-width backends can use
//! exact vector/tail counts, while scalable-width backends keep those counts
//! target-dependent.

use std::fmt;

use crate::compute_ir::{Layout, ScalarType, VectorType, VectorWidth, VectorWidthKind};
use crate::compute_schedule::{IterationSpace, LoopExtent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannedVectorWidth {
    Fixed(u16),
    ScalableMin(u16),
}

impl PlannedVectorWidth {
    pub const fn min_lanes(self) -> u16 {
        match self {
            Self::Fixed(lanes) | Self::ScalableMin(lanes) => lanes,
        }
    }

    pub const fn is_scalable(self) -> bool {
        matches!(self, Self::ScalableMin(_))
    }
}

impl From<VectorWidth> for PlannedVectorWidth {
    fn from(width: VectorWidth) -> Self {
        match width.kind() {
            VectorWidthKind::Fixed => Self::Fixed(width.min_lanes()),
            VectorWidthKind::Scalable => Self::ScalableMin(width.min_lanes()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorLoopPlan {
    pub axis: usize,
    pub width: PlannedVectorWidth,
    pub min_vector_bytes: u64,
    pub vector_iterations: Option<u64>,
    pub scalar_tail: Option<u16>,
    pub requires_runtime_tail: bool,
    pub requires_runtime_bounds_check: bool,
    pub start_is_vector_aligned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputePlannerError {
    NotImplemented,
    AxisOutOfBounds { axis: usize, rank: usize },
    ElementTypeMismatch {
        layout: ScalarType,
        vector: ScalarType,
    },
    NonContiguousAxis { axis: usize, stride: u64 },
    UnsupportedStep(i64),
    NegativeStart(i64),
    StaticRangeOutOfBounds {
        axis: usize,
        start: u64,
        end: u64,
        axis_extent: u64,
    },
}

impl fmt::Display for ComputePlannerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented => write!(f, "layout-aware vector planning is not implemented"),
            Self::AxisOutOfBounds { axis, rank } => {
                write!(f, "vector axis {axis} is outside layout rank {rank}")
            }
            Self::ElementTypeMismatch { layout, vector } => write!(
                f,
                "layout element type {layout:?} does not match vector element type {vector:?}"
            ),
            Self::NonContiguousAxis { axis, stride } => {
                write!(f, "vector axis {axis} has non-unit stride {stride}")
            }
            Self::UnsupportedStep(step) => {
                write!(f, "vector planning currently requires step=1, got {step}")
            }
            Self::NegativeStart(start) => {
                write!(f, "vector planning requires a non-negative start, got {start}")
            }
            Self::StaticRangeOutOfBounds {
                axis,
                start,
                end,
                axis_extent,
            } => write!(
                f,
                "static vector range {start}..{end} exceeds axis {axis} extent {axis_extent}"
            ),
        }
    }
}

impl std::error::Error for ComputePlannerError {}

pub fn plan_vector_loop(
    _layout: &Layout,
    _axis: usize,
    _iteration: IterationSpace,
    _vector: VectorType,
) -> Result<VectorLoopPlan, ComputePlannerError> {
    Err(ComputePlannerError::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_ir::{LocalityScope, ScalarType, VectorType, VectorWidth};
    use crate::compute_schedule::{DynamicExtentId, IterationSpace, LoopExtent};

    fn fixed_f32x4() -> VectorType {
        VectorType::new(ScalarType::F32, VectorWidth::fixed(4).unwrap())
    }

    #[test]
    fn plans_exact_fixed_width_vector_iterations() {
        let layout = Layout::from_parts(ScalarType::F32, vec![100], vec![1], 16).unwrap();
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Static(100),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        let plan = plan_vector_loop(&layout, 0, iteration, fixed_f32x4()).unwrap();

        assert_eq!(plan.width, PlannedVectorWidth::Fixed(4));
        assert_eq!(plan.min_vector_bytes, 16);
        assert_eq!(plan.vector_iterations, Some(25));
        assert_eq!(plan.scalar_tail, Some(0));
        assert!(!plan.requires_runtime_tail);
        assert!(!plan.requires_runtime_bounds_check);
        assert!(plan.start_is_vector_aligned);
    }

    #[test]
    fn computes_scalar_tail_for_static_fixed_width_loop() {
        let layout = Layout::row_major(ScalarType::F32, vec![10]).unwrap();
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Static(10),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        let plan = plan_vector_loop(&layout, 0, iteration, fixed_f32x4()).unwrap();

        assert_eq!(plan.vector_iterations, Some(2));
        assert_eq!(plan.scalar_tail, Some(2));
        assert!(!plan.requires_runtime_tail);
    }

    #[test]
    fn keeps_dynamic_extent_tail_and_bounds_runtime_bound() {
        let layout = Layout::row_major(ScalarType::F32, vec![128]).unwrap();
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Dynamic(DynamicExtentId(4)),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        let plan = plan_vector_loop(&layout, 0, iteration, fixed_f32x4()).unwrap();

        assert_eq!(plan.vector_iterations, None);
        assert_eq!(plan.scalar_tail, None);
        assert!(plan.requires_runtime_tail);
        assert!(plan.requires_runtime_bounds_check);
    }

    #[test]
    fn scalable_width_preserves_target_dependent_tail() {
        let layout = Layout::from_parts(ScalarType::F32, vec![64], vec![1], 16).unwrap();
        let vector = VectorType::new(ScalarType::F32, VectorWidth::scalable(4).unwrap());
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Static(64),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        let plan = plan_vector_loop(&layout, 0, iteration, vector).unwrap();

        assert_eq!(plan.width, PlannedVectorWidth::ScalableMin(4));
        assert_eq!(plan.vector_iterations, None);
        assert_eq!(plan.scalar_tail, None);
        assert!(plan.requires_runtime_tail);
        assert!(!plan.requires_runtime_bounds_check);
        assert!(!plan.start_is_vector_aligned);
    }

    #[test]
    fn validates_static_subrange_against_axis_extent() {
        let layout = Layout::row_major(ScalarType::F32, vec![16]).unwrap();
        let iteration = IterationSpace::new(
            12,
            LoopExtent::Static(8),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        assert_eq!(
            plan_vector_loop(&layout, 0, iteration, fixed_f32x4()),
            Err(ComputePlannerError::StaticRangeOutOfBounds {
                axis: 0,
                start: 12,
                end: 20,
                axis_extent: 16,
            })
        );
    }

    #[test]
    fn rejects_non_contiguous_axis() {
        let layout = Layout::from_parts(ScalarType::F32, vec![8, 8], vec![8, 1], 4).unwrap();
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Static(8),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        assert_eq!(
            plan_vector_loop(&layout, 0, iteration, fixed_f32x4()),
            Err(ComputePlannerError::NonContiguousAxis { axis: 0, stride: 8 })
        );
    }

    #[test]
    fn rejects_element_type_mismatch() {
        let layout = Layout::row_major(ScalarType::I64, vec![16]).unwrap();
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Static(16),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        assert_eq!(
            plan_vector_loop(&layout, 0, iteration, fixed_f32x4()),
            Err(ComputePlannerError::ElementTypeMismatch {
                layout: ScalarType::I64,
                vector: ScalarType::F32,
            })
        );
    }

    #[test]
    fn rejects_axis_outside_layout_rank() {
        let layout = Layout::row_major(ScalarType::F32, vec![4, 4]).unwrap();
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Static(4),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        assert_eq!(
            plan_vector_loop(&layout, 2, iteration, fixed_f32x4()),
            Err(ComputePlannerError::AxisOutOfBounds { axis: 2, rank: 2 })
        );
    }
}
