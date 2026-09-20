//! JIT tiering benchmarks: hot loop speedup vs interpreter, tier-up latency.

use criterion::{black_box, criterion_group, BatchSize, Criterion};
use nulang::bytecode::CodeModule;
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;

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

fn fresh_vm(module: &CodeModule) -> VM {
    let mut vm = VM::new();
    vm.load_module(module.clone());
    vm
}

fn bench_jit_hot_loop(c: &mut Criterion) {
    let source =
        "var sum = 0; var i = 0; while i < 100000 { sum = sum + i * 3 - i / 7; i = i + 1; }; sum";
    let module = compile(source);

    // Cold: fresh VM per timed iteration. The 100k-iteration loop exceeds
    // HOT_THRESHOLD (1000), so this measures interpretation up to tier-up
    // plus the one-time JIT compile cost plus the remaining JIT-compiled
    // iterations, all in a single run() call.
    c.bench_function("jit/hot_loop_first_run", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });

    // Warm: `run()` fully resets frame/PC state on every call but the JIT
    // session (hot-region cache) persists across calls on the same VM
    // instance. One untimed warm-up run() in setup guarantees the hot
    // region is already compiled; the timed routine then measures a
    // second run() with zero compile overhead, isolating steady-state
    // JIT-compiled throughput from the one-time tier-up cost above.
    c.bench_function("jit/hot_loop_warm", |b| {
        b.iter_batched(
            || {
                let mut vm = fresh_vm(&module);
                let _ = vm.run();
                vm
            },
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

/// A hot loop that calls a function each iteration. `Call`/`TailCall` are not
/// in the JIT compilable opcode set, so `find_compilable_region` fragments at
/// the call: the loop's arithmetic around the call is JIT-compiled, but the
/// call itself (frame push + dispatch) is interpreted every iteration. This
/// quantifies the real-world JIT gap for call-heavy loops — the largest
/// remaining coverage hole — against the pure-interpreter `interp/function_call`
/// baseline and the no-call `jit/hot_loop_warm` ceiling.
/// Parameter-driven numeric loop. Before compiler-owned entry seeds, the
/// callee's incoming `seed`/`limit` registers start as Unknown in JIT type
/// inference, forcing helper-backed arithmetic/comparisons when those values
/// remain live in the hot loop. This benchmark tracks the steady-state benefit
/// of preserving those types through the function prologue.
fn bench_jit_typed_parameter_loop(c: &mut Criterion) {
    let source = r#"
        fn accumulate(seed: Int, limit: Int) -> Int {
            var sum = seed;
            var i = 0;
            while i < limit {
                sum = sum + i;
                i = i + 1
            };
            sum
        }
        fn main() -> Int { accumulate(1, 100000) }
    "#;
    let module = compile(source);

    c.bench_function("jit/typed_parameter_loop_warm", |b| {
        b.iter_batched(
            || {
                let mut vm = fresh_vm(&module);
                let _ = vm.run();
                vm
            },
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });

    // Frozen .nbc artifacts intentionally drop compiler-only JIT type seeds.
    // This variant therefore measures guarded runtime live-in specialization,
    // not the source-compilation fast path above.
    let artifact_module = CodeModule::from_nbc(
        &module.to_nbc(None).expect("encode benchmark module"),
    )
    .expect("decode benchmark module")
    .module;
    c.bench_function("jit/typed_parameter_loop_nbc_warm", |b| {
        b.iter_batched(
            || {
                let mut vm = fresh_vm(&artifact_module);
                let _ = vm.run();
                vm
            },
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_jit_function_call_loop(c: &mut Criterion) {
    let source = "fn add(x: Int, y: Int) -> Int { x + y }; var sum = 0; var i = 0; while i < 100000 { sum = add(sum, i); i = i + 1; }; sum";
    let module = compile(source);

    c.bench_function("jit/function_call_loop", |b| {
        b.iter_batched(
            || {
                let mut vm = fresh_vm(&module);
                let _ = vm.run();
                vm
            },
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });

    // JIT-disabled interp baseline for the IDENTICAL source, so the bench
    // directly shows whether the JIT helps or hurts call-heavy loops.
    c.bench_function("jit/function_call_loop_interp", |b| {
        b.iter_batched(
            || {
                let mut vm = nulang::vm::VM::new_without_jit();
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
    bench_jit_hot_loop,
    bench_jit_typed_parameter_loop,
    bench_jit_function_call_loop
);
