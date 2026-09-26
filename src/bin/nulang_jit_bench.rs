//! Machine-readable JIT compilation benchmark runner.
//!
//! This complements Criterion by separating compiler wall time from end-to-end
//! execution time. It deliberately uses the public VM/JitBackend telemetry so
//! alternative native backends can be compared without changing the harness.

use std::env;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nulang::backends::JitCompileStats;
use nulang::bytecode::CodeModule;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;
use serde_json::json;

#[derive(Clone, Copy)]
enum OutputFormat {
    Human,
    Jsonl,
}

struct Config {
    format: OutputFormat,
    workload: Option<String>,
    repeat: u32,
}

#[derive(Clone, Copy)]
struct Workload {
    name: &'static str,
    source: &'static str,
}

const WORKLOADS: &[Workload] = &[
    Workload {
        name: "numeric_loop",
        source: "var sum = 0; var i = 0; while i < 100000 { sum = sum + i * 3 - i / 7; i = i + 1; }; sum",
    },
    Workload {
        name: "call_loop",
        source: "fn add(x: Int, y: Int) -> Int { x + y }; var sum = 0; var i = 0; while i < 50000 { sum = add(sum, i); i = i + 1; }; sum",
    },
    Workload {
        name: "branch_loop",
        source: "var sum = 0; var i = 0; while i < 100000 { if i < 50000 then { sum = sum + i } else { sum = sum - 1 }; i = i + 1; }; sum",
    },
];

struct Measurement {
    workload: &'static str,
    first_run: Duration,
    warm_run: Duration,
    first_stats: JitCompileStats,
    warm_stats: JitCompileStats,
    compiled_regions: usize,
    typed_regions: usize,
}

fn compile(source: &str) -> CodeModule {
    let tokens = Lexer::new(source).lex().expect("bench: lex failed");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("bench: parse failed");
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .expect("bench: typecheck failed");
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("bench: MIR lower failed");
    nulang::mir_codegen::compile_mir(&mut mir, "jit-codegen-bench")
        .expect("bench: bytecode codegen failed")
}

fn run_interpreter(module: &CodeModule) -> u64 {
    let mut vm = VM::new_without_jit();
    vm.load_module(module.clone());
    vm.run().expect("interpreter benchmark run failed").as_raw()
}

fn measure(workload: Workload, module: &CodeModule, expected_raw: u64) -> Measurement {
    let mut vm = VM::new();
    vm.load_module(module.clone());

    let first_started = Instant::now();
    let first_result = vm.run().expect("first JIT benchmark run failed");
    let first_run = first_started.elapsed();
    assert_eq!(
        first_result.as_raw(),
        expected_raw,
        "{} first JIT run diverged from interpreter",
        workload.name
    );
    let first_stats = vm.jit_compile_stats();

    let warm_started = Instant::now();
    let warm_result = vm.run().expect("warm JIT benchmark run failed");
    let warm_run = warm_started.elapsed();
    assert_eq!(
        warm_result.as_raw(),
        expected_raw,
        "{} warm JIT run diverged from interpreter",
        workload.name
    );
    let warm_stats = vm.jit_compile_stats();

    Measurement {
        workload: workload.name,
        first_run,
        warm_run,
        first_stats,
        warm_stats,
        compiled_regions: vm.jit_compiled_count(),
        typed_regions: vm.jit_typed_compiled_count(),
    }
}

fn delta(after: JitCompileStats, before: JitCompileStats) -> JitCompileStats {
    JitCompileStats {
        fast_compiles: after.fast_compiles.saturating_sub(before.fast_compiles),
        fast_compile_ns: after.fast_compile_ns.saturating_sub(before.fast_compile_ns),
        optimized_compiles: after
            .optimized_compiles
            .saturating_sub(before.optimized_compiles),
        optimized_compile_ns: after
            .optimized_compile_ns
            .saturating_sub(before.optimized_compile_ns),
    }
}

fn emit(measurement: &Measurement, iteration: u32, format: OutputFormat) {
    let warm_delta = delta(measurement.warm_stats, measurement.first_stats);
    match format {
        OutputFormat::Human => {
            println!(
                "[jit-codegen] {} iteration={} first={:.3}ms warm={:.3}ms regions={} typed={}",
                measurement.workload,
                iteration,
                measurement.first_run.as_secs_f64() * 1_000.0,
                measurement.warm_run.as_secs_f64() * 1_000.0,
                measurement.compiled_regions,
                measurement.typed_regions,
            );
            println!(
                "  first compile: fast={} ({:.3}ms) optimized={} ({:.3}ms)",
                measurement.first_stats.fast_compiles,
                measurement.first_stats.fast_compile_ns as f64 / 1_000_000.0,
                measurement.first_stats.optimized_compiles,
                measurement.first_stats.optimized_compile_ns as f64 / 1_000_000.0,
            );
            println!(
                "  warm delta: fast={} ({:.3}ms) optimized={} ({:.3}ms)",
                warm_delta.fast_compiles,
                warm_delta.fast_compile_ns as f64 / 1_000_000.0,
                warm_delta.optimized_compiles,
                warm_delta.optimized_compile_ns as f64 / 1_000_000.0,
            );
        }
        OutputFormat::Jsonl => {
            println!(
                "{}",
                json!({
                    "schema": 1,
                    "runtime": "nulang",
                    "suite": "jit-codegen",
                    "workload": measurement.workload,
                    "iteration": iteration,
                    "first_run_ns": u64::try_from(measurement.first_run.as_nanos())
                        .expect("first run duration must fit in u64"),
                    "warm_run_ns": u64::try_from(measurement.warm_run.as_nanos())
                        .expect("warm run duration must fit in u64"),
                    "compiled_regions": measurement.compiled_regions,
                    "typed_regions": measurement.typed_regions,
                    "first_compile": {
                        "fast_compiles": measurement.first_stats.fast_compiles,
                        "fast_compile_ns": measurement.first_stats.fast_compile_ns,
                        "optimized_compiles": measurement.first_stats.optimized_compiles,
                        "optimized_compile_ns": measurement.first_stats.optimized_compile_ns,
                        "total_compiles": measurement.first_stats.total_compiles(),
                        "total_compile_ns": measurement.first_stats.total_compile_ns(),
                    },
                    "warm_compile_delta": {
                        "fast_compiles": warm_delta.fast_compiles,
                        "fast_compile_ns": warm_delta.fast_compile_ns,
                        "optimized_compiles": warm_delta.optimized_compiles,
                        "optimized_compile_ns": warm_delta.optimized_compile_ns,
                        "total_compiles": warm_delta.total_compiles(),
                        "total_compile_ns": warm_delta.total_compile_ns(),
                    },
                })
            );
        }
    }
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut format = OutputFormat::Human;
    let mut workload = None;
    let mut repeat = 1u32;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--format" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--format requires human or jsonl".to_string())?;
                format = match value.as_str() {
                    "human" => OutputFormat::Human,
                    "jsonl" => OutputFormat::Jsonl,
                    _ => return Err(format!("unsupported format: {value}")),
                };
            }
            "--workload" => {
                workload = Some(
                    args.next()
                        .ok_or_else(|| "--workload requires a name".to_string())?,
                );
            }
            "--repeat" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--repeat requires a positive integer".to_string())?;
                repeat = value
                    .parse::<u32>()
                    .map_err(|_| format!("invalid repeat count: {value}"))?;
                if repeat == 0 {
                    return Err("--repeat must be at least 1".to_string());
                }
            }
            "--list" => {
                for workload in WORKLOADS {
                    println!("{}", workload.name);
                }
                return Ok(None);
            }
            "-h" | "--help" => {
                print_usage();
                return Ok(None);
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    if let Some(selected) = workload.as_deref() {
        if !WORKLOADS.iter().any(|candidate| candidate.name == selected) {
            return Err(format!("unknown workload: {selected}"));
        }
    }

    Ok(Some(Config {
        format,
        workload,
        repeat,
    }))
}

fn print_usage() {
    eprintln!(
        "Usage: nulang-jit-bench [--format human|jsonl] [--workload NAME] [--repeat N] [--list]"
    );
}

fn main() -> ExitCode {
    let config = match parse_args() {
        Ok(Some(config)) => config,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    for workload in WORKLOADS {
        if config
            .workload
            .as_deref()
            .is_some_and(|selected| selected != workload.name)
        {
            continue;
        }

        let module = compile(workload.source);
        let expected_raw = run_interpreter(&module);

        for iteration in 1..=config.repeat {
            let measurement = measure(*workload, &module, expected_raw);
            emit(&measurement, iteration, config.format);
        }
    }

    ExitCode::SUCCESS
}
