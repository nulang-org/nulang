//! Regression coverage for numerical loops before introducing a tensor ABI.
//!
//! Exercise the full Nulang frontend and VM (not an equivalent Rust algorithm).
//! These cases establish semantic parity before numerical optimizations.

use nulang::bytecode::CodeModule;
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;

fn compile(source: &str) -> CodeModule {
    let tokens = Lexer::new(source).lex().expect("numeric kernel lex failed");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("numeric kernel parse failed");
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .expect("numeric kernel typecheck failed");
    let mut effect_checker = EffectChecker::new();
    effect_checker
        .check_module(&ast.decls)
        .expect("numeric kernel effect check failed");
    let mut cap_analyzer = CapabilityAnalyzer::new();
    let cap_ctx = CapContext::new();
    for decl in nulang::effect_checker::flatten_decls(&ast.decls) {
        if let nulang::ast::Decl::Function { body, .. } = decl {
            cap_analyzer
                .infer_cap(&cap_ctx, body)
                .expect("numeric kernel capability check failed");
        }
    }
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("numeric kernel MIR failed");
    nulang::mir_codegen::compile_mir(&mut mir, "numeric-kernel-test")
        .expect("numeric kernel codegen failed")
}

fn interpreter_result(module: &CodeModule) -> i64 {
    let mut vm = VM::new_without_jit();
    vm.load_module(module.clone());
    vm.run()
        .expect("numeric kernel interpreter failed")
        .as_int()
        .expect("numeric kernel must produce Int")
}

#[cfg(feature = "native-codegen")]
fn jit_results(module: &CodeModule) -> (i64, i64) {
    let mut vm = VM::new();
    vm.load_module(module.clone());
    let first = vm
        .run()
        .expect("numeric kernel first JIT-enabled run failed")
        .as_int()
        .expect("numeric kernel must produce Int");
    let second = vm
        .run()
        .expect("numeric kernel repeated JIT-enabled run failed")
        .as_int()
        .expect("numeric kernel must produce Int");
    (first, second)
}

fn assert_kernel(source: &str, expected: i64) {
    let module = compile(source);
    assert_eq!(interpreter_result(&module), expected, "interpreter result");
    #[cfg(feature = "native-codegen")]
    {
        let (first, warm) = jit_results(&module);
        assert_eq!(first, expected, "first JIT-enabled result");
        assert_eq!(warm, expected, "repeated JIT-enabled result");
    }
}

#[test]
fn contiguous_integer_reduction_preserves_sum() {
    for len in [0usize, 1, 17, 4096] {
        let source = format!(
            "let a = perform Array.new({len}, 7); var i = 0; var total = 0; while i < {len} {{ total = total + a[i]; i = i + 1; }}; total"
        );
        assert_kernel(&source, (len as i64) * 7);
    }
}

#[test]
fn integer_dot_product_preserves_accumulation() {
    for len in [0usize, 1, 17, 4096] {
        let source = format!(
            "let a = perform Array.new({len}, 2); let b = perform Array.new({len}, 3); var i = 0; var total = 0; while i < {len} {{ total = total + a[i] * b[i]; i = i + 1; }}; total"
        );
        assert_kernel(&source, (len as i64) * 6);
    }
}

#[test]
fn integer_reduction_handles_negative_elements() {
    let source = "let a = [-4, 7, -9, 12]; var i = 0; var total = 0; while i < 4 { total = total + a[i]; i = i + 1; }; total";
    assert_kernel(source, 6);
}
