//! Token-usage tracking and cost estimation for the AI Runtime.

use crate::request::ModelPricing;
use crate::response::TokenUsage;

/// Estimated cost in USD for a given usage and pricing rates.
pub fn estimated_cost(usage: &TokenUsage, pricing: &ModelPricing) -> f64 {
    let input_cost = (usage.prompt as f64) * pricing.input_cost_per_1k / 1000.0;
    let output_cost = (usage.completion as f64) * pricing.output_cost_per_1k / 1000.0;
    input_cost + output_cost
}

/// Accumulated token usage and cost summary for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct UsageSummary {
    pub prompt: u32,
    pub completion: u32,
    pub total: u32,
    pub cost: f64,
}

impl UsageSummary {
    /// Create an empty summary.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Add a single request's usage to the summary, computing cost when pricing
    /// is available.
    pub fn accumulate(&mut self, usage: &TokenUsage, pricing: Option<&ModelPricing>) {
        self.prompt = self.prompt.saturating_add(usage.prompt);
        self.completion = self.completion.saturating_add(usage.completion);
        self.total = self.total.saturating_add(usage.total);
        if let Some(p) = pricing {
            self.cost += estimated_cost(usage, p);
        }
    }
}

// -- Token budget enforcement -------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};

/// A token-spending cap for LLM calls, suitable for use across threads
/// (e.g. checked on the scheduler thread, updated after async completions).
#[derive(Debug)]
pub struct TokenBudget {
    /// Hard limit on total tokens that may be consumed.
    limit: u64,
    /// Tokens consumed so far.  Saturates at `limit`.
    used: AtomicU64,
}

impl TokenBudget {
    /// Create a budget with the given token limit.
    pub fn new(limit: u64) -> Self {
        TokenBudget {
            limit,
            used: AtomicU64::new(0),
        }
    }

    /// Tokens remaining before the budget is exhausted.
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used.load(Ordering::Relaxed))
    }

    /// True when zero tokens remain.
    pub fn is_exhausted(&self) -> bool {
        self.remaining() == 0
    }

    /// Charge `tokens` against the budget.  Charges beyond the limit are
    /// clamped (the budget never goes negative).
    pub fn charge(&self, tokens: u64) {
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let new = current.saturating_add(tokens).min(self.limit);
            match self.used.compare_exchange_weak(
                current,
                new,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Return the configured limit.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Total tokens consumed so far.
    pub fn consumed(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn test_token_budget_new_has_full_remaining() {
        let budget = TokenBudget::new(1000);
        assert_eq!(budget.remaining(), 1000);
        assert!(!budget.is_exhausted());
    }

    #[test]
    fn test_token_budget_charge_reduces_remaining() {
        let budget = TokenBudget::new(1000);
        budget.charge(300);
        assert_eq!(budget.remaining(), 700);
        assert_eq!(budget.consumed(), 300);
        assert!(!budget.is_exhausted());
    }

    #[test]
    fn test_token_budget_exhausted_after_full_charge() {
        let budget = TokenBudget::new(500);
        budget.charge(500);
        assert_eq!(budget.remaining(), 0);
        assert!(budget.is_exhausted());
    }

    #[test]
    fn test_token_budget_charge_clamps_at_limit() {
        let budget = TokenBudget::new(100);
        budget.charge(200); // overcharge
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.consumed(), 100); // clamped
        assert!(budget.is_exhausted());
    }

    #[test]
    fn test_token_budget_large_charge_never_wraps_or_panics() {
        let budget = TokenBudget::new(u64::MAX);
        budget.charge(u64::MAX - 1);
        budget.charge(100);
        assert_eq!(budget.consumed(), u64::MAX);
        assert_eq!(budget.remaining(), 0);
        assert!(budget.is_exhausted());
    }

    #[test]
    fn test_token_budget_large_charge_respects_smaller_limit() {
        let budget = TokenBudget::new(100);
        budget.charge(99);
        budget.charge(u64::MAX);
        assert_eq!(budget.consumed(), 100);
        assert!(budget.is_exhausted());
    }

    #[test]
    fn test_token_budget_concurrent_charges() {
        use std::sync::Arc;
        use std::thread;

        let budget = Arc::new(TokenBudget::new(1000));
        let mut handles = vec![];
        for _ in 0..10 {
            let b = budget.clone();
            handles.push(thread::spawn(move || {
                b.charge(100);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(budget.consumed(), 1000);
        assert!(budget.is_exhausted());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimated_cost_known_pricing() {
        let usage = TokenUsage::new(1000, 500);
        let pricing = ModelPricing {
            input_cost_per_1k: 0.03,
            output_cost_per_1k: 0.06,
        };
        // 1000 input tokens @ $0.03/1k = $0.03
        // 500 output tokens @ $0.06/1k = $0.03
        assert!((estimated_cost(&usage, &pricing) - 0.06).abs() < 1e-9);
    }

    #[test]
    fn test_usage_summary_accumulate() {
        let mut summary = UsageSummary::empty();
        let pricing = ModelPricing {
            input_cost_per_1k: 0.01,
            output_cost_per_1k: 0.02,
        };

        summary.accumulate(&TokenUsage::new(1000, 500), Some(&pricing));
        assert_eq!(summary.prompt, 1000);
        assert_eq!(summary.completion, 500);
        assert_eq!(summary.total, 1500);
        assert!((summary.cost - 0.02).abs() < 1e-9);

        summary.accumulate(&TokenUsage::new(2000, 1000), Some(&pricing));
        assert_eq!(summary.prompt, 3000);
        assert_eq!(summary.completion, 1500);
        assert_eq!(summary.total, 4500);
        assert!((summary.cost - 0.06).abs() < 1e-9);
    }
}
