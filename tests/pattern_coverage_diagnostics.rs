use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::types::NuWarning;

fn warnings_for(source: &str) -> Vec<NuWarning> {
    let tokens = Lexer::new(source).lex().expect("lex");
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_module().expect("parse");
    let mut type_checker = TypeChecker::new();
    type_checker.check_module(&ast).expect("typecheck");
    type_checker.take_warnings()
}

#[test]
fn non_exhaustive_variant_match_emits_w0201() {
    let warnings = warnings_for(
        r#"
        type Color = Red | Green | Blue
        fn name(c: Color) -> String {
            match c {
                | Red => "red"
                | Green => "green"
            }
        }
        "#,
    );

    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "W0201");
    assert!(warnings[0].msg.contains("Blue"));
}

#[test]
fn exhaustive_variant_match_has_no_coverage_warning() {
    let warnings = warnings_for(
        r#"
        type Color = Red | Green | Blue
        fn name(c: Color) -> String {
            match c {
                | Red => "red"
                | Green => "green"
                | Blue => "blue"
            }
        }
        "#,
    );

    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}

#[test]
fn redundant_variant_arm_emits_w0202() {
    let warnings = warnings_for(
        r#"
        type Color = Red | Green
        fn warm(c: Color) -> Int {
            match c {
                | Red => 1
                | Red => 2
                | _ => 0
            }
        }
        "#,
    );

    assert!(warnings.iter().any(|w| w.code == "W0202"));
}

#[test]
fn non_exhaustive_bool_match_emits_w0201() {
    let warnings = warnings_for(
        r#"
        fn choose(flag: Bool) -> Int {
            match flag {
                | true => 1
            }
        }
        "#,
    );

    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "W0201");
    assert!(warnings[0].msg.contains("false"));
}

#[test]
fn exhaustive_bool_match_has_no_coverage_warning() {
    let warnings = warnings_for(
        r#"
        fn choose(flag: Bool) -> Int {
            match flag {
                | true => 1
                | false => 0
            }
        }
        "#,
    );

    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}

fn typecheck_source(source: &str) -> nulang::types::NuResult<nulang::types::Type> {
    let tokens = Lexer::new(source).lex()?;
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_module()?;
    TypeChecker::new().check_module(&ast)
}

#[test]
fn impossible_variant_constructor_is_a_type_error() {
    let source = r#"
        type Color = Red | Green
        fn classify(c: Color) -> Int {
            match c {
                | Red => 1
                | Green => 2
                | Blue => 3
            }
        }
    "#;
    let err = typecheck_source(source).expect_err("Blue is not a Color");
    assert!(err.to_string().contains("Blue"), "diagnostic: {err}");
}

#[test]
fn payload_variant_requires_payload_pattern() {
    let source = r#"
        type MaybeNum = Some(Int) | None
        fn classify(c: MaybeNum) -> Int {
            match c {
                | Some => 1
                | None => 0
            }
        }
    "#;
    let err = typecheck_source(source).expect_err("Some requires a payload pattern");
    assert!(err.to_string().contains("Some"), "diagnostic: {err}");
}

#[test]
fn nullary_variant_rejects_payload_pattern() {
    let source = r#"
        type MaybeNum = Some(Int) | None
        fn classify(c: MaybeNum) -> Int {
            match c {
                | Some(_) => 1
                | None(unexpected) => 0
            }
        }
    "#;
    let err = typecheck_source(source).expect_err("None takes no payload");
    assert!(err.to_string().contains("None"), "diagnostic: {err}");
}
