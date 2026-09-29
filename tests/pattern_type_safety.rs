//! Regression coverage for statically impossible match patterns.
//!
//! These cases are already impossible from the scrutinee's known type. The
//! typechecker must reject them instead of accepting a dead arm and deferring
//! failure to runtime.

use nulang::ast::{Expr, Literal, Pattern};
use nulang::typechecker::TypeChecker;
use nulang::types::{Capability, NuError, Span, Type, TypeContext};

fn sp() -> Span {
    Span::default()
}

fn int_lit(value: i64) -> Expr {
    Expr::Literal(Literal::Int(value), sp())
}

fn var(name: &str) -> Expr {
    Expr::Var(name.to_string(), sp())
}

fn assert_type_error(err: NuError, expected_fragment: &str) {
    match err {
        NuError::TypeError { msg, .. } => assert!(
            msg.contains(expected_fragment),
            "expected type error containing {expected_fragment:?}, got {msg:?}"
        ),
        other => panic!("expected TypeError, got {other:?}"),
    }
}

#[test]
fn unknown_variant_constructor_is_rejected() {
    let mut ctx = TypeContext::new();
    ctx.bind(
        "value",
        Type::Variant(vec![
            ("Some".to_string(), Some(Type::int())),
            ("None".to_string(), None),
        ]),
        Capability::Ref,
        false,
    );

    let expr = Expr::Match {
        scrutinee: Box::new(var("value")),
        arms: vec![(
            Pattern::Variant("Missing".to_string(), None),
            None,
            int_lit(0),
        )],
        span: sp(),
    };

    let err = TypeChecker::new()
        .infer_expr(&ctx, &expr)
        .expect_err("unknown variant constructors must be rejected");
    assert_type_error(err, "Unknown variant constructor");
}

#[test]
fn payload_pattern_on_payloadless_variant_is_rejected() {
    let mut ctx = TypeContext::new();
    ctx.bind(
        "value",
        Type::Variant(vec![
            ("Some".to_string(), Some(Type::int())),
            ("None".to_string(), None),
        ]),
        Capability::Ref,
        false,
    );

    let expr = Expr::Match {
        scrutinee: Box::new(var("value")),
        arms: vec![(
            Pattern::Variant("None".to_string(), Some(Box::new(Pattern::Wild))),
            None,
            int_lit(0),
        )],
        span: sp(),
    };

    let err = TypeChecker::new()
        .infer_expr(&ctx, &expr)
        .expect_err("payload patterns on payloadless variants must be rejected");
    assert_type_error(err, "does not carry a payload");
}

#[test]
fn missing_payload_pattern_on_payload_variant_is_rejected() {
    let mut ctx = TypeContext::new();
    ctx.bind(
        "value",
        Type::Variant(vec![
            ("Some".to_string(), Some(Type::int())),
            ("None".to_string(), None),
        ]),
        Capability::Ref,
        false,
    );

    let expr = Expr::Match {
        scrutinee: Box::new(var("value")),
        arms: vec![(Pattern::Variant("Some".to_string(), None), None, int_lit(0))],
        span: sp(),
    };

    let err = TypeChecker::new()
        .infer_expr(&ctx, &expr)
        .expect_err("payload-carrying variants require a payload pattern");
    assert_type_error(err, "requires a payload pattern");
}

#[test]
fn tuple_pattern_arity_mismatch_is_rejected() {
    let mut ctx = TypeContext::new();
    ctx.bind(
        "value",
        Type::Tuple(vec![Type::int(), Type::bool()]),
        Capability::Ref,
        false,
    );

    let expr = Expr::Match {
        scrutinee: Box::new(var("value")),
        arms: vec![(
            Pattern::Tuple(vec![Pattern::Var("x".to_string())]),
            None,
            int_lit(0),
        )],
        span: sp(),
    };

    let err = TypeChecker::new()
        .infer_expr(&ctx, &expr)
        .expect_err("tuple pattern arity must match the scrutinee tuple");
    assert_type_error(err, "Tuple pattern arity mismatch");
}

#[test]
fn record_pattern_unknown_field_is_rejected() {
    let mut ctx = TypeContext::new();
    ctx.bind(
        "value",
        Type::Record(vec![("known".to_string(), Type::int())]),
        Capability::Ref,
        false,
    );

    let expr = Expr::Match {
        scrutinee: Box::new(var("value")),
        arms: vec![(
            Pattern::Record(vec![("missing".to_string(), Pattern::Var("x".to_string()))]),
            None,
            int_lit(0),
        )],
        span: sp(),
    };

    let err = TypeChecker::new()
        .infer_expr(&ctx, &expr)
        .expect_err("record patterns must reference known fields");
    assert_type_error(err, "Unknown record field");
}

#[test]
fn tuple_pattern_on_non_tuple_scrutinee_is_rejected() {
    let mut ctx = TypeContext::new();
    ctx.bind("value", Type::int(), Capability::Ref, false);

    let expr = Expr::Match {
        scrutinee: Box::new(var("value")),
        arms: vec![(
            Pattern::Tuple(vec![Pattern::Var("x".to_string())]),
            None,
            int_lit(0),
        )],
        span: sp(),
    };

    let err = TypeChecker::new()
        .infer_expr(&ctx, &expr)
        .expect_err("tuple patterns require tuple scrutinees");
    assert_type_error(err, "Tuple pattern requires a tuple scrutinee");
}

#[test]
fn record_pattern_on_non_record_scrutinee_is_rejected() {
    let mut ctx = TypeContext::new();
    ctx.bind("value", Type::int(), Capability::Ref, false);

    let expr = Expr::Match {
        scrutinee: Box::new(var("value")),
        arms: vec![(
            Pattern::Record(vec![("field".to_string(), Pattern::Var("x".to_string()))]),
            None,
            int_lit(0),
        )],
        span: sp(),
    };

    let err = TypeChecker::new()
        .infer_expr(&ctx, &expr)
        .expect_err("record patterns require record scrutinees");
    assert_type_error(err, "Record pattern requires a record scrutinee");
}

#[test]
fn variant_pattern_on_non_variant_scrutinee_is_rejected() {
    let mut ctx = TypeContext::new();
    ctx.bind("value", Type::bool(), Capability::Ref, false);

    let expr = Expr::Match {
        scrutinee: Box::new(var("value")),
        arms: vec![(
            Pattern::Variant("Some".to_string(), Some(Box::new(Pattern::Wild))),
            None,
            int_lit(0),
        )],
        span: sp(),
    };

    let err = TypeChecker::new()
        .infer_expr(&ctx, &expr)
        .expect_err("variant patterns require variant scrutinees");
    assert_type_error(err, "Variant pattern requires a variant scrutinee");
}
