//! Cross-language parity kernels for comparing Nulang with Go.
//!
//! These are deliberately in-process benchmarks. The Nulang frontend is run
//! once in setup; timed iterations measure a warm JIT execution or an already
//! compiled AOT module. This avoids comparing Nulang CLI/compiler startup with
//! a Go function call.
//!
//! Run with:
//!   cargo bench --bench bench_main -- go_parity
//!
//! Then compare with:
//!   python3 benchmarks/go-parity/compare.py

use criterion::{black_box, criterion_group, BatchSize, Criterion};
use nulang::aot::AotModule;
use nulang::bytecode::CodeModule;
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;

const INT_LOOP: &str =
    "var sum = 0; var i = 0; while i < 100000 { sum = sum + i * 3 - i / 7; i = i + 1; }; sum";

const FLOAT_LOOP: &str =
    "var x = 0.0; var i = 0; while i < 100000 { x = x * 1.000001 + 0.25; i = i + 1; }; x";

const DIRECT_CALL_LOOP: &str =
    "fn add(x: Int, y: Int) -> Int { x + y }; var sum = 0; var i = 0; while i < 100000 { sum = add(sum, i); i = i + 1; }; sum";

const FIB25: &str = r#"
    fn fib(n: Int) -> Int {
        if n < 2 then { n } else { fib(n - 1) + fib(n - 2) }
    }
    fn main() -> Int { fib(25) }
"#;

fn frontend(source: &str) -> (nulang::ast::AstModule, TypeChecker) {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.lex().expect("lex failed");
    let mut parser = Parser::new(tokens);
    let ast = parser.parse_module().expect("parse failed");

    let mut type_checker = TypeChecker::new();
    type_checker.check_module(&ast).expect("typecheck failed");

    let mut effect_checker = EffectChecker::new();
    effect_checker
        .check_module(&ast.decls)
        .expect("effect check failed");

    let mut cap_analyzer = CapabilityAnalyzer::new();
    let cap_ctx = CapContext::new();
    for decl in nulang::effect_checker::flatten_decls(&ast.decls) {
        if let nulang::ast::Decl::Function { body, .. } = decl {
            cap_analyzer
                .infer_cap(&cap_ctx, body)
                .expect("cap check failed");
        }
    }

    (ast, type_checker)
}

fn compile_bytecode(source: &str) -> CodeModule {
    let (ast, type_checker) = frontend(source);
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("mir lower failed");
    nulang::mir_codegen::compile_mir(&mut mir, "go_parity").expect("bytecode codegen failed")
}

fn compile_aot(source: &str) -> AotModule {
    let (ast, type_checker) = frontend(source);
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mir = nulang::mir_lower::lower_module(&hir).expect("mir lower failed");
    AotModule::compile(&mir).expect("aot compile failed")
}

fn warm_jit_vm(module: &CodeModule) -> VM {
    let mut vm = VM::new();
    vm.load_module(module.clone());
    black_box(vm.run().expect("JIT warm-up failed"));
    vm
}

fn bench_jit(c: &mut Criterion, name: &str, source: &str) {
    let module = compile_bytecode(source);
    c.bench_function(&format!("go_parity/jit/{name}"), |b| {
        b.iter_batched(
            || warm_jit_vm(&module),
            |mut vm| black_box(vm.run().expect("JIT execution failed")),
            BatchSize::SmallInput,
        )
    });
}

fn bench_aot(c: &mut Criterion, name: &str, source: &str) {
    let module = compile_aot(source);
    c.bench_function(&format!("go_parity/aot/{name}"), |b| {
        b.iter(|| black_box(module.run().expect("AOT execution failed")))
    });
}

fn bench_go_parity(c: &mut Criterion) {
    for (name, source) in [
        ("int_loop", INT_LOOP),
        ("float_loop", FLOAT_LOOP),
        ("direct_call_loop", DIRECT_CALL_LOOP),
        ("fib25", FIB25),
    ] {
        bench_jit(c, name, source);
        bench_aot(c, name, source);
    }
}

criterion_group!(benches, bench_go_parity);
