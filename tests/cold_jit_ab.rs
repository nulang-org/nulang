#[cfg(feature = "native-codegen")]
mod benchmarks {
    use std::hint::black_box;
    use std::time::Instant;

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

    fn report_ab(name: &str, operations: u64, elapsed: std::time::Duration) {
        println!(
            "[ab-bench] benchmark={name} operations={operations} elapsed_ns={}",
            elapsed.as_nanos()
        );
    }

    #[test]
    fn bench_ab_interp_cold_jit_probe() {
        let source =
            "var sum = 0; var i = 0; while i < 500 { sum = sum + i * 2 - i / 3; i = i + 1; }; sum";
        let module = compile(source);

        let mut interp_vms: Vec<VM> = (0..REPEATS)
            .map(|_| {
                let mut vm = VM::new_without_jit();
                vm.load_module(module.clone());
                vm
            })
            .collect();
        let interp_start = Instant::now();
        let mut interp_result = None;
        for vm in &mut interp_vms {
            interp_result = Some(black_box(
                vm.run().expect("bench: cold interpreter run failed"),
            ));
        }
        let interp_elapsed = interp_start.elapsed();
        report_ab("interp_cold_jit_off", REPEATS as u64, interp_elapsed);

        let mut jit_vms: Vec<VM> = (0..REPEATS)
            .map(|_| {
                let mut vm = VM::new();
                vm.load_module(module.clone());
                vm
            })
            .collect();
        let jit_start = Instant::now();
        let mut jit_result = None;
        for vm in &mut jit_vms {
            jit_result = Some(black_box(
                vm.run().expect("bench: cold JIT-enabled run failed"),
            ));
        }
        let jit_elapsed = jit_start.elapsed();

        assert_eq!(
            interp_result.and_then(|value| value.as_int()),
            jit_result.and_then(|value| value.as_int()),
            "cold JIT probe must preserve interpreter semantics"
        );
        for vm in &jit_vms {
            assert_eq!(
                vm.jit_compiled_count(),
                0,
                "sub-threshold cold probe must not compile a JIT region"
            );
        }
        report_ab("interp_cold_jit_on", REPEATS as u64, jit_elapsed);
    }
}
