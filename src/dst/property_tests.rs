//! Contract-first tests for seeded trace properties and bounded shrinking.
//! The generator returns operations; the predicate must replay each candidate
//! against a fresh system when testing mutable runtimes.

use super::property::{check_trace_property, TracePropertyConfig};
use super::shrink::MinimizationStatus;
use super::DeterministicRng;
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Noise,
    Send,
    Crash,
    Restart,
}

fn has_ordered_failure(operations: &[Op]) -> bool {
    let required = [Op::Send, Op::Crash, Op::Restart];
    let mut next = 0;
    for operation in operations {
        if *operation == required[next] {
            next += 1;
            if next == required.len() {
                return true;
            }
        }
    }
    false
}

#[test]
fn identical_seeds_produce_identical_generated_cases() {
    let config = TracePropertyConfig {
        seed: 0x5eed,
        cases: 24,
        max_shrink_replays: 100,
    };
    let mut first_cases = Vec::new();
    let first = check_trace_property(
        config,
        |rng| vec![rng.next(), rng.next(), rng.next()],
        |trace| {
            first_cases.push(trace.to_vec());
            true
        },
    );
    let mut second_cases = Vec::new();
    let second = check_trace_property(
        config,
        |rng| vec![rng.next(), rng.next(), rng.next()],
        |trace| {
            second_cases.push(trace.to_vec());
            true
        },
    );

    assert_eq!(first, Ok(24));
    assert_eq!(second, Ok(24));
    assert_eq!(first_cases, second_cases);
    assert_eq!(first_cases.len(), 24);
}

#[test]
fn failing_generated_trace_is_shrunk_to_ordered_counterexample() {
    let failure = check_trace_property(
        TracePropertyConfig {
            seed: 19,
            cases: 10,
            max_shrink_replays: 100,
        },
        |_| {
            vec![
                Op::Noise,
                Op::Send,
                Op::Noise,
                Op::Crash,
                Op::Noise,
                Op::Restart,
                Op::Noise,
            ]
        },
        |trace| !has_ordered_failure(trace),
    )
    .expect_err("generated failure should be reported");

    assert_eq!(failure.case_index, 0);
    assert_eq!(failure.seed, 19);
    assert_eq!(failure.minimized.status, MinimizationStatus::OneMinimal);
    assert_eq!(
        failure.minimized.trace,
        vec![Op::Send, Op::Crash, Op::Restart]
    );
    assert!(has_ordered_failure(&failure.original));
}

#[test]
fn case_seed_replays_original_generation_independently() {
    fn generate(rng: &mut DeterministicRng) -> Vec<u64> {
        vec![rng.next(), rng.next() % 31]
    }

    let failure = check_trace_property(
        TracePropertyConfig {
            seed: 41,
            cases: 10,
            max_shrink_replays: 1,
        },
        generate,
        |_| false,
    )
    .expect_err("first case fails");

    let mut replay_rng = DeterministicRng::new(failure.case_seed);
    assert_eq!(failure.original, generate(&mut replay_rng));
    assert_eq!(
        failure.minimized.status,
        MinimizationStatus::BudgetExhausted
    );
}

#[test]
fn nonreproducible_failure_is_never_marked_minimal() {
    let calls = Cell::new(0);
    let failure = check_trace_property(
        TracePropertyConfig {
            seed: 4,
            cases: 1,
            max_shrink_replays: 30,
        },
        |_| vec![Op::Noise, Op::Send],
        |_| {
            let current = calls.get();
            calls.set(current + 1);
            current != 0
        },
    )
    .expect_err("first invocation fails");

    assert_eq!(calls.get(), 2);
    assert_eq!(failure.minimized.status, MinimizationStatus::NotFailing);
    assert_eq!(failure.minimized.trace, failure.original);
}

#[test]
fn zero_shrink_budget_never_claims_minimality() {
    let failure = check_trace_property(
        TracePropertyConfig {
            seed: 4,
            cases: 1,
            max_shrink_replays: 0,
        },
        |_| vec![Op::Send, Op::Crash],
        |_| false,
    )
    .expect_err("first case fails");

    assert_eq!(
        failure.minimized.status,
        MinimizationStatus::BudgetExhausted
    );
    assert_eq!(failure.minimized.replays, 0);
    assert_eq!(failure.minimized.trace, failure.original);
}

#[test]
fn zero_cases_skip_generation_and_checking() {
    let result = check_trace_property(
        TracePropertyConfig {
            seed: 11,
            cases: 0,
            max_shrink_replays: 30,
        },
        |_| -> Vec<u64> { panic!("generator should not be called") },
        |_| panic!("property should not be called"),
    );
    assert_eq!(result, Ok(0));
}
