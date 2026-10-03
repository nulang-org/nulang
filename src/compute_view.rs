//! Symbolic buffer views for compute planning when one or more dimensions are
//! only known at runtime.

use std::fmt;

use crate::compute_ir::{ComputeIrError, Layout, ScalarType, VectorType};
use crate::compute_planner::{plan_vector_loop, ComputePlannerError, VectorLoopPlan};
use crate::compute_schedule::{DynamicExtentId, IterationSpace, LoopExtent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShapeExtent {
    Static(u64),
    Dynamic(DynamicExtentId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferView {
    element: ScalarType,
    shape: Vec<ShapeExtent>,
    strides: Vec<u64>,
    alignment: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeViewError {
    RankMismatch {
        shape_rank: usize,
        stride_rank: usize,
    },
    InvalidAlignment(u32),
}

impl fmt::Display for ComputeViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RankMismatch {
                shape_rank,
                stride_rank,
            } => write!(
                f,
                "buffer view shape rank {shape_rank} does not match stride rank {stride_rank}"
            ),
            Self::InvalidAlignment(alignment) => write!(
                f,
                "buffer view alignment must be a non-zero power of two, got {alignment}"
            ),
        }
    }
}

impl std::error::Error for ComputeViewError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputeViewPlanError {
    NotImplemented,
    AxisOutOfBounds { axis: usize, rank: usize },
    Ir(ComputeIrError),
    Planner(ComputePlannerError),
}

impl fmt::Display for ComputeViewPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented => write!(f, "symbolic buffer view planning is not implemented"),
            Self::AxisOutOfBounds { axis, rank } => {
                write!(f, "vector axis {axis} is outside buffer view rank {rank}")
            }
            Self::Ir(error) => write!(f, "{error}"),
            Self::Planner(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ComputeViewPlanError {}

impl From<ComputeIrError> for ComputeViewPlanError {
    fn from(error: ComputeIrError) -> Self {
        Self::Ir(error)
    }
}

impl From<ComputePlannerError> for ComputeViewPlanError {
    fn from(error: ComputePlannerError) -> Self {
        Self::Planner(error)
    }
}

impl BufferView {
    pub fn from_parts(
        element: ScalarType,
        shape: impl Into<Vec<ShapeExtent>>,
        strides: impl Into<Vec<u64>>,
        alignment: u32,
    ) -> Result<Self, ComputeViewError> {
        let shape = shape.into();
        let strides = strides.into();

        if shape.len() != strides.len() {
            return Err(ComputeViewError::RankMismatch {
                shape_rank: shape.len(),
                stride_rank: strides.len(),
            });
        }
        if alignment == 0 || !alignment.is_power_of_two() {
            return Err(ComputeViewError::InvalidAlignment(alignment));
        }

        Ok(Self {
            element,
            shape,
            strides,
            alignment,
        })
    }

    pub fn from_layout(layout: &Layout) -> Self {
        Self {
            element: layout.element(),
            shape: layout
                .shape()
                .iter()
                .copied()
                .map(ShapeExtent::Static)
                .collect(),
            strides: layout.strides().to_vec(),
            alignment: layout.alignment(),
        }
    }

    pub const fn element(&self) -> ScalarType {
        self.element
    }

    pub fn shape(&self) -> &[ShapeExtent] {
        &self.shape
    }

    pub fn strides(&self) -> &[u64] {
        &self.strides
    }

    pub const fn alignment(&self) -> u32 {
        self.alignment
    }

    pub fn rank(&self) -> usize {
        self.shape.len()
    }
}

pub fn plan_vector_view(
    _view: &BufferView,
    _axis: usize,
    _iteration: IterationSpace,
    _vector: VectorType,
) -> Result<VectorLoopPlan, ComputeViewPlanError> {
    Err(ComputeViewPlanError::NotImplemented)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_ir::{LocalityScope, ScalarType, VectorType, VectorWidth};

    fn fixed_f32x4() -> VectorType {
        VectorType::new(ScalarType::F32, VectorWidth::fixed(4).unwrap())
    }

    #[test]
    fn creates_symbolic_one_dimensional_view() {
        let extent = DynamicExtentId(7);
        let view = BufferView::from_parts(
            ScalarType::F32,
            vec![ShapeExtent::Dynamic(extent)],
            vec![1],
            4,
        )
        .unwrap();

        assert_eq!(view.element(), ScalarType::F32);
        assert_eq!(view.shape(), &[ShapeExtent::Dynamic(extent)]);
        assert_eq!(view.strides(), &[1]);
        assert_eq!(view.alignment(), 4);
        assert_eq!(view.rank(), 1);
    }

    #[test]
    fn converts_static_layout_without_losing_shape_or_stride() {
        let layout = Layout::from_parts(ScalarType::I64, vec![3, 5], vec![5, 1], 8).unwrap();
        let view = BufferView::from_layout(&layout);

        assert_eq!(
            view.shape(),
            &[ShapeExtent::Static(3), ShapeExtent::Static(5)]
        );
        assert_eq!(view.strides(), &[5, 1]);
        assert_eq!(view.alignment(), 8);
    }

    #[test]
    fn rejects_rank_mismatch() {
        assert_eq!(
            BufferView::from_parts(
                ScalarType::F32,
                vec![ShapeExtent::Static(4), ShapeExtent::Static(4)],
                vec![1],
                4,
            ),
            Err(ComputeViewError::RankMismatch {
                shape_rank: 2,
                stride_rank: 1,
            })
        );
    }

    #[test]
    fn rejects_invalid_alignment() {
        assert_eq!(
            BufferView::from_parts(
                ScalarType::F32,
                vec![ShapeExtent::Static(4)],
                vec![1],
                3,
            ),
            Err(ComputeViewError::InvalidAlignment(3))
        );
    }

    #[test]
    fn shared_dynamic_extent_elides_redundant_bounds_check() {
        let extent = DynamicExtentId(5);
        let view = BufferView::from_parts(
            ScalarType::F32,
            vec![ShapeExtent::Dynamic(extent)],
            vec![1],
            4,
        )
        .unwrap();
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Dynamic(extent),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        let plan = plan_vector_view(&view, 0, iteration, fixed_f32x4()).unwrap();

        assert_eq!(plan.vector_iterations, None);
        assert_eq!(plan.scalar_tail, None);
        assert!(plan.requires_runtime_tail);
        assert!(!plan.requires_runtime_bounds_check);
    }

    #[test]
    fn different_dynamic_extents_keep_runtime_bounds_check() {
        let view = BufferView::from_parts(
            ScalarType::F32,
            vec![ShapeExtent::Dynamic(DynamicExtentId(5))],
            vec![1],
            4,
        )
        .unwrap();
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Dynamic(DynamicExtentId(6)),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        let plan = plan_vector_view(&view, 0, iteration, fixed_f32x4()).unwrap();

        assert!(plan.requires_runtime_bounds_check);
        assert!(plan.requires_runtime_tail);
    }

    #[test]
    fn static_iteration_over_dynamic_view_has_exact_tail_but_runtime_bounds_check() {
        let view = BufferView::from_parts(
            ScalarType::F32,
            vec![ShapeExtent::Dynamic(DynamicExtentId(5))],
            vec![1],
            4,
        )
        .unwrap();
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Static(10),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        let plan = plan_vector_view(&view, 0, iteration, fixed_f32x4()).unwrap();

        assert_eq!(plan.vector_iterations, Some(2));
        assert_eq!(plan.scalar_tail, Some(2));
        assert!(plan.requires_runtime_bounds_check);
        assert!(!plan.requires_runtime_tail);
    }

    #[test]
    fn static_view_delegates_to_static_layout_rules() {
        let layout = Layout::row_major(ScalarType::F32, vec![10]).unwrap();
        let view = BufferView::from_layout(&layout);
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Static(10),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        let from_view = plan_vector_view(&view, 0, iteration, fixed_f32x4()).unwrap();
        let from_layout = plan_vector_loop(&layout, 0, iteration, fixed_f32x4()).unwrap();

        assert_eq!(from_view, from_layout);
    }

    #[test]
    fn rejects_axis_outside_symbolic_view_rank() {
        let view = BufferView::from_parts(
            ScalarType::F32,
            vec![ShapeExtent::Dynamic(DynamicExtentId(1))],
            vec![1],
            4,
        )
        .unwrap();
        let iteration = IterationSpace::new(
            0,
            LoopExtent::Dynamic(DynamicExtentId(1)),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        assert_eq!(
            plan_vector_view(&view, 1, iteration, fixed_f32x4()),
            Err(ComputeViewPlanError::AxisOutOfBounds { axis: 1, rank: 1 })
        );
    }
}
