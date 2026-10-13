//! RED coverage for compiler-owned structured-concurrency effect scheduling.
//!
//! Replay semantics and parallel-scheduling semantics are separate axes. Host
//! operations reuse the compiler-owned HostReplayClass registry; unknown effects
//! fail closed until the compiler has an explicit execution contract.

use nulang::ast::{BinOp, Expr, Literal};
use nulang::effect_semantics::{classify_effect_operation, ParallelEffectConstraint};
use nulang::host_effect_abi::HostReplayClass;
use nulang::parallel_analysis::summarize_branch;
use nulang::types::Span;

fn sp() -> Span {
    Span::default()
}

fn int(value: i64) -> Expr {
    Expr::Literal(Literal::Int(value), sp())
}

#[test]
fn host_replay_contract_drives_effect_scheduling_metadata() {
    let semantics = classify_effect_operation("Storage", "read");
    assert_eq!(semantics.replay, Some(HostReplayClass::JournalResult));
    assert_eq!(semantics.parallel, ParallelEffectConstraint::SequentialOnly);
    assert!(semantics.externally_observable);
}

#[test]
fn unknown_custom_effects_fail_closed_for_parallel_overlap() {
    let semantics = classify_effect_operation("Payments", "charge");
    assert_eq!(semantics.replay, None);
    assert_eq!(semantics.parallel, ParallelEffectConstraint::SequentialOnly);
}

#[test]
fn pure_branch_has_no_effect_imposed_scheduling_constraint() {
    let branch = Expr::Binary {
        op: BinOp::Add,
        left: Box::new(int(1)),
        right: Box::new(int(2)),
        span: sp(),
    };
    let summary = summarize_branch(0, &branch);

    assert_eq!(
        summary.effect_constraint,
        ParallelEffectConstraint::Unconstrained
    );
    assert!(!summary.may_suspend);
}

#[test]
fn suspension_points_are_visible_to_future_task_scheduler() {
    let branch = Expr::Perform {
        effect: "Timer".to_string(),
        op: "sleep".to_string(),
        args: vec![int(10)],
        span: sp(),
    };
    let summary = summarize_branch(0, &branch);

    assert_eq!(
        summary.effect_constraint,
        ParallelEffectConstraint::SequentialOnly
    );
    assert!(summary.may_suspend);
}
