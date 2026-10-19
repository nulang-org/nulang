//! Conservative finite-domain match diagnostics (RFC 0002-compatible warnings).
use nulang::ast::{Expr, Literal, Pattern};
use nulang::match_coverage::check_coverage;
use nulang::types::{PrimitiveType, Span, Type};

fn body() -> Expr {
    Expr::Literal(Literal::Int(0), Span::default())
}

fn arm(pattern: Pattern, guarded: bool) -> (Pattern, Option<Expr>, Expr) {
    (
        pattern,
        guarded.then(|| Expr::Literal(Literal::Bool(true), Span::default())),
        body(),
    )
}

fn bool_type() -> Type {
    Type::Primitive(PrimitiveType::Bool)
}

fn closed_color_type() -> Type {
    Type::Variant(vec![
        ("Red".to_string(), None),
        ("Green".to_string(), None),
        ("Blue".to_string(), None),
    ])
}

#[test]
fn missing_boolean_branch_is_warning_not_a_type_error() {
    let warnings = check_coverage(
        &bool_type(),
        &[arm(Pattern::Lit(Literal::Bool(true)), false)],
        Span::new(10, 25),
    );
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "W0201");
    assert!(warnings[0].msg.contains("false"), "{:?}", warnings[0]);
}

#[test]
fn both_boolean_branches_are_exhaustive() {
    let warnings = check_coverage(
        &bool_type(),
        &[
            arm(Pattern::Lit(Literal::Bool(true)), false),
            arm(Pattern::Lit(Literal::Bool(false)), false),
        ],
        Span::default(),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn guarded_arm_does_not_prove_coverage() {
    let warnings = check_coverage(
        &bool_type(),
        &[
            arm(Pattern::Lit(Literal::Bool(true)), true),
            arm(Pattern::Lit(Literal::Bool(false)), false),
        ],
        Span::default(),
    );
    assert!(warnings.iter().any(|w| w.code == "W0201" && w.msg.contains("true")));
}

#[test]
fn arm_after_unconditional_wildcard_is_unreachable() {
    let warnings = check_coverage(
        &bool_type(),
        &[
            arm(Pattern::Wild, false),
            arm(Pattern::Lit(Literal::Bool(false)), false),
        ],
        Span::default(),
    );
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "W0202");
}

#[test]
fn missing_closed_variant_constructor_has_witness() {
    let warnings = check_coverage(
        &closed_color_type(),
        &[
            arm(Pattern::Variant("Red".into(), None), false),
            arm(Pattern::Variant("Blue".into(), None), false),
        ],
        Span::default(),
    );
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "W0201");
    assert!(warnings[0].msg.contains("Green"));
}

#[test]
fn payload_patterns_only_cover_a_constructor_when_irrefutable() {
    let ty = Type::Variant(vec![
        ("Some".to_string(), Some(Box::new(Type::int()))),
        ("None".to_string(), None),
    ]);
    let warnings = check_coverage(
        &ty,
        &[
            arm(Pattern::Variant("Some".into(), Some(Box::new(Pattern::Lit(Literal::Int(1))))), false),
            arm(Pattern::Variant("None".into(), None), false),
        ],
        Span::default(),
    );
    assert!(warnings.iter().any(|w| w.code == "W0201" && w.msg.contains("Some")));
}

#[test]
fn infinite_domain_does_not_generate_missing_case_warnings() {
    let warnings = check_coverage(
        &Type::int(),
        &[arm(Pattern::Lit(Literal::Int(1)), false)],
        Span::default(),
    );
    assert!(warnings.is_empty());
}

#[test]
fn typechecker_exposes_warnings_for_a_valid_nonexhaustive_variant_match() {
    let source = r#"
type Color = Red | Green | Blue
fn pick(c: Color) -> Int {
    match c { | Red => 1 | Green => 2 }
}
"#;
    let tokens = nulang::lexer::Lexer::new(source).lex().unwrap();
    let ast = nulang::parser::Parser::new(tokens).parse_module().unwrap();
    let mut checker = nulang::typechecker::TypeChecker::new();
    checker.check_module(&ast).expect("default compilation must remain valid");
    assert!(checker.warnings.iter().any(|w| w.code == "W0201" && w.msg.contains("Blue")),
        "{:?}", checker.warnings);
}
