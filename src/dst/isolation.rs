//! PostgreSQL-inspired isolation-schedule enumeration.
//!
//! Each lane is an ordered list of logical operations (like a database
//! session). The enumerator explores *all* interleavings preserving the
//! order within each lane. It never creates threads or makes time-based
//! scheduling decisions. Callers execute each schedule against a fresh
//! real runtime or persistence backend and assert the resulting state.
//!
//! This deliberately does not model transaction serializability itself:
//! the assertions belong to each backend-specific scenario.

/// Maximum total operations in a single isolation schedule. Keeps recursive
/// enumeration stack usage bounded even with a large result budget.
pub const MAX_ISOLATION_STEPS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsolationStep {
    /// Zero-based lane/session index.
    pub lane: usize,
    /// Zero-based operation index within that lane.
    pub operation: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationSchedule {
    pub steps: Vec<IsolationStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationError {
    /// A zero limit cannot distinguish exhaustive exploration from truncation.
    EmptyBudget,
    /// Extremely long schedules are not safe to enumerate recursively.
    TooManySteps,
    /// More interleavings exist than the requested result budget.
    /// Never silently treat a truncated search as exhaustive.
    BudgetExceeded { limit: usize },
}

/// Exhaustively enumerate legal interleavings of ordered session operations.
///
/// For two sessions of two operations each, this returns six schedules.
/// The result is deterministic, ordered by ascending lane index at each step.
/// `limit` is a hard bound on the number of schedules: exceeding it is an
/// error rather than an incomplete success.
pub fn enumerate_interleavings(
    lane_lengths: &[usize],
    limit: usize,
) -> Result<Vec<IsolationSchedule>, IsolationError> {
    if limit == 0 {
        return Err(IsolationError::EmptyBudget);
    }
    let total = lane_lengths.iter().try_fold(0usize, |sum, len| {
        sum.checked_add(*len).ok_or(IsolationError::TooManySteps)
    })?;
    if total > MAX_ISOLATION_STEPS || lane_lengths.len() > MAX_ISOLATION_STEPS {
        return Err(IsolationError::TooManySteps);
    }

    fn visit(
        lane_lengths: &[usize],
        positions: &mut [usize],
        current: &mut Vec<IsolationStep>,
        total: usize,
        limit: usize,
        results: &mut Vec<IsolationSchedule>,
    ) -> Result<(), IsolationError> {
        if current.len() == total {
            if results.len() >= limit {
                return Err(IsolationError::BudgetExceeded { limit });
            }
            results.push(IsolationSchedule {
                steps: current.clone(),
            });
            return Ok(());
        }

        for lane in 0..lane_lengths.len() {
            if positions[lane] == lane_lengths[lane] {
                continue;
            }
            let operation = positions[lane];
            positions[lane] += 1;
            current.push(IsolationStep { lane, operation });

            let outcome = visit(lane_lengths, positions, current, total, limit, results);
            current.pop();
            positions[lane] -= 1;
            outcome?;
        }
        Ok(())
    }

    let mut results = Vec::new();
    let mut positions = vec![0; lane_lengths.len()];
    visit(
        lane_lengths,
        &mut positions,
        &mut Vec::with_capacity(total),
        total,
        limit,
        &mut results,
    )?;
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_program_has_exactly_one_empty_schedule() {
        assert_eq!(
            enumerate_interleavings(&[], 1).unwrap(),
            vec![IsolationSchedule { steps: vec![] }]
        );
    }

    #[test]
    fn ordering_is_stable_for_two_lanes() {
        let schedules = enumerate_interleavings(&[1, 1], 2).unwrap();
        assert_eq!(
            schedules,
            vec![
                IsolationSchedule {
                    steps: vec![
                        IsolationStep { lane: 0, operation: 0 },
                        IsolationStep { lane: 1, operation: 0 },
                    ],
                },
                IsolationSchedule {
                    steps: vec![
                        IsolationStep { lane: 1, operation: 0 },
                        IsolationStep { lane: 0, operation: 0 },
                    ],
                },
            ]
        );
    }

    #[test]
    fn does_not_hide_combinatorial_explosion() {
        assert_eq!(
            enumerate_interleavings(&[2, 2], 5),
            Err(IsolationError::BudgetExceeded { limit: 5 })
        );
        assert_eq!(
            enumerate_interleavings(&[MAX_ISOLATION_STEPS + 1], 1),
            Err(IsolationError::TooManySteps)
        );
        assert_eq!(
            enumerate_interleavings(&[usize::MAX, 1], 1),
            Err(IsolationError::TooManySteps)
        );
    }
}
