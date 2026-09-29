//! Structured-concurrency result-shape regressions.
//!
//! These tests pin the source contract before parallel execution is enabled:
//! every non-empty `par` returns a source-ordered tuple, and a trailing comma
//! distinguishes a one-element tuple pattern from a grouped pattern.

use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::types::NuResult;

fn check(source: &str) -> NuResult<nulang::types::Type> {
    let tokens = Lexer::new(source).lex()?;
    let module = Parser::new(tokens).parse_module()?;
    TypeChecker::new().check_module(&module)
}

#[test]
fn par_result_type_is_source_ordered_tuple() {
    let result = check(
        r#"
        fn main() -> Int {
            let result: (Int, Int, Int) = par {
                1
                2
                3
            }
            match result {
                | (a, b, c) => a * 100 + b * 10 + c
            }
        }
        "#,
    );

    assert!(
        result.is_ok(),
        "par branches must contribute a source-ordered tuple result: {:?}",
        result.err()
    );
}

#[test]
fn trailing_comma_preserves_one_element_tuple_pattern() {
    let result = check(
        r#"
        fn main() -> Int {
            match (7,) {
                | (x,) => x
            }
        }
        "#,
    );

    assert!(
        result.is_ok(),
        "(x,) must remain a one-element tuple pattern instead of collapsing to x: {:?}",
        result.err()
    );
}

#[test]
fn one_branch_par_is_a_one_element_tuple() {
    let result = check(
        r#"
        fn main() -> Int {
            let result = par {
                7
            }
            match result {
                | (x,) => x
            }
        }
        "#,
    );

    assert!(
        result.is_ok(),
        "one-branch par must preserve tuple arity: {:?}",
        result.err()
    );
}

#[test]
fn empty_par_remains_unit() {
    let result = check(
        r#"
        fn main() -> Unit {
            par {}
        }
        "#,
    );

    assert!(
        result.is_ok(),
        "empty par must remain Unit: {:?}",
        result.err()
    );
}

#[test]
fn formatter_preserves_unary_tuple_pattern_trailing_comma() {
    let formatted = nulang::fmt::format_source(
        r#"fn main() -> Int {
    match (7,) {
        | (x,) => x
    }
}"#,
    )
    .expect("format source");

    assert!(
        formatted.contains("(x,)"),
        "formatter must preserve the unary-tuple pattern marker: {formatted}"
    );
}
