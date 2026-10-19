//! Qualified imports: syntax is preserved and rejected safely until module
//! namespace resolution is implemented across the compiler and backends.

use nulang::ast::Decl;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

#[test]
fn parse_alias_is_not_an_ordinary_unqualified_import() {
    let tokens = Lexer::new("import stdlib::list as lists")
        .lex()
        .expect("lex aliased import");
    let module = Parser::new(tokens)
        .parse_module()
        .expect("parse aliased import");
    match &module.decls[0] {
        Decl::Import { path, alias, items, .. } => {
            assert_eq!(path, "stdlib::list");
            assert_eq!(alias.as_deref(), Some("lists"));
            assert!(items.is_empty());
        }
        other => panic!("expected import AST, found {other:?}"),
    }

    let err = TypeChecker::new()
        .check_module(&module)
        .expect_err("raw typechecking must not ignore an unresolved alias");
    assert!(format!("{err}").contains("qualified imports"), "{err}");
}

#[test]
fn legacy_unqualified_import_remains_a_distinct_ast_form() {
    let tokens = Lexer::new("import math").lex().expect("lex legacy import");
    let module = Parser::new(tokens).parse_module().expect("parse legacy import");
    match &module.decls[0] {
        Decl::Import { path, alias, .. } => {
            assert_eq!(path, "math");
            assert!(alias.is_none());
        }
        other => panic!("expected import AST, found {other:?}"),
    }
}
