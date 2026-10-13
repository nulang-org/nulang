//! Compiler-stage timing probe for the isolated-module Nulang bytecode pipeline.
//!
//! This deliberately excludes CLI startup, prelude injection, import resolution,
//! native linking, execution, and JIT compilation. It is not an end-to-end
//! `nulang` or apples-to-apples Go build benchmark.

use std::env;
use std::process::ExitCode;
use std::time::Instant;

use nulang::ast::Decl;
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use serde_json::{json, Value};

const WORKLOADS: &[&str] = &["literal", "functions_10", "functions_100", "branch_loop"];

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

#[derive(Debug, Default)]
struct Timings {
    lex_ns: u128,
    parse_ns: u128,
    typecheck_ns: u128,
    effectcheck_ns: u128,
    capability_ns: u128,
    hir_ns: u128,
    mir_ns: u128,
    bytecode_ns: u128,
    total_ns: u128,
}

impl Timings {
    fn stage_sum_ns(&self) -> u128 {
        self.lex_ns
            + self.parse_ns
            + self.typecheck_ns
            + self.effectcheck_ns
            + self.capability_ns
            + self.hir_ns
            + self.mir_ns
            + self.bytecode_ns
    }
}

fn functions_source(count: usize) -> String {
    let mut source = String::new();
    for i in 0..count {
        source.push_str(&format!("fn f{i}(x: Int) -> Int {{ x + {i} }}; "));
    }
    source.push_str(&format!("f{}(1)", count - 1));
    source
}

fn workload_source(name: &str) -> String {
    match name {
        "literal" => "42".to_string(),
        "functions_10" => functions_source(10),
        "functions_100" => functions_source(100),
        "branch_loop" => {
            "var sum = 0; var i = 0; while i < 100000 { if i < 50000 then { sum = sum + i } else { sum = sum - 1 }; i = i + 1; }; sum".to_string()
        }
        _ => unreachable!("workload names are validated before use"),
    }
}

fn measure(source: &str) -> Result<Timings, String> {
    let total = Instant::now();
    let mut times = Timings::default();

    let started = Instant::now();
    let tokens = Lexer::new(source)
        .lex()
        .map_err(|err| format!("lex: {err}"))?;
    times.lex_ns = started.elapsed().as_nanos();

    let started = Instant::now();
    let ast = Parser::new(tokens)
        .parse_module()
        .map_err(|err| format!("parse: {err}"))?;
    times.parse_ns = started.elapsed().as_nanos();

    let started = Instant::now();
    let mut checker = TypeChecker::new();
    checker
        .check_module(&ast)
        .map_err(|err| format!("typecheck: {err}"))?;
    times.typecheck_ns = started.elapsed().as_nanos();

    let started = Instant::now();
    let mut effect_checker = EffectChecker::new();
    effect_checker
        .check_module(&ast.decls)
        .map_err(|err| format!("effectcheck: {err}"))?;
    times.effectcheck_ns = started.elapsed().as_nanos();

    // Mirror capability checks for top-level functions in the CLI frontend.
    // The benchmark fixtures intentionally contain no actors or workflows.
    let started = Instant::now();
    let mut analyzer = CapabilityAnalyzer::new();
    for decl in &ast.decls {
        if let Decl::Function { body, params, .. } = decl {
            let mut ctx = CapContext::new();
            for param in params {
                if let Some(cap) = param.cap {
                    ctx = ctx.with_binding(&param.name, cap);
                }
            }
            analyzer
                .infer_cap(&ctx, body)
                .map_err(|err| format!("capability: {err}"))?;
        }
    }
    times.capability_ns = started.elapsed().as_nanos();

    let started = Instant::now();
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    times.hir_ns = started.elapsed().as_nanos();

    let started = Instant::now();
    let mut mir = nulang::mir_lower::lower_module(&hir)
        .map_err(|err| format!("MIR: {err}"))?;
    times.mir_ns = started.elapsed().as_nanos();

    let started = Instant::now();
    let module = nulang::mir_codegen::compile_mir(&mut mir, "compile-bench")
        .map_err(|err| format!("bytecode: {err}"))?;
    times.bytecode_ns = started.elapsed().as_nanos();
    std::hint::black_box(module);

    times.total_ns = total.elapsed().as_nanos();
    Ok(times)
}

fn json_record(workload: &str, iteration: u32, source_bytes: usize, times: &Timings) -> Value {
    json!({
        "schema": 1,
        "workload": workload,
        "iteration": iteration,
        "backend": "bytecode",
        "scope": "isolated_module",
        "source_bytes": source_bytes,
        "total_ns": times.total_ns,
        "stage_sum_ns": times.stage_sum_ns(),
        "stages_ns": {
            "lex": times.lex_ns,
            "parse": times.parse_ns,
            "typecheck": times.typecheck_ns,
            "effectcheck": times.effectcheck_ns,
            "capability": times.capability_ns,
            "hir": times.hir_ns,
            "mir": times.mir_ns,
            "bytecode": times.bytecode_ns,
        },
    })
}

fn emit(
    workload: &str,
    iteration: u32,
    source_bytes: usize,
    times: &Timings,
    format: OutputFormat,
) {
    match format {
        OutputFormat::Jsonl => {
            println!("{}", json_record(workload, iteration, source_bytes, times));
        }
        OutputFormat::Human => {
            println!(
                "{workload} run={iteration} bytes={source_bytes} total={:.3}ms lex={:.3}ms parse={:.3}ms type={:.3}ms effects={:.3}ms caps={:.3}ms HIR={:.3}ms MIR={:.3}ms bytecode={:.3}ms",
                times.total_ns as f64 / 1_000_000.0,
                times.lex_ns as f64 / 1_000_000.0,
                times.parse_ns as f64 / 1_000_000.0,
                times.typecheck_ns as f64 / 1_000_000.0,
                times.effectcheck_ns as f64 / 1_000_000.0,
                times.capability_ns as f64 / 1_000_000.0,
                times.hir_ns as f64 / 1_000_000.0,
                times.mir_ns as f64 / 1_000_000.0,
                times.bytecode_ns as f64 / 1_000_000.0,
            );
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut format = OutputFormat::Human;
    let mut workload = None;
    let mut repeat = 5;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--format" => {
                format = match args.next().as_deref() {
                    Some("human") => OutputFormat::Human,
                    Some("jsonl") => OutputFormat::Jsonl,
                    _ => return Err("--format requires human or jsonl".to_string()),
                };
            }
            "--workload" => {
                workload = Some(
                    args.next()
                        .ok_or_else(|| "--workload requires a name".to_string())?,
                );
            }
            "--repeat" => {
                repeat = args
                    .next()
                    .ok_or_else(|| "--repeat requires a positive integer".to_string())?
                    .parse::<u32>()
                    .map_err(|_| "--repeat requires a positive integer".to_string())?;
                if repeat == 0 {
                    return Err("--repeat must be at least 1".to_string());
                }
            }
            "--list" => {
                for name in WORKLOADS {
                    println!("{name}");
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
    if let Some(ref name) = workload {
        if !WORKLOADS.contains(&name.as_str()) {
            return Err(format!("unknown workload: {name}"));
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
        "Usage: nulang-compile-bench [--format human|jsonl] [--workload NAME] [--repeat N] [--list]"
    );
}

fn main() -> ExitCode {
    let config = match parse_args(env::args().skip(1)) {
        Ok(Some(config)) => config,
        Ok(None) => return ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            print_usage();
            return ExitCode::from(2);
        }
    };
    for workload in WORKLOADS {
        if config
            .workload
            .as_deref()
            .is_some_and(|name| name != *workload)
        {
            continue;
        }
        let source = workload_source(workload);
        for iteration in 1..=config.repeat {
            match measure(&source) {
                Ok(times) => emit(workload, iteration, source.len(), &times, config.format),
                Err(err) => {
                    eprintln!("{workload} run={iteration} failed: {err}");
                    return ExitCode::FAILURE;
                }
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fixture_compiles_and_accounts_for_every_stage() {
        for workload in WORKLOADS {
            let times = measure(&workload_source(workload))
                .unwrap_or_else(|err| panic!("{workload} failed to compile: {err}"));
            assert!(
                times.total_ns >= times.stage_sum_ns(),
                "{workload} must include all stages in end-to-end measurement"
            );
        }
    }

    #[test]
    fn jsonl_schema_separates_stages_and_total() {
        let times = measure(&workload_source("literal")).expect("literal compiles");
        let record = json_record("literal", 1, 2, &times);
        assert_eq!(record["schema"], 1);
        assert_eq!(record["scope"], "isolated_module");
        assert_eq!(record["backend"], "bytecode");
        assert_eq!(record["source_bytes"], 2);
        for stage in [
            "lex", "parse", "typecheck", "effectcheck", "capability", "hir", "mir",
            "bytecode",
        ] {
            assert!(record["stages_ns"][stage].is_number(), "missing stage {stage}");
        }
    }

    #[test]
    fn rejects_invalid_workloads_and_zero_repetitions() {
        let workload_args = ["--workload", "not-a-workload"].into_iter().map(str::to_string);
        let repeat_args = ["--repeat", "0"].into_iter().map(str::to_string);
        assert!(parse_args(workload_args).is_err());
        assert!(parse_args(repeat_args).is_err());
    }

    #[test]
    fn identical_fixture_inputs_are_deterministic() {
        assert_eq!(
            workload_source("functions_100"),
            workload_source("functions_100")
        );
        assert_ne!(
            workload_source("functions_10"),
            workload_source("functions_100")
        );
    }
}
