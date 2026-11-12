//! Integration contracts for independent compiler-soundness changes.
//! These tests deliberately combine semantics owned by separate PRs.

use nulang::ast::{Decl, Expr};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

fn parse(source: &str) -> nulang::ast::AstModule {
    let tokens = Lexer::new(source).lex().expect("lex source");
    Parser::new(tokens).parse_module().expect("parse source")
}

#[test]
fn top_level_var_before_main_remains_a_mutable_binding() {
    let module = parse("var counter = 0\nfn main() { counter }");
    let main_body = module
        .decls
        .iter()
        .find_map(|decl| match decl {
            Decl::Function { name, body, .. } if name == "main" => Some(body),
            _ => None,
        })
        .expect("main declaration");
    assert!(
        matches!(main_body, Expr::Let { name, mutable: true, .. } if name == "counter"),
        "the synthetic main wrapper must preserve mutable storage"
    );
}

#[test]
fn incomplete_match_and_discarded_result_report_once_each() {
    let module = parse(
        r#"
        type Result[Ok, Err] = Ok(Ok) | Error(Err)
        type Color = Red | Green
        fn choose(c: Color) -> Int {
            Ok(1);
            match c { | Red => 1 }
        }
        "#,
    );
    let mut checker = TypeChecker::new();
    checker.check_module(&module).expect("typecheck");
    let warnings = checker.take_warnings();
    assert_eq!(warnings.iter().filter(|w| w.code == "W0201").count(), 1);
    assert_eq!(warnings.iter().filter(|w| w.code == "W0301").count(), 1);
    assert_eq!(warnings.len(), 2, "unexpected compiler warnings: {warnings:?}");
}
