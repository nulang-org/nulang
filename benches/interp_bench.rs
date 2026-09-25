//! Pure-interpreter throughput benchmarks.
//!
//! Unlike `vm_bench`, these run with the JIT tiering disabled
//! (`VM::new_without_jit`) so every instruction flows through the
//! interpreter. This isolates interpreter dispatch cost — the baseline
//! that JIT tiering speeds up — and is the measurement target for
//! interpreter optimizations (dispatch throughput, macro-op fusion,
//! cache-line layout). Run with:
//!   cargo bench --bench bench_main -- interp

use criterion::{black_box, criterion_group, BatchSize, Criterion};
use nulang::bytecode::CodeModule;
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;

/// Compile `source` through the full frontend → bytecode pipeline and return
/// the compiled module. Panics on compile failure.
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
        match decl {
            nulang::ast::Decl::Function { body, .. } => {
                cap_analyzer
                    .infer_cap(&cap_ctx, body)
                    .expect("cap check failed");
            }
            _ => {}
        }
    }
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("mir lower failed");
    nulang::mir_codegen::compile_mir(&mut mir, "bench").expect("codegen failed")
}

/// Fresh interpreter-only VM loaded with a clone of `module`. Each timed
/// iteration gets a fresh VM over a cheap `CodeModule` clone (compiled once
/// per benchmark). The JIT session is absent, so nothing can tier up mid-run.
fn fresh_interp_vm(module: &CodeModule) -> VM {
    let mut vm = VM::new_without_jit();
    vm.load_module(module.clone());
    vm
}

fn bench_interp_int_loop(c: &mut Criterion) {
    let source =
        "var sum = 0; var i = 0; while i < 1000 { sum = sum + i * 2 - i / 3; i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("interp/int_loop", |b| {
        b.iter_batched(
            || fresh_interp_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_interp_float_loop(c: &mut Criterion) {
    let source = "var sum = 0.0; var i = 0; while i < 500 { sum = sum + perform Int.to_float(i) * 2.5 - perform Int.to_float(i) / 3.0; i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("interp/float_loop", |b| {
        b.iter_batched(
            || fresh_interp_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_interp_function_call(c: &mut Criterion) {
    let source = "fn add(x: Int, y: Int) -> Int { x + y }; var sum = 0; var i = 0; while i < 500 { sum = add(sum, i); i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("interp/function_call", |b| {
        b.iter_batched(
            || fresh_interp_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_interp_record_loop(c: &mut Criterion) {
    let source = "let r = { x: 1, y: 2, z: 3 }; var sum = 0; var i = 0; while i < 1000 { sum = sum + r.x + r.y + r.z; i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("interp/record_loop", |b| {
        b.iter_batched(
            || fresh_interp_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

/// A loop below HOT_THRESHOLD (500 < 1000 iterations), so no region ever
/// tiers up. Comparing JIT-enabled (`VM::new`) vs JIT-disabled
/// (`new_without_jit`) on identical source quantifies the per-instruction
/// JIT-probe overhead the default path pays on cold code. The VM now gates
/// probes through a precomputed candidate-PC bitmap and updates a dense hot
/// counter only at real execution-region entries.
fn bench_interp_cold_jit_probe(c: &mut Criterion) {
    let source =
        "var sum = 0; var i = 0; while i < 500 { sum = sum + i * 2 - i / 3; i = i + 1; }; sum";
    let module = compile(source);

    // Baseline: JIT disabled — pure interpreter dispatch, no probes.
    c.bench_function("interp/cold_jit_off", |b| {
        b.iter_batched(
            || fresh_interp_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });

    // JIT enabled but nothing hot: only candidate region-entry PCs probe.
    c.bench_function("interp/cold_jit_on", |b| {
        b.iter_batched(
            || {
                let mut vm = VM::new(); // JIT enabled (default)
                vm.load_module(module.clone());
                vm
            },
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(
    benches,
    bench_interp_int_loop,
    bench_interp_float_loop,
    bench_interp_function_call,
    bench_interp_record_loop,
    bench_interp_cold_jit_probe,
);
