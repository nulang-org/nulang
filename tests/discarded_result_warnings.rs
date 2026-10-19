//! Regression-first tests for accidentally ignored typed errors.
//! W0301 is non-fatal in Nulang 1.x and shares the existing strict warning policy.
use nulang::ast::{Expr, Literal};
use nulang::typechecker::TypeChecker;
use nulang::types::{Capability, Span, Type, TypeContext};

fn result_type() -> Type {
    Type::Variant(vec![
        ("Ok".to_string(), Some(Type::int())),
        ("Error".to_string(), Some(Type::string())),
    ])
}

fn var(name: &str) -> Expr {
    Expr::Var(name.to_string(), Span::default())
}

fn int(value: i64) -> Expr {
    Expr::Literal(Literal::Int(value), Span::default())
}

fn block(exprs: Vec<Expr>) -> Expr {
    Expr::Block { exprs, span: Span::default() }
}

fn infer_with_response(expr: Expr, ty: Type) -> TypeChecker {
    let mut ctx = TypeContext::new();
    ctx.bind("response", ty, Capability::Ref, false);
    let mut tc = TypeChecker::new();
    tc.infer_expr(&ctx, &expr).expect("expression typechecks");
    tc
}

#[test]
fn discarded_result_in_non_final_block_position_emits_w0301() {
    let tc = infer_with_response(block(vec![var("response"), int(42)]), result_type());
    assert_eq!(tc.warnings.len(), 1, "{:?}", tc.warnings);
    assert_eq!(tc.warnings[0].code, "W0301");
    assert!(tc.warnings[0].msg.contains("Result"));
}

#[test]
fn result_returned_from_block_is_not_discarded() {
    let tc = infer_with_response(block(vec![int(42), var("response")]), result_type());
    assert!(tc.warnings.is_empty(), "{:?}", tc.warnings);
}

#[test]
fn a_result_passed_through_the_block_is_not_discarded() {
    let tc = infer_with_response(block(vec![var("response")]), result_type());
    assert!(tc.warnings.is_empty(), "{:?}", tc.warnings);
}

#[test]
fn ordinary_non_final_expressions_remain_valid_and_warning_free() {
    let tc = infer_with_response(block(vec![int(1), int(42)]), result_type());
    assert!(tc.warnings.is_empty(), "{:?}", tc.warnings);
}

#[test]
fn option_and_unrelated_variants_are_not_classified_as_result() {
    let option = Type::Variant(vec![
        ("Some".to_string(), Some(Type::int())),
        ("None".to_string(), None),
    ]);
    let tc = infer_with_response(block(vec![var("response"), int(42)]), option);
    assert!(tc.warnings.is_empty(), "{:?}", tc.warnings);

    let custom = Type::Variant(vec![
        ("Ok".to_string(), Some(Type::int())),
        ("Error".to_string(), Some(Type::string())),
        ("Cancelled".to_string(), None),
    ]);
    let tc = infer_with_response(block(vec![var("response"), int(42)]), custom);
    assert!(tc.warnings.is_empty(), "{:?}", tc.warnings);
}

#[test]
fn nested_blocks_report_only_the_result_actually_discarded() {
    let inner = block(vec![var("response"), int(7)]);
    let tc = infer_with_response(block(vec![inner, int(42)]), result_type());
    assert_eq!(tc.warnings.len(), 1, "{:?}", tc.warnings);
    assert_eq!(tc.warnings[0].code, "W0301");
}
