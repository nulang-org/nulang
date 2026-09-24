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


#[test]
fn warnings_are_not_sticky_across_checker_reuse() {
    fn parse(source: &str) -> nulang::ast::AstModule {
        let tokens = Lexer::new(source).lex().expect("lex");
        Parser::new(tokens).parse_module().expect("parse")
    }

    let non_exhaustive = parse(
        r#"
        fn choose(flag: Bool) -> Int {
            match flag {
                | true => 1
            }
        }
        "#,
    );
    let exhaustive = parse(
        r#"
        fn choose(flag: Bool) -> Int {
            match flag {
                | true => 1
                | false => 0
            }
        }
        "#,
    );

    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&non_exhaustive)
        .expect("first typecheck");
    assert!(type_checker
        .take_warnings()
        .iter()
        .any(|warning| warning.code == "W0201"));

    type_checker
        .check_module(&exhaustive)
        .expect("second typecheck");
    assert!(
        type_checker.take_warnings().is_empty(),
        "semantic warnings must belong to the current check only"
    );
}
