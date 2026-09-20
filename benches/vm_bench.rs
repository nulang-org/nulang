//! VM throughput benchmarks: arithmetic, function calls, closures, dispatch,
//! record/array access, and direct effect dispatch.

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

/// Construct a fresh VM loaded with a clone of `module`. `VM` doesn't
/// implement `Clone` (it owns a JIT session and heap state), so each timed
/// iteration gets a fresh VM over a cheap `CodeModule` clone instead —
/// compiled once per benchmark, not once per iteration.
fn fresh_vm(module: &CodeModule) -> VM {
    let mut vm = VM::new();
    vm.load_module(module.clone());
    vm
}

fn bench_int_arithmetic(c: &mut Criterion) {
    let source =
        "var sum = 0; var i = 0; while i < 1000 { sum = sum + i * 2 - i / 3; i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("vm/int_arithmetic", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_float_arithmetic(c: &mut Criterion) {
    // Float loop: use explicit float literals
    let source = "var sum = 0.0; var i = 0; while i < 500 { sum = sum + perform Int.to_float(i) * 2.5 - perform Int.to_float(i) / 3.0; i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("vm/float_arithmetic", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_function_call(c: &mut Criterion) {
    let source = "fn add(x: Int, y: Int) -> Int { x + y }; fn mul(x: Int, y: Int) -> Int { x * y }; var sum = 0; var i = 0; while i < 500 { sum = add(sum, mul(i, 3)); i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("vm/function_call", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_closure_capture(c: &mut Criterion) {
    let source = "let base = 10; let adder = fn(x: Int) -> Int { x + base }; var sum = 0; var i = 0; while i < 500 { sum = adder(i); i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("vm/closure_capture", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_record_access(c: &mut Criterion) {
    let source = "let r = { x: 1, y: 2, z: 3 }; var sum = 0; var i = 0; while i < 1000 { sum = sum + r.x + r.y + r.z; i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("vm/record_access", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_array_indexing(c: &mut Criterion) {
    let source = "let arr = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]; var sum = 0; var i = 0; while i < 1000 { sum = sum + arr[i % 10]; i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("vm/array_indexing", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_array_view_read_heavy(c: &mut Criterion) {
    // Compare the zero-copy view with an explicitly materialized equivalent.
    // The materialized case is a present-day control, not a historical claim.
    let view_source = r#"
        let base = [0, 1, 2, 3, 4, 5, 6, 7]
        let xs = perform Array.slice(base, 1, 7)
        var sum = 0
        var i = 0
        while i < 1000 {
            sum = sum + xs[i % 6]
            i = i + 1
        }
        sum
    "#;
    let materialized_source = r#"
        let base = [0, 1, 2, 3, 4, 5, 6, 7]
        let xs = [base[1], base[2], base[3], base[4], base[5], base[6]]
        var sum = 0
        var i = 0
        while i < 1000 {
            sum = sum + xs[i % 6]
            i = i + 1
        }
        sum
    "#;
    let view = compile(view_source);
    let materialized = compile(materialized_source);
    let mut group = c.benchmark_group("vm/array_view/read_heavy");
    group.bench_function("zero_copy_view", |b| {
        b.iter_batched(
            || fresh_vm(&view),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("materialized_equivalent", |b| {
        b.iter_batched(
            || fresh_vm(&materialized),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

fn bench_array_view_repeated_mutation(c: &mut Criterion) {
    let view_source = r#"
        let base = [0, 1, 2, 3, 4, 5, 6, 7]
        let xs = perform Array.slice(base, 1, 7)
        var i = 0
        while i < 1000 {
            xs[i % 6] = i
            i = i + 1
        }
        xs[0] + base[1]
    "#;
    let materialized_source = r#"
        let base = [0, 1, 2, 3, 4, 5, 6, 7]
        let xs = [base[1], base[2], base[3], base[4], base[5], base[6]]
        var i = 0
        while i < 1000 {
            xs[i % 6] = i
            i = i + 1
        }
        xs[0] + base[1]
    "#;
    let view = compile(view_source);
    let materialized = compile(materialized_source);
    let mut group = c.benchmark_group("vm/array_view/repeated_mutation");
    group.bench_function("view_cow_then_writes", |b| {
        b.iter_batched(
            || fresh_vm(&view),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("materialized_writes", |b| {
        b.iter_batched(
            || fresh_vm(&materialized),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

fn bench_array_view_cow_mutation(c: &mut Criterion) {
    // Measure first-write detach+copy versus a same-sized materialized array.
    let view_source = r#"
        let base = [0, 1, 2, 3, 4, 5, 6, 7]
        let xs = perform Array.slice(base, 1, 7)
        xs[0] = 99
        xs[0] + xs[5] + base[1]
    "#;
    let materialized_source = r#"
        let base = [0, 1, 2, 3, 4, 5, 6, 7]
        let xs = [base[1], base[2], base[3], base[4], base[5], base[6]]
        xs[0] = 99
        xs[0] + xs[5] + base[1]
    "#;
    let view = compile(view_source);
    let materialized = compile(materialized_source);
    let mut group = c.benchmark_group("vm/array_view/cow_mutation");
    group.bench_function("view_first_write", |b| {
        b.iter_batched(
            || fresh_vm(&view),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("materialized_write", |b| {
        b.iter_batched(
            || fresh_vm(&materialized),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

/// Baseline the `Perform` hot path before module-load name caching.
/// The performed operation is stable for every loop iteration, so repeated
/// parsing/allocation of `Float.sqrt` is pure dispatch overhead.
fn bench_perform_float_sqrt(c: &mut Criterion) {
    let source = "var sum = 0.0; var x = 1.0; var i = 0; while i < 1000 { sum = sum + perform Float.sqrt(x); x = x + 1.0; i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("vm/perform/float_sqrt", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_perform_int_to_float(c: &mut Criterion) {
    let source = "var sum = 0.0; var i = 0; while i < 1000 { sum = sum + perform Int.to_float(i); i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("vm/perform/int_to_float", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_perform_array_length(c: &mut Criterion) {
    let source = "let xs = [1, 2, 3, 4, 5, 6, 7, 8]; var sum = 0; var i = 0; while i < 1000 { sum = sum + perform Array.length(xs); i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("vm/perform/array_length", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

fn bench_perform_string_length(c: &mut Criterion) {
    let source = "let s = \"perform-direct-baseline\"; var sum = 0; var i = 0; while i < 1000 { sum = sum + perform String.length(s); i = i + 1; }; sum";
    let module = compile(source);
    c.bench_function("vm/perform/string_length", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

/// Control benchmark for the statically resolved handler path. The compiler
/// lowers this known handler to PerformDirect, so it is intentionally kept
/// separate from the generic Perform cache measurements above.
fn bench_perform_direct_custom_handler(c: &mut Criterion) {
    let source = "let result = handle { var sum = 0; var i = 0; while i < 500 { sum = sum + perform Counter.ask(); i = i + 1; }; sum } { | Counter.ask() resume => 1 }; result";
    let module = compile(source);
    c.bench_function("vm/perform_direct/custom_handler", |b| {
        b.iter_batched(
            || fresh_vm(&module),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(
    benches,
    bench_int_arithmetic,
    bench_float_arithmetic,
    bench_function_call,
    bench_closure_capture,
    bench_record_access,
    bench_array_indexing,
    bench_array_view_read_heavy,
    bench_array_view_cow_mutation,
    bench_array_view_repeated_mutation,
    bench_perform_float_sqrt,
    bench_perform_int_to_float,
    bench_perform_array_length,
    bench_perform_string_length,
    bench_perform_direct_custom_handler,
);
