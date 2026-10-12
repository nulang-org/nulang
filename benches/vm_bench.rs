//! VM throughput benchmarks: arithmetic, function calls, closures, dispatch,
//! record/array access, specialized numeric fast paths, and array construction.

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

/// Compare repeated copy-on-append construction with one allocation followed
/// by in-place ArrStore writes. Both produce the same 512-element array.
fn bench_array_construction(c: &mut Criterion) {
    let push = compile(
        "var result = []; var i = 0; while i < 512 { result = perform Array.push(result, i); i = i + 1; }; perform Array.length(result)",
    );
    let preallocated = compile(
        "var result = perform Array.new(512, 0); var i = 0; while i < 512 { result[i] = i; i = i + 1; }; perform Array.length(result)",
    );

    c.bench_function("vm/array_push_build", |b| {
        b.iter_batched(
            || fresh_vm(&push),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
    c.bench_function("vm/array_preallocated_build", |b| {
        b.iter_batched(
            || fresh_vm(&preallocated),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

/// Compare the VM's specialized IPow path (binary exponentiation) with the
/// old std.math.pow strategy: an O(exp) user-level recursive multiply loop.
/// Keep the base mutable so MIR constant folding cannot erase the operation.
fn bench_int_pow_fastpath(c: &mut Criterion) {
    let specialized = compile(
        "var sum = 0; var x = 3; var i = 0; while i < 1000 { sum = sum + (x ** 13); x = if x < 7 then x + 1 else 3; i = i + 1; }; sum",
    );
    let user_loop = compile(
        "fn pow_loop(base: Int, exp: Int) -> Int { let rec loop = fn(b: Int, e: Int, acc: Int) -> Int { if e == 0 then acc else loop(b, e - 1, acc * b) }; loop(base, exp, 1) }; var sum = 0; var x = 3; var i = 0; while i < 1000 { sum = sum + pow_loop(x, 13); x = if x < 7 then x + 1 else 3; i = i + 1; }; sum",
    );

    c.bench_function("vm/int_pow_ipow", |b| {
        b.iter_batched(
            || fresh_vm(&specialized),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
    c.bench_function("vm/int_pow_user_loop", |b| {
        b.iter_batched(
            || fresh_vm(&user_loop),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
}

/// Compare native Float.sqrt builtin dispatch with the previous std.math.sqrt
/// implementation, which performed twenty Newton iterations in bytecode for
/// every call. Inputs vary per iteration so this is not a constant-folding
/// benchmark.
fn bench_float_sqrt_fastpath(c: &mut Criterion) {
    let builtin = compile(
        "var sum = 0.0; var x = 1.0; var i = 0; while i < 200 { sum = sum + perform Float.sqrt(x); x = x + 1.0; i = i + 1; }; sum",
    );
    let newton = compile(
        "fn sqrt_loop(x: Float) -> Float { if x < 0.0 then -1.0 else if x == 0.0 then 0.0 else { var guess = x / 2.0; var j = 0; while j < 20 { guess = (guess + x / guess) / 2.0; j = j + 1 }; guess } }; var sum = 0.0; var x = 1.0; var i = 0; while i < 200 { sum = sum + sqrt_loop(x); x = x + 1.0; i = i + 1; }; sum",
    );

    c.bench_function("vm/float_sqrt_builtin", |b| {
        b.iter_batched(
            || fresh_vm(&builtin),
            |mut vm| black_box(vm.run().unwrap()),
            BatchSize::SmallInput,
        )
    });
    c.bench_function("vm/float_sqrt_newton_loop", |b| {
        b.iter_batched(
            || fresh_vm(&newton),
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
    bench_array_construction,
    bench_int_pow_fastpath,
    bench_float_sqrt_fastpath,
);
