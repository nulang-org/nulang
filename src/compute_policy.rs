//! Target capability and profitability policy for vector execution.
//!
//! `compute_planner` answers whether vector access is legal. This layer decides
//! whether a legal plan should be selected for a particular backend policy,
//! without baking hardware heuristics into the portable IR.

use std::fmt;

use crate::compute_planner::{PlannedVectorWidth, VectorLoopPlan};
use crate::compute_tiling::{plan_vector_tile, ComputeTileError, VectorTilePlan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorizationPolicy {
    min_vectors_per_loop: u64,
    target_tile_bytes: u64,
    supports_scalable_vectors: bool,
    supports_unaligned_access: bool,
}

impl VectorizationPolicy {
    pub fn new(
        min_vectors_per_loop: u64,
        target_tile_bytes: u64,
        supports_scalable_vectors: bool,
        supports_unaligned_access: bool,
    ) -> Result<Self, ComputePolicyError> {
        if min_vectors_per_loop == 0 {
            return Err(ComputePolicyError::ZeroMinimumVectors);
        }
        if target_tile_bytes == 0 {
            return Err(ComputePolicyError::ZeroTileBudget);
        }

        Ok(Self {
            min_vectors_per_loop,
            target_tile_bytes,
            supports_scalable_vectors,
            supports_unaligned_access,
        })
    }

    pub const fn min_vectors_per_loop(self) -> u64 {
        self.min_vectors_per_loop
    }

    pub const fn target_tile_bytes(self) -> u64 {
        self.target_tile_bytes
    }

    pub const fn supports_scalable_vectors(self) -> bool {
        self.supports_scalable_vectors
    }

    pub const fn supports_unaligned_access(self) -> bool {
        self.supports_unaligned_access
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfitabilityGuard {
    None,
    FixedElementsAtLeast(u64),
    TargetVectorIterationsAtLeast(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorExecutionPlan {
    pub loop_plan: VectorLoopPlan,
    pub tile: VectorTilePlan,
    pub profitability_guard: ProfitabilityGuard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorFallbackReason {
    ScalableVectorsUnsupported,
    UnalignedAccessUnsupported,
    StaticLoopTooSmall {
        vector_iterations: u64,
        minimum: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorizationDecision {
    Vectorize(VectorExecutionPlan),
    Scalar(VectorFallbackReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputePolicyError {
    ZeroMinimumVectors,
    ZeroTileBudget,
    ThresholdOverflow {
        min_vectors: u64,
        lanes: u16,
    },
    Tile(ComputeTileError),
}

impl fmt::Display for ComputePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroMinimumVectors => {
                write!(f, "vector policy requires at least one vector per loop")
            }
            Self::ZeroTileBudget => write!(f, "vector policy tile budget must be non-zero"),
            Self::ThresholdOverflow { min_vectors, lanes } => write!(
                f,
                "vector profitability threshold overflows: {min_vectors} vectors x {lanes} lanes"
            ),
            Self::Tile(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ComputePolicyError {}

impl From<ComputeTileError> for ComputePolicyError {
    fn from(error: ComputeTileError) -> Self {
        Self::Tile(error)
    }
}

/// Apply backend capabilities and caller-supplied profitability thresholds to
/// a legal vector loop plan.
///
/// Capability mismatches and statically unprofitable loops produce an explicit
/// scalar decision. Configuration/arithmetic failures remain errors so callers
/// can distinguish bad policy from an intentional scalar fallback.
pub fn assess_vectorization(
    loop_plan: VectorLoopPlan,
    policy: VectorizationPolicy,
) -> Result<VectorizationDecision, ComputePolicyError> {
    if loop_plan.width.is_scalable() && !policy.supports_scalable_vectors {
        return Ok(VectorizationDecision::Scalar(
            VectorFallbackReason::ScalableVectorsUnsupported,
        ));
    }

    if !loop_plan.start_is_vector_aligned && !policy.supports_unaligned_access {
        return Ok(VectorizationDecision::Scalar(
            VectorFallbackReason::UnalignedAccessUnsupported,
        ));
    }

    if let Some(vector_iterations) = loop_plan.vector_iterations {
        if vector_iterations < policy.min_vectors_per_loop {
            return Ok(VectorizationDecision::Scalar(
                VectorFallbackReason::StaticLoopTooSmall {
                    vector_iterations,
                    minimum: policy.min_vectors_per_loop,
                },
            ));
        }
    }

    let tile = plan_vector_tile(loop_plan, policy.target_tile_bytes)?;
    let profitability_guard = match loop_plan.width {
        PlannedVectorWidth::Fixed(lanes) if loop_plan.vector_iterations.is_none() => {
            let min_elements = policy
                .min_vectors_per_loop
                .checked_mul(u64::from(lanes))
                .ok_or(ComputePolicyError::ThresholdOverflow {
                    min_vectors: policy.min_vectors_per_loop,
                    lanes,
                })?;
            ProfitabilityGuard::FixedElementsAtLeast(min_elements)
        }
        PlannedVectorWidth::Fixed(_) => ProfitabilityGuard::None,
        PlannedVectorWidth::ScalableMin(_) => {
            ProfitabilityGuard::TargetVectorIterationsAtLeast(policy.min_vectors_per_loop)
        }
    };

    Ok(VectorizationDecision::Vectorize(VectorExecutionPlan {
        loop_plan,
        tile,
        profitability_guard,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute_planner::{PlannedVectorWidth, VectorLoopPlan};

    fn fixed_plan(vector_iterations: Option<u64>, aligned: bool) -> VectorLoopPlan {
        VectorLoopPlan {
            axis: 0,
            width: PlannedVectorWidth::Fixed(4),
            min_vector_bytes: 16,
            vector_iterations,
            scalar_tail: vector_iterations.map(|_| 2),
            requires_runtime_tail: vector_iterations.is_none(),
            requires_runtime_bounds_check: vector_iterations.is_none(),
            start_is_vector_aligned: aligned,
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

    fn policy() -> VectorizationPolicy {
        VectorizationPolicy::new(4, 64, false, true).unwrap()
    }

    #[test]
    fn rejects_zero_policy_thresholds() {
        assert_eq!(
            VectorizationPolicy::new(0, 64, false, true),
            Err(ComputePolicyError::ZeroMinimumVectors)
        );
        assert_eq!(
            VectorizationPolicy::new(4, 0, false, true),
            Err(ComputePolicyError::ZeroTileBudget)
        );
    }

    #[test]
    fn selects_profitable_static_fixed_width_loop_without_runtime_guard() {
        let decision = assess_vectorization(fixed_plan(Some(25), true), policy()).unwrap();
        let VectorizationDecision::Vectorize(plan) = decision else {
            panic!("expected vectorization");
        };

        assert_eq!(plan.tile.vectors_per_tile, 4);
        assert_eq!(plan.tile.fixed_elements_per_tile, Some(16));
        assert_eq!(plan.profitability_guard, ProfitabilityGuard::None);
    }

    #[test]
    fn rejects_static_loop_below_profitability_threshold() {
        assert_eq!(
            assess_vectorization(fixed_plan(Some(2), true), policy()).unwrap(),
            VectorizationDecision::Scalar(VectorFallbackReason::StaticLoopTooSmall {
                vector_iterations: 2,
                minimum: 4,
            })
        );
    }

    #[test]
    fn dynamic_fixed_width_loop_uses_element_threshold_guard() {
        let decision = assess_vectorization(fixed_plan(None, true), policy()).unwrap();
        let VectorizationDecision::Vectorize(plan) = decision else {
            panic!("expected guarded vectorization");
        };

        assert_eq!(
            plan.profitability_guard,
            ProfitabilityGuard::FixedElementsAtLeast(16)
        );
    }

    #[test]
    fn rejects_scalable_plan_when_target_does_not_support_it() {
        assert_eq!(
            assess_vectorization(scalable_plan(), policy()).unwrap(),
            VectorizationDecision::Scalar(VectorFallbackReason::ScalableVectorsUnsupported)
        );
    }

    #[test]
    fn scalable_target_uses_target_vector_iteration_guard() {
        let policy = VectorizationPolicy::new(4, 64, true, true).unwrap();
        let decision = assess_vectorization(scalable_plan(), policy).unwrap();
        let VectorizationDecision::Vectorize(plan) = decision else {
            panic!("expected scalable vectorization");
        };

        assert_eq!(plan.tile.vectors_per_tile, 4);
        assert_eq!(plan.tile.fixed_elements_per_tile, None);
        assert_eq!(
            plan.profitability_guard,
            ProfitabilityGuard::TargetVectorIterationsAtLeast(4)
        );
    }

    #[test]
    fn rejects_unaligned_plan_when_target_requires_alignment() {
        let policy = VectorizationPolicy::new(4, 64, false, false).unwrap();

        assert_eq!(
            assess_vectorization(fixed_plan(Some(25), false), policy).unwrap(),
            VectorizationDecision::Scalar(VectorFallbackReason::UnalignedAccessUnsupported)
        );
    }

    #[test]
    fn detects_fixed_threshold_element_overflow() {
        let policy = VectorizationPolicy::new(u64::MAX, 64, false, true).unwrap();

        assert_eq!(
            assess_vectorization(fixed_plan(None, true), policy),
            Err(ComputePolicyError::ThresholdOverflow {
                min_vectors: u64::MAX,
                lanes: 4,
            })
        );
    }
}
