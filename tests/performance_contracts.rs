use nulang::ast::{Decl, FunctionAnnotation, PerformanceContract};
use nulang::effect_checker::EffectChecker;
use nulang::lexer::Lexer;
use nulang::parser::Parser;

fn parse(source: &str) -> nulang::ast::AstModule {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.lex().expect("lex failed");
    let mut parser = Parser::new(tokens);
    parser.parse_module().expect("parse failed")
}

#[test]
fn test_performance_contract_annotations_parse() {
    let ast = parse(
        r#"
@hot()
@no_block()
@no_suspend()
fn quote_midpoint() -> Int {
    42
}
"#,
    );

    let annotations = match &ast.decls[0] {
        Decl::Function { annotations, .. } => annotations,
        other => panic!("expected function, got {other:?}"),
    };

    assert!(annotations.contains(&FunctionAnnotation::Performance(
        PerformanceContract::Hot
    )));
    assert!(annotations.contains(&FunctionAnnotation::Performance(
        PerformanceContract::NoBlock
    )));
    assert!(annotations.contains(&FunctionAnnotation::Performance(
        PerformanceContract::NoSuspend
    )));
}

#[test]
fn test_no_block_rejects_transitive_io() {
    let ast = parse(
        r#"
fn write_log() -> Unit ! {IO} {
    perform IO.print("x")
}

@no_block()
fn hot_path() -> Unit {
    write_log()
}
"#,
    );
    let mut checker = EffectChecker::new();
    let err = checker
        .check_module(&ast.decls)
        .expect_err("@no_block must reject transitive IO");
    let msg = err.to_string();
    assert!(msg.contains("@no_block()"), "{msg}");
    assert!(msg.contains("IO"), "{msg}");
}

#[test]
fn test_no_suspend_rejects_async_effect() {
    let ast = parse(
        r#"
@no_suspend()
fn hot_path() -> Unit ! {Async} {
}
"#,
    );
    let mut checker = EffectChecker::new();
    let err = checker
        .check_module(&ast.decls)
        .expect_err("@no_suspend must reject Async");
    let msg = err.to_string();
    assert!(msg.contains("@no_suspend()"), "{msg}");
    assert!(msg.contains("Async"), "{msg}");
}

#[test]
fn test_performance_contracts_allow_pure_function() {
    let ast = parse(
        r#"
@hot()
@no_block()
@no_suspend()
fn add_one(x: Int) -> Int {
    x + 1
}
"#,
    );
    let mut checker = EffectChecker::new();
    checker
        .check_module(&ast.decls)
        .expect("pure function should satisfy performance contracts");
}
