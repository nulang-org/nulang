//! End-to-end Nulang numeric-array baselines.
//!
//! Keep compilation outside the timed iteration. Array.new allocations stay
//! inside the measured program, so these are *not* raw SIMD or BLAS timings.
//! Compare identical sources on the interpreter, cold JIT-enabled VM, and
//! repeated JIT-enabled VM; "warm" does not imply SIMD was actually emitted.
//!
//! Run: cargo bench --bench bench_main -- numeric_kernel
//! Minimal profile: cargo bench --locked --no-default-features --bench bench_main -- numeric_kernel
//! Native JIT profile: cargo bench --locked --no-default-features --features native-codegen --bench bench_main -- numeric_kernel

use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput};
use nulang::bytecode::CodeModule;
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;

fn compile(source: &str) -> CodeModule {
    let tokens = Lexer::new(source).lex().expect("numeric bench lex failed");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("numeric bench parse failed");
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .expect("numeric bench typecheck failed");
    let mut effect_checker = EffectChecker::new();
    effect_checker
        .check_module(&ast.decls)
        .expect("numeric bench effect check failed");
    let mut cap_analyzer = CapabilityAnalyzer::new();
    let cap_ctx = CapContext::new();
    for decl in nulang::effect_checker::flatten_decls(&ast.decls) {
        if let nulang::ast::Decl::Function { body, .. } = decl {
            cap_analyzer
                .infer_cap(&cap_ctx, body)
                .expect("numeric bench capability check failed");
        }
    }
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("numeric bench MIR failed");
    nulang::mir_codegen::compile_mir(&mut mir, "numeric-bench")
        .expect("numeric bench codegen failed")
}

fn fresh_interp(module: &CodeModule) -> VM {
    let mut vm = VM::new_without_jit();
    vm.load_module(module.clone());
    vm
}

#[cfg(feature = "native-codegen")]
fn fresh_jit(module: &CodeModule) -> VM {
    let mut vm = VM::new();
    vm.load_module(module.clone());
    vm
}

fn kernel_source(kernel: &str, len: usize) -> String {
    match kernel {
        "sum" => format!(
            "let a = perform Array.new({len}, 7); var i = 0; var total = 0; while i < {len} {{ total = total + a[i]; i = i + 1; }}; total"
        ),
        "dot" => format!(
            "let a = perform Array.new({len}, 2); let b = perform Array.new({len}, 3); var i = 0; var total = 0; while i < {len} {{ total = total + a[i] * b[i]; i = i + 1; }}; total"
        ),
        _ => unreachable!("unknown numerical kernel"),
    }
}

fn bench_numeric_kernels(c: &mut Criterion) {
    for kernel in ["sum", "dot"] {
        let mut group = c.benchmark_group(format!("numeric_kernel/{kernel}"));
        for len in [128usize, 4096] {
            let module = compile(&kernel_source(kernel, len));
            group.throughput(Throughput::Elements(len as u64));

            group.bench_with_input(
                BenchmarkId::new("interpreter", len),
                &module,
                |b, module| {
                    b.iter_batched_ref(
                        || fresh_interp(module),
                        |vm| black_box(vm.run().expect("numeric interpreter benchmark failed")),
                        BatchSize::SmallInput,
                    )
                },
            );

            #[cfg(feature = "native-codegen")]
            {
                group.bench_with_input(
                    BenchmarkId::new("jit_enabled_first_run", len),
                    &module,
                    |b, module| {
                        b.iter_batched_ref(
                            || fresh_jit(module),
                            |vm| black_box(vm.run().expect("numeric first-run benchmark failed")),
                            BatchSize::SmallInput,
                        )
                    },
                );

                group.bench_with_input(
                    BenchmarkId::new("jit_enabled_repeated", len),
                    &module,
                    |b, module| {
                        b.iter_batched_ref(
                            || {
                                let mut vm = fresh_jit(module);
                                vm.run().expect("numeric benchmark warm-up failed");
                                vm
                            },
                            |vm| black_box(vm.run().expect("numeric repeated benchmark failed")),
                            BatchSize::SmallInput,
                        )
                    },
                );
            }
        }
        group.finish();
    }
}

criterion_group!(benches, bench_numeric_kernels);
