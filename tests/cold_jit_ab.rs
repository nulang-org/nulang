#[cfg(feature = "native-codegen")]
mod benchmarks {
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    use nulang::bytecode::CodeModule;
    use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
    use nulang::lexer::Lexer;
    use nulang::parser::Parser;
    use nulang::typechecker::TypeChecker;
    use nulang::vm::VM;

    const REPEATS: usize = 200;

    fn compile(source: &str) -> CodeModule {
        let tokens = Lexer::new(source).lex().expect("bench: lex failed");
        let ast = Parser::new(tokens)
            .parse_module()
            .expect("bench: parse failed");
        let mut type_checker = TypeChecker::new();
        type_checker
            .check_module(&ast)
            .expect("bench: typecheck failed");
        let mut effect_checker = EffectChecker::new();
        effect_checker
            .check_module(&ast.decls)
            .expect("bench: effect check failed");
        let mut cap_analyzer = CapabilityAnalyzer::new();
        let cap_ctx = CapContext::new();
        for decl in nulang::effect_checker::flatten_decls(&ast.decls) {
            if let nulang::ast::Decl::Function { body, .. } = decl {
                cap_analyzer
                    .infer_cap(&cap_ctx, body)
                    .expect("bench: capability check failed");
            }
        }
        let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = nulang::mir_lower::lower_module(&hir).expect("bench: MIR lower failed");
        nulang::mir_codegen::compile_mir(&mut mir, "bench-ab-cold-jit")
            .expect("bench: codegen failed")
    }

    fn report_ab(name: &str, operations: u64, elapsed: Duration) {
        println!(
            "[ab-bench] benchmark={name} operations={operations} elapsed_ns={}",
            elapsed.as_nanos()
        );
    }

    fn timed_run(vm: &mut VM, failure: &str) -> (Option<i64>, Duration) {
        let started = Instant::now();
        let result = black_box(vm.run().expect(failure));
        (result.as_int(), started.elapsed())
    }

    #[test]
    fn bench_ab_interp_cold_jit_probe() {
        // Match benches/interp_bench.rs exactly. The loop remains below the
        // current HOT_THRESHOLD, so a JIT-enabled VM should pay only cold
        // candidate/hotness-probe overhead and compile no native region.
        let source =
            "var sum = 0; var i = 0; while i < 500 { sum = sum + i * 2 - i / 3; i = i + 1; }; sum";
        let module = compile(source);

        let mut interp_elapsed = Duration::ZERO;
        let mut jit_elapsed = Duration::ZERO;
        let mut expected_result = None;

        // Counterbalance execution order on every repetition. This keeps
        // thermal/frequency/cache drift from being systematically attributed
        // to either JIT-off or JIT-on when the effect under test is small.
        for repetition in 0..REPEATS {
            let mut interp_vm = VM::new_without_jit();
            interp_vm.load_module(module.clone());
            let mut jit_vm = VM::new();
            jit_vm.load_module(module.clone());

            let (interp_result, jit_result, interp_sample, jit_sample) =
                if repetition % 2 == 0 {
                    let (interp_result, interp_sample) =
                        timed_run(&mut interp_vm, "bench: cold interpreter run failed");
                    let (jit_result, jit_sample) =
                        timed_run(&mut jit_vm, "bench: cold JIT-enabled run failed");
                    (interp_result, jit_result, interp_sample, jit_sample)
                } else {
                    let (jit_result, jit_sample) =
                        timed_run(&mut jit_vm, "bench: cold JIT-enabled run failed");
                    let (interp_result, interp_sample) =
                        timed_run(&mut interp_vm, "bench: cold interpreter run failed");
                    (interp_result, jit_result, interp_sample, jit_sample)
                };

            assert_eq!(
                interp_result, jit_result,
                "cold JIT probe must preserve interpreter semantics"
            );
            assert_eq!(
                jit_vm.jit_compiled_count(),
                0,
                "sub-threshold cold probe must not compile a JIT region"
            );

            if let Some(previous) = expected_result {
                assert_eq!(
                    interp_result,
                    Some(previous),
                    "cold probe result must be stable across repetitions"
                );
            } else {
                expected_result = interp_result;
            }

            interp_elapsed += interp_sample;
            jit_elapsed += jit_sample;
        }

        report_ab("interp_cold_jit_off", REPEATS as u64, interp_elapsed);
        report_ab("interp_cold_jit_on", REPEATS as u64, jit_elapsed);
    }
}
