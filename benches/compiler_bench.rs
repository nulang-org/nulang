//! Compiler-latency benchmarks.
//!
//! These benchmarks time compilation itself, not execution. They provide a
//! stable baseline for Nulang's compile-time goal: preserve a fast edit/run
//! loop while improving generated-code quality.

use criterion::{black_box, criterion_group, BenchmarkId, Criterion};
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

fn generated_program(functions: usize) -> String {
    let mut source = String::with_capacity(functions * 64 + 8);
    for i in 0..functions {
        source.push_str(&format!("fn f{i}(x: Int) -> Int {{ x * 3 + {i} }};\n"));
    }
    source.push_str("0");
    source
}

fn lower_to_mir(source: &str) -> nulang::mir::Module {
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
                .expect("capability check failed");
        }
    }

    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    nulang::mir_lower::lower_module(&hir).expect("MIR lowering failed")
}

fn compile_bytecode(source: &str) -> nulang::bytecode::CodeModule {
    let mut mir = lower_to_mir(source);
    nulang::mir_codegen::compile_mir(&mut mir, "compiler-bench").expect("bytecode codegen failed")
}

fn bench_frontend_to_mir(c: &mut Criterion) {
    let mut group = c.benchmark_group("compile/frontend_to_mir");
    for functions in [20usize, 100, 500] {
        let source = generated_program(functions);
        group.bench_with_input(
            BenchmarkId::from_parameter(functions),
            &source,
            |b, source| b.iter(|| black_box(lower_to_mir(black_box(source)))),
        );
    }
    group.finish();
}

fn bench_source_to_bytecode(c: &mut Criterion) {
    let mut group = c.benchmark_group("compile/source_to_bytecode");
    for functions in [20usize, 100, 500] {
        let source = generated_program(functions);
        group.bench_with_input(
            BenchmarkId::from_parameter(functions),
            &source,
            |b, source| b.iter(|| black_box(compile_bytecode(black_box(source)))),
        );
    }
    group.finish();
}

#[cfg(feature = "native-codegen")]
fn bench_source_to_native_aot(c: &mut Criterion) {
    use nulang::aot::AotModule;

    let mut group = c.benchmark_group("compile/source_to_native_aot");
    // Keep the largest AOT case bounded so the normal benchmark suite remains
    // practical in CI; the 500-function frontend/bytecode cases still expose
    // scaling behavior for the language-owned compiler phases.
    for functions in [20usize, 100] {
        let source = generated_program(functions);
        group.bench_with_input(
            BenchmarkId::from_parameter(functions),
            &source,
            |b, source| {
                b.iter(|| {
                    let mir = lower_to_mir(black_box(source));
                    black_box(AotModule::compile(&mir).expect("AOT compile failed"))
                })
            },
        );
    }
    group.finish();
}

#[cfg(feature = "native-codegen")]
criterion_group!(
    benches,
    bench_frontend_to_mir,
    bench_source_to_bytecode,
    bench_source_to_native_aot
);

#[cfg(not(feature = "native-codegen"))]
criterion_group!(benches, bench_frontend_to_mir, bench_source_to_bytecode);
