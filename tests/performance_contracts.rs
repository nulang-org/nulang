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
@no_alloc()
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
    assert!(annotations.contains(&FunctionAnnotation::Performance(
        PerformanceContract::NoAlloc
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
@no_alloc()
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


fn lower_to_mir(source: &str) -> Result<nulang::mir::Module, nulang::types::NuError> {
    let ast = parse(source);
    let mut type_checker = nulang::typechecker::TypeChecker::new();
    type_checker.check_module(&ast)?;
    let mut effect_checker = EffectChecker::new();
    effect_checker.check_module(&ast.decls)?;
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    nulang::mir_lower::lower_module(&hir)
}

#[test]
fn test_no_alloc_allows_pure_optimized_mir() {
    let mir = lower_to_mir(
        r#"
@no_alloc()
fn add_one(x: Int) -> Int {
    x + 1
}
"#,
    )
    .expect("pure function should satisfy @no_alloc");
    let function = mir
        .functions
        .iter()
        .find(|function| function.name == "add_one")
        .expect("add_one MIR function");
    assert!(function
        .performance_contracts
        .contains(&PerformanceContract::NoAlloc));
}

#[test]
fn test_no_alloc_rejects_managed_heap_allocation() {
    let err = lower_to_mir(
        r#"
@no_alloc()
fn make_string() -> String {
    "allocated"
}
"#,
    )
    .expect_err("string materialization must violate @no_alloc");
    let msg = err.to_string();
    assert!(msg.contains("@no_alloc()"), "{msg}");
    assert!(msg.contains("managed heap allocation"), "{msg}");
}

#[test]
fn test_no_alloc_rejects_transitive_allocation() {
    let err = lower_to_mir(
        r#"
fn make_string() -> String {
    "allocated"
}

@no_alloc()
fn hot_path() -> String {
    make_string()
}
"#,
    )
    .expect_err("transitive managed allocation must violate @no_alloc");
    let msg = err.to_string();
    assert!(msg.contains("hot_path"), "{msg}");
    assert!(msg.contains("@no_alloc()"), "{msg}");
}
