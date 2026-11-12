use nulang::{
    hir_lower, lexer::Lexer, mir_codegen, mir_lower, parser::Parser, typechecker::TypeChecker,
};
fn compile() -> nulang::bytecode::CodeModule {
    let source = "fn value() -> Int { 1 + 2 }";
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let mut tc = TypeChecker::new();
    tc.check_module(&ast).unwrap();
    let hir = hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).unwrap();
    mir_codegen::compile_mir(&mut mir, "core").unwrap()
}
#[test]
fn sequential_compiles_preserve_debug_metadata_and_identical_nbc_bytes() {
    let first = compile();
    let second = compile();
    assert_eq!(
        first.debug_functions, second.debug_functions,
        "HIR-generated temporary names must not depend on earlier compilations"
    );
    let first_bytes = first.to_nbc(Some([1; 32])).expect("serialize first");
    let second_bytes = second.to_nbc(Some([1; 32])).expect("serialize second");
    assert_eq!(
        first_bytes, second_bytes,
        "identical compiler input must produce byte-identical NBC artifacts"
    );
}
