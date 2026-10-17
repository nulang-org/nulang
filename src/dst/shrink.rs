//! Bounded, deterministic delta-debugging for DST failure traces.
//!
//! This helper removes operations only; it never reorders or invents them.
//! The predicate must replay every candidate on a fresh runtime/simulator,
//! returning true only for the same observed failure signature.

/// Whether the input failed, and whether minimization completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinimizationStatus {
    /// The original trace did not reproduce the failure.
    NotFailing,
    /// Every single-operation deletion was checked and did not preserve failure.
    OneMinimal,
    /// The replay budget was exhausted before one-minimality was established.
    BudgetExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinimizedTrace<T> {
    pub trace: Vec<T>,
    /// Includes the mandatory baseline replay when the budget is nonzero.
    pub replays: usize,
    pub status: MinimizationStatus,
}

/// Reduce an ordered operation trace while preserving an externally checked failure.
///
/// `max_replays` strictly caps predicate calls; a zero budget does not
/// certify the input as failing. The returned trace remains a subsequence
/// of the original, preserving the order of retained operations.
pub fn minimize_failing_trace<T: Clone, F: FnMut(&[T]) -> bool>(
    trace: &[T],
    max_replays: usize,
    mut still_fails: F,
) -> MinimizedTrace<T> {
    // A reported reproducer must first be independently verified. Exhaustion
    // is a separate outcome: neither a zero budget nor a nonreproducible
    // baseline may be mislabeled as a minimized failure.
    if max_replays == 0 {
        return MinimizedTrace {
            trace: trace.to_vec(),
            replays: 0,
            status: MinimizationStatus::BudgetExhausted,
        };
    }

    let mut replays = 1;
    if !still_fails(trace) {
        return MinimizedTrace {
            trace: trace.to_vec(),
            replays,
            status: MinimizationStatus::NotFailing,
        };
    }

    let mut current = trace.to_vec();
    let mut granularity = 2usize;

    // Standard ddmin complement reduction. Each candidate is a contiguous
    // deletion from the current sequence, so the final trace is always an
    // ordered subsequence. Deterministic chunk order and no RNG make the
    // entire replay sequence stable across runs.
    while !current.is_empty() {
        let chunk_size =
            current.len() / granularity + usize::from(current.len() % granularity != 0);
        let mut start = 0;
        let mut reduced = false;

        while start < current.len() {
            if replays >= max_replays {
                return MinimizedTrace {
                    trace: current,
                    replays,
                    status: MinimizationStatus::BudgetExhausted,
                };
            }

            let end = start.saturating_add(chunk_size).min(current.len());
            let candidate: Vec<T> = current[..start]
                .iter()
                .chain(current[end..].iter())
                .cloned()
                .collect();

            replays += 1;
            if still_fails(&candidate) {
                current = candidate;
                granularity = granularity.saturating_sub(1).max(2);
                reduced = true;
                break;
            }
            start = end;
        }

        if !reduced {
            if granularity >= current.len() {
                // Every single-element deletion has been evaluated (or the
                // trace is empty), establishing 1-minimality for this oracle.
                break;
            }
            granularity = granularity.saturating_mul(2).min(current.len());
        }
    }

    MinimizedTrace {
        trace: current,
        replays,
        status: MinimizationStatus::OneMinimal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        Noise,
        Send,
        Crash,
        Restart,
    }

    fn ordered_failure(events: &[Event]) -> bool {
        let required = [Event::Send, Event::Crash, Event::Restart];
        let mut next = 0;
        for event in events {
            if *event == required[next] {
                next += 1;
                if next == required.len() {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn shrinks_to_ordered_failure_without_inventing_events() {
        let events = [
            Event::Noise,
            Event::Send,
            Event::Noise,
            Event::Crash,
            Event::Noise,
            Event::Restart,
            Event::Noise,
        ];
        let minimized = minimize_failing_trace(&events, 100, ordered_failure);
        assert_eq!(minimized.status, MinimizationStatus::OneMinimal);
        assert_eq!(
            minimized.trace,
            vec![Event::Send, Event::Crash, Event::Restart]
        );
        assert!(minimized.replays <= 100);
    }

    #[test]
    fn a_nonfailing_baseline_is_not_reported_as_reproduced() {
        let events = [Event::Noise, Event::Send];
        let minimized = minimize_failing_trace(&events, 100, ordered_failure);
        assert_eq!(minimized.status, MinimizationStatus::NotFailing);
        assert_eq!(minimized.trace, events);
        assert_eq!(minimized.replays, 1);
    }

    #[test]
    fn does_not_call_the_predicate_without_a_replay_budget() {
        let events = [Event::Crash, Event::Restart];
        let minimized = minimize_failing_trace(&events, 0, |_| panic!("must not replay"));
        assert_eq!(minimized.status, MinimizationStatus::BudgetExhausted);
        assert_eq!(minimized.replays, 0);
        assert_eq!(minimized.trace, events);
    }

    #[test]
    fn budget_exhaustion_does_not_claim_minimality() {
        let events = [Event::Noise, Event::Send, Event::Crash, Event::Restart];
        let minimized = minimize_failing_trace(&events, 1, ordered_failure);
        assert_eq!(minimized.status, MinimizationStatus::BudgetExhausted);
        assert_eq!(minimized.replays, 1);
        assert_eq!(minimized.trace, events);
    }

    #[test]
    fn an_empty_failing_candidate_reduces_to_zero_operations() {
        let events = [Event::Noise, Event::Crash, Event::Restart];
        let minimized = minimize_failing_trace(&events, 100, |_| true);
        assert_eq!(minimized.trace, Vec::<Event>::new());
        assert_eq!(minimized.status, MinimizationStatus::OneMinimal);
    }

    #[test]
    fn same_input_and_predicate_have_identical_replay_sequence() {
        let events = [Event::Noise, Event::Send, Event::Noise, Event::Crash, Event::Restart];
        let mut first_checked = Vec::new();
        let first = minimize_failing_trace(&events, 100, |candidate| {
            first_checked.push(candidate.to_vec());
            ordered_failure(candidate)
        });
        let mut second_checked = Vec::new();
        let second = minimize_failing_trace(&events, 100, |candidate| {
            second_checked.push(candidate.to_vec());
            ordered_failure(candidate)
        });
        assert_eq!(first, second);
        assert_eq!(first_checked, second_checked);
    }

    #[test]
    fn single_event_failure_is_one_minimal() {
        let events = [Event::Noise, Event::Crash, Event::Noise];
        let minimized = minimize_failing_trace(&events, 100, |candidate| {
            candidate.contains(&Event::Crash)
        });
        assert_eq!(minimized.trace, vec![Event::Crash]);
        assert_eq!(minimized.status, MinimizationStatus::OneMinimal);
    }
}
