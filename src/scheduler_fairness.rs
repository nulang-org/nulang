//! Deterministic weighted-fair arbitration for actor priority bands.
//!
//! The current scheduler probes `High -> Normal -> Low` on every dequeue. That
//! gives excellent High latency but permits unbounded starvation when High work
//! is continuously available. This module defines the policy primitive needed
//! to preserve priority preference while giving lower bands bounded service
//! opportunities.
//!
//! It is intentionally separate from the Chase-Lev queue implementation so the
//! policy can be tested independently before changing scheduler hot paths.

use crate::runtime::ActorPriority;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Relative service opportunities for actor priority bands.
///
/// A weight of zero means that band is never the *preferred* band, but it still
/// remains in fallback order so work can run when preferred bands are empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityWeights {
    pub high: u16,
    pub normal: u16,
    pub low: u16,
}

impl PriorityWeights {
    pub const fn new(high: u16, normal: u16, low: u16) -> Self {
        Self { high, normal, low }
    }

    pub const fn total(self) -> u32 {
        self.high as u32 + self.normal as u32 + self.low as u32
    }
}

impl Default for PriorityWeights {
    fn default() -> Self {
        // High remains dominant under saturation while Normal and Low receive
        // deterministic opportunities often enough to prevent starvation.
        Self::new(8, 4, 1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidPriorityWeights;

impl fmt::Display for InvalidPriorityWeights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at least one actor priority weight must be non-zero")
    }
}

impl std::error::Error for InvalidPriorityWeights {}

/// One scheduling turn: the first band is preferred and the remaining bands
/// are fallbacks if the preferred queue has no runnable work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriorityTurn {
    pub order: [ActorPriority; 3],
}

impl PriorityTurn {
    pub const fn preferred(self) -> ActorPriority {
        self.order[0]
    }
}

/// Deterministic weighted service wheel for scheduler dequeue attempts.
///
/// Under sustained non-empty queues and the default 8:4:1 weights, each 13
/// turns prefer High 8 times, Normal 4 times, and Low once. This changes the
/// starvation bound from unbounded to at most one complete service cycle for a
/// continuously runnable lower-priority band, while High still receives most
/// service opportunities.
#[derive(Debug, Clone)]
pub struct FairPriorityPolicy {
    weights: PriorityWeights,
    cursor: u32,
}

impl FairPriorityPolicy {
    pub fn new(weights: PriorityWeights) -> Result<Self, InvalidPriorityWeights> {
        if weights.total() == 0 {
            return Err(InvalidPriorityWeights);
        }
        Ok(Self { weights, cursor: 0 })
    }

    pub fn weights(&self) -> PriorityWeights {
        self.weights
    }

    pub fn cycle_len(&self) -> u32 {
        self.weights.total()
    }

    pub fn reset(&mut self) {
        self.cursor = 0;
    }

    /// Produce the priority order for one dequeue attempt and advance the
    /// deterministic service wheel.
    pub fn next_turn(&mut self) -> PriorityTurn {
        let total = self.weights.total();
        debug_assert!(total > 0);

        let slot = self.cursor;
        self.cursor = (self.cursor + 1) % total;

        let preferred = if slot < self.weights.high as u32 {
            ActorPriority::High
        } else if slot < self.weights.high as u32 + self.weights.normal as u32 {
            ActorPriority::Normal
        } else {
            ActorPriority::Low
        };

        PriorityTurn {
            order: fallback_order(preferred),
        }
    }
}

impl Default for FairPriorityPolicy {
    fn default() -> Self {
        // Default weights are statically known to contain non-zero service.
        Self::new(PriorityWeights::default()).expect("default priority weights are valid")
    }
}

/// Preserve the current High-first preference as a selectable compatibility
/// policy. Normal/Low remain fallbacks when High is empty, exactly matching
/// the existing scheduler's priority semantics.
pub fn strict_priority_policy() -> FairPriorityPolicy {
    FairPriorityPolicy::new(PriorityWeights::new(1, 0, 0))
        .expect("strict priority policy has one non-zero weight")
}

const fn fallback_order(preferred: ActorPriority) -> [ActorPriority; 3] {
    match preferred {
        ActorPriority::High => [
            ActorPriority::High,
            ActorPriority::Normal,
            ActorPriority::Low,
        ],
        ActorPriority::Normal => [
            ActorPriority::Normal,
            ActorPriority::High,
            ActorPriority::Low,
        ],
        ActorPriority::Low => [
            ActorPriority::Low,
            ActorPriority::High,
            ActorPriority::Normal,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cycle_has_exact_weighted_preference_counts() {
        let mut policy = FairPriorityPolicy::default();
        let mut high = 0;
        let mut normal = 0;
        let mut low = 0;

        for _ in 0..policy.cycle_len() {
            match policy.next_turn().preferred() {
                ActorPriority::High => high += 1,
                ActorPriority::Normal => normal += 1,
                ActorPriority::Low => low += 1,
            }
        }

        assert_eq!((high, normal, low), (8, 4, 1));
    }

    #[test]
    fn low_priority_gets_a_bounded_service_opportunity() {
        let mut policy = FairPriorityPolicy::default();
        let mut saw_low_at = None;

        for turn in 0..policy.cycle_len() {
            if policy.next_turn().preferred() == ActorPriority::Low {
                saw_low_at = Some(turn);
                break;
            }
        }

        assert!(saw_low_at.is_some());
        assert!(saw_low_at.unwrap() < policy.cycle_len());
    }

    #[test]
    fn every_turn_contains_each_priority_exactly_once() {
        let mut policy = FairPriorityPolicy::default();
        for _ in 0..policy.cycle_len() * 2 {
            let order = policy.next_turn().order;
            assert_ne!(order[0], order[1]);
            assert_ne!(order[0], order[2]);
            assert_ne!(order[1], order[2]);
        }
    }

    #[test]
    fn zero_weight_band_is_fallback_but_never_preferred() {
        let mut policy = FairPriorityPolicy::new(PriorityWeights::new(3, 1, 0)).unwrap();
        for _ in 0..32 {
            assert_ne!(policy.next_turn().preferred(), ActorPriority::Low);
        }
    }

    #[test]
    fn all_zero_weights_are_rejected() {
        assert!(FairPriorityPolicy::new(PriorityWeights::new(0, 0, 0)).is_err());
    }

    #[test]
    fn strict_policy_matches_current_high_first_preference() {
        let mut policy = strict_priority_policy();
        for _ in 0..8 {
            assert_eq!(
                policy.next_turn().order,
                [
                    ActorPriority::High,
                    ActorPriority::Normal,
                    ActorPriority::Low,
                ]
            );
        }
    }

    #[test]
    fn reset_restarts_the_service_cycle() {
        let mut policy = FairPriorityPolicy::new(PriorityWeights::new(1, 1, 1)).unwrap();
        assert_eq!(policy.next_turn().preferred(), ActorPriority::High);
        assert_eq!(policy.next_turn().preferred(), ActorPriority::Normal);
        policy.reset();
        assert_eq!(policy.next_turn().preferred(), ActorPriority::High);
    }
}
