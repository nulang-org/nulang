//! Backend-neutral iteration-space metadata for portable compute lowering.
//!
//! `compute_ir` describes operations and locality. This companion module
//! describes how many logical iterations a compute loop has without requiring
//! every extent to be known at compile time. Dynamic extents are symbolic IDs;
//! backend adapters bind those IDs to their own runtime values (for example a
//! bytecode `ArrLen` register) without leaking backend-specific identifiers
//! into the portable model.

use std::fmt;

use crate::compute_ir::LocalityScope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DynamicExtentId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoopExtent {
    Static(u64),
    Dynamic(DynamicExtentId),
}

impl LoopExtent {
    pub const fn is_dynamic(self) -> bool {
        matches!(self, Self::Dynamic(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IterationSpace {
    pub start: i64,
    pub extent: LoopExtent,
    pub step: i64,
    pub scope: LocalityScope,
}

impl IterationSpace {
    pub fn new(
        start: i64,
        extent: LoopExtent,
        step: i64,
        scope: LocalityScope,
    ) -> Result<Self, ComputeScheduleError> {
        if step == 0 {
            return Err(ComputeScheduleError::ZeroStep);
        }

        let space = Self {
            start,
            extent,
            step,
            scope,
        };
        // Validate a known range eagerly so backends can assume it is
        // representable in Nulang's signed loop-index domain.
        let _ = space.end_exclusive()?;
        Ok(space)
    }

    /// Return the exclusive end index when the extent is static. Dynamic
    /// iteration spaces return `Ok(None)` until a backend binds the extent.
    pub fn end_exclusive(self) -> Result<Option<i64>, ComputeScheduleError> {
        let LoopExtent::Static(count) = self.extent else {
            return Ok(None);
        };

        let end = self.start as i128 + self.step as i128 * count as i128;
        if end < i64::MIN as i128 || end > i64::MAX as i128 {
            return Err(ComputeScheduleError::StaticRangeOverflow {
                start: self.start,
                count,
                step: self.step,
            });
        }
        Ok(Some(end as i64))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeScheduleError {
    ZeroStep,
    StaticRangeOverflow { start: i64, count: u64, step: i64 },
}

impl fmt::Display for ComputeScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroStep => write!(f, "compute iteration step cannot be zero"),
            Self::StaticRangeOverflow { start, count, step } => write!(
                f,
                "compute iteration range overflows i64: start={start}, count={count}, step={step}"
            ),
        }
    }
}

impl std::error::Error for ComputeScheduleError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_iteration_space_computes_exclusive_end() {
        let space = IterationSpace::new(
            4,
            LoopExtent::Static(8),
            2,
            LocalityScope::Lane,
        )
        .unwrap();

        assert_eq!(space.end_exclusive().unwrap(), Some(20));
        assert!(!space.extent.is_dynamic());
    }

    #[test]
    fn descending_static_iteration_space_is_representable() {
        let space = IterationSpace::new(
            10,
            LoopExtent::Static(5),
            -2,
            LocalityScope::Lane,
        )
        .unwrap();

        assert_eq!(space.end_exclusive().unwrap(), Some(0));
    }

    #[test]
    fn dynamic_iteration_space_keeps_extent_symbolic() {
        let space = IterationSpace::new(
            0,
            LoopExtent::Dynamic(DynamicExtentId(7)),
            1,
            LocalityScope::Lane,
        )
        .unwrap();

        assert!(space.extent.is_dynamic());
        assert_eq!(space.end_exclusive().unwrap(), None);
    }

    #[test]
    fn zero_step_is_rejected() {
        assert_eq!(
            IterationSpace::new(
                0,
                LoopExtent::Static(4),
                0,
                LocalityScope::Lane,
            ),
            Err(ComputeScheduleError::ZeroStep)
        );
    }

    #[test]
    fn static_range_overflow_is_rejected() {
        assert_eq!(
            IterationSpace::new(
                i64::MAX,
                LoopExtent::Static(1),
                1,
                LocalityScope::Lane,
            ),
            Err(ComputeScheduleError::StaticRangeOverflow {
                start: i64::MAX,
                count: 1,
                step: 1,
            })
        );
    }
}
