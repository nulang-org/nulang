//! Backend-neutral tiling for already-validated vector loops.
//!
//! `compute_planner` establishes whether vector access is legal. This module
//! chunks that legal work into a target byte budget without inventing a fixed
//! lane count for scalable vectors.

use std::fmt;

use crate::compute_planner::{PlannedVectorWidth, VectorLoopPlan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorTilePlan {
    pub vectors_per_tile: u64,
    pub fixed_elements_per_tile: Option<u64>,
    pub min_bytes_per_tile: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeTileError {
    ZeroVectorBytes,
    ZeroVectorLanes,
    BudgetTooSmall {
        target_bytes: u64,
        min_vector_bytes: u64,
    },
    ArithmeticOverflow,
}

impl fmt::Display for ComputeTileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroVectorBytes => write!(f, "vector loop plan has a zero-byte vector width"),
            Self::ZeroVectorLanes => write!(f, "vector loop plan has zero lanes"),
            Self::BudgetTooSmall {
                target_bytes,
                min_vector_bytes,
            } => write!(
                f,
                "tile budget {target_bytes} bytes cannot hold one vector of at least {min_vector_bytes} bytes"
            ),
            Self::ArithmeticOverflow => write!(f, "vector tile size overflows u64"),
        }
    }
}

impl std::error::Error for ComputeTileError {}

/// Choose a vector tile that fits within `target_tile_bytes`.
///
/// The budget is rounded down to a whole number of vectors. Fixed-width plans
/// can expose an exact element count; scalable-width plans intentionally keep
/// the element count unknown because the concrete lane count is target/runtime
/// dependent.
pub fn plan_vector_tile(
    loop_plan: VectorLoopPlan,
    target_tile_bytes: u64,
) -> Result<VectorTilePlan, ComputeTileError> {
    if loop_plan.min_vector_bytes == 0 {
        return Err(ComputeTileError::ZeroVectorBytes);
    }
    if loop_plan.width.min_lanes() == 0 {
        return Err(ComputeTileError::ZeroVectorLanes);
    }
    if target_tile_bytes < loop_plan.min_vector_bytes {
        return Err(ComputeTileError::BudgetTooSmall {
            target_bytes: target_tile_bytes,
            min_vector_bytes: loop_plan.min_vector_bytes,
        });
    }

    let vectors_per_tile = target_tile_bytes / loop_plan.min_vector_bytes;
    let min_bytes_per_tile = vectors_per_tile
        .checked_mul(loop_plan.min_vector_bytes)
        .ok_or(ComputeTileError::ArithmeticOverflow)?;

    let fixed_elements_per_tile = match loop_plan.width {
        PlannedVectorWidth::Fixed(lanes) => Some(
            vectors_per_tile
                .checked_mul(u64::from(lanes))
                .ok_or(ComputeTileError::ArithmeticOverflow)?,
        ),
        PlannedVectorWidth::ScalableMin(_) => None,
    };

    Ok(VectorTilePlan {
        vectors_per_tile,
        fixed_elements_per_tile,
        min_bytes_per_tile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_plan() -> VectorLoopPlan {
        VectorLoopPlan {
            axis: 0,
            width: PlannedVectorWidth::Fixed(4),
            min_vector_bytes: 16,
            vector_iterations: Some(25),
            scalar_tail: Some(0),
            requires_runtime_tail: false,
            requires_runtime_bounds_check: false,
            start_is_vector_aligned: true,
        }
    }

    fn scalable_plan() -> VectorLoopPlan {
        VectorLoopPlan {
            axis: 0,
            width: PlannedVectorWidth::ScalableMin(4),
            min_vector_bytes: 16,
            vector_iterations: None,
            scalar_tail: None,
            requires_runtime_tail: true,
            requires_runtime_bounds_check: false,
            start_is_vector_aligned: false,
        }
    }

    #[test]
    fn fixed_width_tile_is_a_whole_number_of_vectors() {
        let tile = plan_vector_tile(fixed_plan(), 64).unwrap();

        assert_eq!(tile.vectors_per_tile, 4);
        assert_eq!(tile.fixed_elements_per_tile, Some(16));
        assert_eq!(tile.min_bytes_per_tile, 64);
    }

    #[test]
    fn fixed_width_tile_rounds_budget_down_to_vector_boundary() {
        let tile = plan_vector_tile(fixed_plan(), 70).unwrap();

        assert_eq!(tile.vectors_per_tile, 4);
        assert_eq!(tile.fixed_elements_per_tile, Some(16));
        assert_eq!(tile.min_bytes_per_tile, 64);
    }

    #[test]
    fn scalable_tile_keeps_element_count_target_dependent() {
        let tile = plan_vector_tile(scalable_plan(), 64).unwrap();

        assert_eq!(tile.vectors_per_tile, 4);
        assert_eq!(tile.fixed_elements_per_tile, None);
        assert_eq!(tile.min_bytes_per_tile, 64);
    }

    #[test]
    fn rejects_budget_smaller_than_one_vector() {
        assert_eq!(
            plan_vector_tile(fixed_plan(), 8),
            Err(ComputeTileError::BudgetTooSmall {
                target_bytes: 8,
                min_vector_bytes: 16,
            })
        );
    }

    #[test]
    fn rejects_public_plan_with_zero_vector_bytes() {
        let mut plan = fixed_plan();
        plan.min_vector_bytes = 0;

        assert_eq!(
            plan_vector_tile(plan, 64),
            Err(ComputeTileError::ZeroVectorBytes)
        );
    }

    #[test]
    fn rejects_public_plan_with_zero_vector_lanes() {
        let mut plan = fixed_plan();
        plan.width = PlannedVectorWidth::Fixed(0);

        assert_eq!(
            plan_vector_tile(plan, 64),
            Err(ComputeTileError::ZeroVectorLanes)
        );
    }
}
