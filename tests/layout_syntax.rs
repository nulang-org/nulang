//! Experimental indentation syntax: behavioral regression tests.
//! Keep this interface opt-in; the default lexer and parser must not change.

use nulang::ast::Decl;
use nulang::layout::lex_layout;
use nulang::lexer::{Lexer, TokenKind};
use nulang::parser::Parser;

fn parse_layout(source: &str) -> nulang::ast::AstModule {
    let tokens = lex_layout(source).expect("layout lowering should succeed");
    Parser::new(tokens)
        .parse_module()
        .expect("lowered syntax should parse with the existing parser")
}

#[test]
fn lowers_a_function_body_into_the_existing_ast() {
    let module = parse_layout(
        "fn add(x: Int, y: Int) -> Int:\n    let total = x + y\n    total\n",
    );
    assert!(module.decls.iter().any(
        |decl| matches!(decl, Decl::Function { name, .. } if name == "add")
    ));
}

#[test]
fn lowers_nested_actor_and_behavior_bodies() {
    let module = parse_layout(
        "actor Counter:\n    state count: Int = 0\n\n    behavior get() -> Int:\n        self.count\n",
    );
    assert!(module.decls.iter().any(
        |decl| matches!(decl, Decl::Actor { name, .. } if name == "Counter")
    ));
}

#[test]
fn leaves_existing_explicit_braces_untouched() {
    let source = "fn add(x: Int, y: Int) -> Int {\n    x + y\n}\n";
    let canonical = Lexer::new(source).lex().unwrap();
    let experimental = lex_layout(source).unwrap();
    assert_eq!(experimental, canonical);
}

#[test]
fn rejects_missing_body_indentation() {
    let err = lex_layout("fn value() -> Int:\n42\n").unwrap_err();
    assert!(err.to_string().contains("indent"), "{err}");
}

#[test]
fn rejects_inconsistent_dedentation() {
    let err = lex_layout("fn value() -> Int:\n    42\n  43\n").unwrap_err();
    assert!(err.to_string().contains("indent"), "{err}");
}

#[test]
fn rejects_unexpected_extra_indentation() {
    let err = lex_layout("fn value() -> Int:\n    42\n        43\n").unwrap_err();
    assert!(err.to_string().contains("indent"), "{err}");
}

#[test]
fn preserves_source_offsets_in_synthetic_braces() {
    let source = "fn value() -> Int:\n    42\n";
    let tokens = lex_layout(source).unwrap();
    let opening = tokens.iter().find(|token| token.kind == TokenKind::LBrace).unwrap();
    assert_eq!(opening.span.start as usize, source.find(':').unwrap());
    let closing = tokens.iter().find(|token| token.kind == TokenKind::RBrace).unwrap();
    assert_eq!(closing.span.start as usize, source.len());
}

#[test]
fn ignores_blank_lines_and_comments_between_header_and_body() {
    let module = parse_layout("fn value() -> Int:\n\n    // comment\n    42\n");
    assert!(module.decls.iter().any(
        |decl| matches!(decl, Decl::Function { name, .. } if name == "value")
    ));
}
