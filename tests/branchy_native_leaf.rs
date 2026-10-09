#![cfg(feature = "native-codegen")]

use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::hir_lower::lower_module;
use nulang::lexer::Lexer;
use nulang::mir_codegen::compile_mir;
use nulang::mir_lower::lower_module as lower_mir;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;

fn compile(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");

    let mut type_checker = TypeChecker::new();
    type_checker.check_module(&ast).expect("typecheck");

    let mut effect_checker = EffectChecker::new();
    effect_checker
        .check_module(&ast.decls)
        .expect("effect check");

    let mut cap_analyzer = CapabilityAnalyzer::new();
    let cap_ctx = CapContext::new();
    for decl in nulang::effect_checker::flatten_decls(&ast.decls) {
        if let nulang::ast::Decl::Function { body, .. } = decl {
            cap_analyzer
                .infer_cap(&cap_ctx, body)
                .expect("capability check");
        }
    }

    let hir = lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = lower_mir(&hir).expect("MIR lower");
    compile_mir(&mut mir, "branchy-native-leaf-red").expect("codegen")
}

#[test]
fn branchy_native_leaf_compiles_as_second_fast_function() {
    let source = r#"
        fn adjust(x: Int) -> Int {
            if x < 2500 then {
                x + 3
            } else {
                x - 2
            }
        };

        var sum = 0;
        var i = 0;
        while i < 5000 {
            sum = sum + adjust(i);
            i = i + 1
        };
        sum
    "#;
    let module = compile(source);

    let mut interp = VM::new_without_jit();
    interp.load_module(module.clone());
    let expected = interp.run().expect("interpreter run");

    let mut jit = VM::new();
    jit.load_module(module);
    let actual = jit.run().expect("JIT run");

    assert_eq!(
        actual.as_int(),
        expected.as_int(),
        "native leaf must preserve semantics"
    );
    assert_eq!(
        jit.jit_compile_stats().fast_compiles,
        2,
        "hot caller plus the proven acyclic branchy leaf should each compile once"
    );
}
