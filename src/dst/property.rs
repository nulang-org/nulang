//! QuickCheck-inspired deterministic property checks for operation traces.
//!
//! This small harness builds on Nulang's existing SplitMix64 generator and
//! budgeted, deletion-only DST shrinker. It does not mutate or reuse
//! simulation state: property callbacks must reconstruct a fresh system on
//! every invocation, including each shrink replay.
//!
//! Failure reporting distinguishes a reproduced, one-minimal counterexample
//! from nonreproducible failures and replay-budget exhaustion. OneMinimal
//! only means no single-operation deletion preserves the same failure; it
//! does not mean the smallest possible trace or minimal event payloads.

use super::shrink::{minimize_failing_trace, MinimizedTrace};
use super::DeterministicRng;

/// One deterministic sequence of property cases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TracePropertyConfig {
    /// Root seed; printed in a failing test to reproduce the complete sweep.
    pub seed: u64,
    /// Maximum generated cases. Zero succeeds without generating a case.
    pub cases: usize,
    /// Total shrink predicate calls, including baseline reproduction.
    /// Zero disables shrink reproduction rather than claiming minimality.
    pub max_shrink_replays: usize,
}

/// A generated counterexample with its exact reproduction metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TracePropertyFailure<T> {
    pub seed: u64,
    /// Zero-based index in the deterministic sweep.
    pub case_index: usize,
    /// Re-run the generator with this seed to reproduce the original trace.
    pub case_seed: u64,
    pub original: Vec<T>,
    /// Status distinguishes genuine minimization from incomplete evidence.
    pub minimized: MinimizedTrace<T>,
}

/// Generate deterministic operation traces, check an invariant, and shrink the
/// first failing trace by removing events without changing their order.
///
/// Each case has an independently derived seed. Generator RNG consumption in
/// one case therefore does not perturb later case seeds.
///
/// The predicate returns true when the invariant holds. For runtime/DST
/// properties it must create a fresh runtime and replay the supplied trace,
/// including during shrinking; do not reuse state between invocations.
///
/// This is a Rust-side testing utility, not a new Nulang-language builtin.
/// It does not catch panics in the property; panics retain ordinary Rust test
/// behavior. Use the returned root/case seeds in a failing test's assertion
/// message so CI provides a reproducible counterexample.
pub fn check_trace_property<T, G, P>(
    config: TracePropertyConfig,
    mut generate: G,
    mut property_holds: P,
) -> Result<usize, TracePropertyFailure<T>>
where
    T: Clone,
    G: FnMut(&mut DeterministicRng) -> Vec<T>,
    P: FnMut(&[T]) -> bool,
{
    let mut seeds = DeterministicRng::new(config.seed);
    for case_index in 0..config.cases {
        let case_seed = seeds.next();
        let mut rng = DeterministicRng::new(case_seed);
        let original = generate(&mut rng);
        if !property_holds(&original) {
            // Recheck the baseline. In particular, a stateful/flaky predicate
            // must not be presented as an independently reproduced failure.
            let minimized =
                minimize_failing_trace(&original, config.max_shrink_replays, |candidate| {
                    !property_holds(candidate)
                });
            return Err(TracePropertyFailure {
                seed: config.seed,
                case_index,
                case_seed,
                original,
                minimized,
            });
        }
    }
    Ok(config.cases)
}
