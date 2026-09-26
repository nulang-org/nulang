#![cfg(feature = "native-codegen")]

use nulang::hir_lower::lower_module;
use nulang::lexer::Lexer;
use nulang::mir_codegen::compile_mir;
use nulang::mir_lower::lower_module as lower_mir;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;

/// Architecture-neutral execution gate for a real tiered-JIT transition.
///
/// This intentionally lives as an integration test instead of selecting a
/// unit test from `src/jit/tests.rs`: cross-target CI can compile only the
/// normal library plus this focused test, avoiding unrelated architecture-
/// specific unit-test code while still proving that the VM crosses its hot
/// threshold, emits native code for the host ISA, executes it, and preserves
/// interpreter semantics.
#[test]
fn tiered_jit_executes_hot_loop() {
    let source = r#"
        fn bump(x: Int) -> Int { x + 1 }
        fn main() -> Int {
            var s = 0;
            var i = 0;
            while i < 20000 {
                s = bump(s);
                i = i + 1
            };
            s
        }
    "#;

    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut tc = TypeChecker::new();
    tc.check_module(&ast).expect("typecheck");
    let hir = lower_module(&ast, &tc.inferred_decl_types);
    let mut mir = lower_mir(&hir).expect("mir");
    let module = compile_mir(&mut mir, "riscv_jit_smoke").expect("codegen");

    let mut interp = VM::new_without_jit();
    interp.load_module(module.clone());
    let expected = interp.run().expect("interpreter run");

    let mut jit = VM::new();
    jit.load_module(module);
    let result = jit.run().expect("tiered JIT run");

    assert_eq!(result.as_int(), expected.as_int());
    assert_eq!(result.as_int(), Some(20000));
    assert!(
        jit.jit_compiled_count() > 0,
        "hot loop completed without compiling any JIT region"
    );
}
