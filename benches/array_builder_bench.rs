//! ArrayBuilder construction benchmarks.
//!
//! These are intentionally isolated from the general VM microbenchmarks
//! because the 10K repeated `Array.push` baseline is O(n²) in bytes copied.

use criterion::{black_box, criterion_group, BenchmarkId, Criterion, Throughput};
use nulang::bytecode::CodeModule;
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;
use std::time::Duration;

fn compile(source: &str) -> CodeModule {
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
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("mir lower failed");
    nulang::mir_codegen::compile_mir(&mut mir, "array-builder-bench").expect("codegen failed")
}

fn fresh_vm(module: &CodeModule) -> VM {
    let mut vm = VM::new();
    vm.load_module(module.clone());
    vm
}

fn bench_array_builder_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("array_builder");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    for n in [1_000usize, 10_000usize] {
        let push = compile(&format!(
            "var xs = []; var i = 0; while i < {n} {{ xs = perform Array.push(xs, i); i = i + 1; }}; perform Array.length(xs)"
        ));
        let builder = compile(&format!(
            "var b = perform ArrayBuilder.new(); var i = 0; while i < {n} {{ b = perform ArrayBuilder.push(b, i); i = i + 1; }}; let xs = perform ArrayBuilder.to_array(b); perform Array.length(xs)"
        ));

        group.throughput(Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("array_push", n), &push, |b, module| {
            b.iter_batched(
                || fresh_vm(module),
                |mut vm| black_box(vm.run().unwrap()),
                criterion::BatchSize::SmallInput,
            )
        });

        group.bench_with_input(
            BenchmarkId::new("array_builder", n),
            &builder,
            |b, module| {
                b.iter_batched(
                    || fresh_vm(module),
                    |mut vm| black_box(vm.run().unwrap()),
                    criterion::BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}

criterion_group!(pub benches, bench_array_builder_construction);
