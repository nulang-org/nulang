//! Constraint-solving contract for open effect-row variables.
//!
//! This exercises a pure solver independently of inference integration so
//! caller-side effect inclusion remains fail-closed until bindings propagate
//! through the entire typed compiler pipeline.

use nulang::effect_row_solver::{EffectRowSolveError, EffectRowSolver};
use nulang::types::{Capability, Effect, EffectRow, Region, Type};

fn open(effects: &[Effect], tail: Region) -> EffectRow {
    EffectRow::Open(effects.to_vec(), tail)
}

fn closed(effects: &[Effect]) -> EffectRow {
    EffectRow::Closed(effects.to_vec())
}

#[test]
fn open_row_tail_can_be_closed_by_a_matching_concrete_row() {
    let tail = Region::fresh();
    let mut solver = EffectRowSolver::new();

    solver
        .unify(
            &open(&[Effect::IO], tail),
            &closed(&[Effect::FS, Effect::IO]),
        )
        .unwrap();

    assert_eq!(
        solver.resolve(&open(&[Effect::IO], tail)).unwrap(),
        closed(&[Effect::FS, Effect::IO])
    );
    assert_eq!(
        solver.resolve(&open(&[], tail)).unwrap(),
        closed(&[Effect::FS])
    );
}

#[test]
fn chained_constraints_preserve_a_residual_open_tail() {
    let first = Region::fresh();
    let second = Region::fresh();
    let mut solver = EffectRowSolver::new();

    solver.bind(first, open(&[Effect::FS], second)).unwrap();
    assert_eq!(
        solver.resolve(&open(&[Effect::IO], first)).unwrap(),
        open(&[Effect::FS, Effect::IO], second)
    );

    solver.bind(second, closed(&[Effect::Net])).unwrap();
    assert_eq!(
        solver.resolve(&open(&[Effect::IO], first)).unwrap(),
        closed(&[Effect::FS, Effect::IO, Effect::Net])
    );
}

#[test]
fn two_unrelated_tails_unify_without_losing_either_effect() {
    let left_tail = Region::fresh();
    let right_tail = Region::fresh();
    let left = open(&[Effect::IO], left_tail);
    let right = open(&[Effect::FS], right_tail);
    let mut solver = EffectRowSolver::new();

    solver.unify(&left, &right).unwrap();

    let resolved_left = solver.resolve(&left).unwrap();
    let resolved_right = solver.resolve(&right).unwrap();
    assert_eq!(resolved_left, resolved_right);
    match resolved_left {
        EffectRow::Open(effects, residual) => {
            assert!(effects.contains(&Effect::IO));
            assert!(effects.contains(&Effect::FS));
            assert_ne!(residual, left_tail);
            assert_ne!(residual, right_tail);
        }
        other => panic!("unrelated tails must retain a residual variable: {other:?}"),
    }
}

#[test]
fn common_tail_with_different_explicit_effects_can_be_constrained() {
    let shared = Region::fresh();
    let left = open(&[Effect::IO], shared);
    let right = open(&[Effect::FS], shared);
    let mut solver = EffectRowSolver::new();

    solver.unify(&left, &right).unwrap();

    assert_eq!(
        solver.resolve(&left).unwrap(),
        solver.resolve(&right).unwrap()
    );
}

#[test]
fn occurs_check_rejects_recursive_bindings_without_mutating_state() {
    let first = Region::fresh();
    let second = Region::fresh();
    let mut solver = EffectRowSolver::new();

    let self_cycle = solver.bind(first, open(&[Effect::IO], first));
    assert!(matches!(
        self_cycle,
        Err(EffectRowSolveError::CyclicTail(_))
    ));
    assert_eq!(solver.resolve(&open(&[], first)).unwrap(), open(&[], first));

    solver.bind(first, open(&[], second)).unwrap();
    let mutual_cycle = solver.bind(second, open(&[], first));
    assert!(matches!(
        mutual_cycle,
        Err(EffectRowSolveError::CyclicTail(_))
    ));
    assert_eq!(
        solver.resolve(&open(&[], second)).unwrap(),
        open(&[], second)
    );
}

#[test]
fn inconsistent_closed_constraints_roll_back_only_the_failed_operation() {
    let tail = Region::fresh();
    let mut solver = EffectRowSolver::new();
    solver.bind(tail, closed(&[Effect::IO])).unwrap();

    let failure = solver.unify(&open(&[], tail), &closed(&[Effect::FS]));
    assert!(matches!(
        failure,
        Err(EffectRowSolveError::IncompatibleClosedRows { .. })
    ));
    assert_eq!(
        solver.resolve(&open(&[], tail)).unwrap(),
        closed(&[Effect::IO])
    );
}

#[test]
fn solver_applies_bindings_to_nested_callback_types() {
    let tail = Region::fresh();
    let callback = Type::Function {
        param: Box::new(Type::unit()),
        ret: Box::new(Type::unit()),
        effect: open(&[Effect::IO], tail),
        cap: Capability::Ref,
    };
    let nested = Type::Array(Box::new(Type::Record(vec![("cb".to_string(), callback)])));
    let mut solver = EffectRowSolver::new();
    solver.bind(tail, closed(&[Effect::FS])).unwrap();

    let resolved = solver.apply_to_type(&nested).unwrap();
    let Type::Array(record) = resolved else {
        panic!("expected array");
    };
    let Type::Record(fields) = *record else {
        panic!("expected record");
    };
    let Type::Function { effect, .. } = &fields[0].1 else {
        panic!("expected callback");
    };
    assert_eq!(*effect, closed(&[Effect::FS, Effect::IO]));
}

#[test]
fn equivalent_effect_sets_deduplicate_and_sort_deterministically() {
    let tail = Region::fresh();
    let mut solver = EffectRowSolver::new();
    solver
        .bind(tail, closed(&[Effect::Net, Effect::IO, Effect::IO]))
        .unwrap();
    assert_eq!(
        solver
            .resolve(&open(&[Effect::IO, Effect::FS], tail))
            .unwrap(),
        closed(&[Effect::FS, Effect::IO, Effect::Net])
    );
}
