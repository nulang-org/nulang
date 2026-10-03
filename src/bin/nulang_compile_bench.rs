//! Machine-readable clean compiler-pipeline benchmark.
//!
//! Measures the language frontend separately from native tiering so Nulang can
//! track its "Go-like development compile speed" goal with phase-level data.
//! The benchmark intentionally performs a fresh lex/parse/check/lower/codegen
//! pipeline on every iteration; incremental/cache benchmarks should be tracked
//! separately rather than conflated with clean compilation.

use std::env;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
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
        name: "tiny",
        source: "fn add(x: Int, y: Int) -> Int { x + y }; add(20, 22)",
    },
    Workload {
        name: "numeric",
        source: "fn mix(x: Int, y: Int) -> Int { (x * 3 + y * 5) - (x / 7) }; var sum = 0; var i = 0; while i < 1000 { sum = mix(sum, i); i = i + 1; }; sum",
    },
    Workload {
        name: "actor",
        source: r#"
            actor Counter {
                state count: Int = 0
                behavior add(n: Int) { self.count = self.count + n }
                behavior get() { self.count }
            }
            fn twice(x: Int) -> Int { x * 2 }
            twice(21)
        "#,
    },
];

#[derive(Debug, Clone, Copy, Default)]
struct PhaseTimes {
    lex: Duration,
    parse: Duration,
    typecheck: Duration,
    effect_check: Duration,
    capability_check: Duration,
    hir_lower: Duration,
    mir_lower: Duration,
    bytecode_codegen: Duration,
    total: Duration,
}

impl PhaseTimes {
    fn accounted(self) -> Duration {
        self.lex
            + self.parse
            + self.typecheck
            + self.effect_check
            + self.capability_check
            + self.hir_lower
            + self.mir_lower
            + self.bytecode_codegen
    }
}

struct Measurement {
    workload: &'static str,
    source_bytes: usize,
    instructions: usize,
    capability_bodies: usize,
    phases: PhaseTimes,
}

fn elapsed_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Run the capability phase for the declaration shapes exercised by this
/// benchmark suite and return the number of expression bodies visited.
///
/// This intentionally mirrors the CLI's function/actor capability work rather
/// than timing only top-level functions. Actor workloads therefore include
/// behavior bodies, state defaults, and explicit init expressions.
fn check_capabilities(decls: &[nulang::ast::Decl]) -> usize {
    let mut analyzer = CapabilityAnalyzer::new();
    let base_ctx = CapContext::new();
    let mut bodies = 0usize;

    for decl in nulang::effect_checker::flatten_decls(decls) {
        match decl {
            nulang::ast::Decl::Function { body, params, .. } => {
                let ctx = base_ctx.with_params(params);
                analyzer
                    .infer_cap(&ctx, body)
                    .expect("compile bench: capability check failed");
                bodies += 1;
            }
            nulang::ast::Decl::Actor {
                behaviors,
                state_fields,
                init,
                ..
            } => {
                for behavior in behaviors {
                    let ctx = base_ctx.with_params(&behavior.params);
                    analyzer
                        .infer_cap(&ctx, &behavior.body)
                        .expect("compile bench: actor behavior capability check failed");
                    bodies += 1;
                }
                for (_, _, _, default) in state_fields {
                    analyzer
                        .infer_cap(&base_ctx, default)
                        .expect("compile bench: actor state capability check failed");
                    bodies += 1;
                }
                for (_, expr) in init {
                    analyzer
                        .infer_cap(&base_ctx, expr)
                        .expect("compile bench: actor init capability check failed");
                    bodies += 1;
                }
            }
            _ => {}
        }
    }

    bodies
}

fn measure_compile(workload: Workload) -> Measurement {
    let total_started = Instant::now();

    let started = Instant::now();
    let tokens = Lexer::new(workload.source)
        .lex()
        .expect("compile bench: lex failed");
    let lex = started.elapsed();

    let started = Instant::now();
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("compile bench: parse failed");
    let parse = started.elapsed();

    let started = Instant::now();
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .expect("compile bench: typecheck failed");
    let typecheck = started.elapsed();

    let started = Instant::now();
    let mut effect_checker = EffectChecker::new();
    effect_checker
        .check_module(&ast.decls)
        .expect("compile bench: effect check failed");
    let effect_check = started.elapsed();

    let started = Instant::now();
    let capability_bodies = check_capabilities(&ast.decls);
    let capability_check = started.elapsed();

    let started = Instant::now();
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let hir_lower = started.elapsed();

    let started = Instant::now();
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("compile bench: MIR lower failed");
    let mir_lower = started.elapsed();

    let started = Instant::now();
    let module = nulang::mir_codegen::compile_mir(&mut mir, "compile-bench")
        .expect("compile bench: bytecode codegen failed");
    let bytecode_codegen = started.elapsed();

    let total = total_started.elapsed();

    Measurement {
        workload: workload.name,
        source_bytes: workload.source.len(),
        instructions: module.instructions.len(),
        capability_bodies,
        phases: PhaseTimes {
            lex,
            parse,
            typecheck,
            effect_check,
            capability_check,
            hir_lower,
            mir_lower,
            bytecode_codegen,
            total,
        },
    }
}

fn emit(measurement: &Measurement, iteration: u32, format: OutputFormat) {
    let p = measurement.phases;
    match format {
        OutputFormat::Human => {
            println!(
                "[compile] {} iteration={} source={}B bytecode={} cap_bodies={} total={:.3}ms accounted={:.3}ms",
                measurement.workload,
                iteration,
                measurement.source_bytes,
                measurement.instructions,
                measurement.capability_bodies,
                p.total.as_secs_f64() * 1_000.0,
                p.accounted().as_secs_f64() * 1_000.0,
            );
            println!(
                "  lex={:.3} parse={:.3} typecheck={:.3} effects={:.3} caps={:.3} hir={:.3} mir={:.3} bytecode={:.3} ms",
                p.lex.as_secs_f64() * 1_000.0,
                p.parse.as_secs_f64() * 1_000.0,
                p.typecheck.as_secs_f64() * 1_000.0,
                p.effect_check.as_secs_f64() * 1_000.0,
                p.capability_check.as_secs_f64() * 1_000.0,
                p.hir_lower.as_secs_f64() * 1_000.0,
                p.mir_lower.as_secs_f64() * 1_000.0,
                p.bytecode_codegen.as_secs_f64() * 1_000.0,
            );
        }
        OutputFormat::Jsonl => {
            println!(
                "{}",
                json!({
                    "schema": 2,
                    "runtime": "nulang",
                    "suite": "clean-compile",
                    "workload": measurement.workload,
                    "iteration": iteration,
                    "source_bytes": measurement.source_bytes,
                    "bytecode_instructions": measurement.instructions,
                    "capability_bodies": measurement.capability_bodies,
                    "total_ns": elapsed_ns(p.total),
                    "accounted_ns": elapsed_ns(p.accounted()),
                    "phases": {
                        "lex_ns": elapsed_ns(p.lex),
                        "parse_ns": elapsed_ns(p.parse),
                        "typecheck_ns": elapsed_ns(p.typecheck),
                        "effect_check_ns": elapsed_ns(p.effect_check),
                        "capability_check_ns": elapsed_ns(p.capability_check),
                        "hir_lower_ns": elapsed_ns(p.hir_lower),
                        "mir_lower_ns": elapsed_ns(p.mir_lower),
                        "bytecode_codegen_ns": elapsed_ns(p.bytecode_codegen),
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
        "Usage: nulang_compile_bench [--format human|jsonl] [--workload NAME] [--repeat N] [--list]"
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

        for iteration in 1..=config.repeat {
            let measurement = measure_compile(*workload);
            emit(&measurement, iteration, config.format);
        }
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benchmark_covers_distinct_frontend_shapes() {
        let names: Vec<_> = WORKLOADS.iter().map(|workload| workload.name).collect();
        assert_eq!(names, vec!["tiny", "numeric", "actor"]);
    }

    #[test]
    fn actor_workload_includes_actor_capability_bodies() {
        let actor = WORKLOADS
            .iter()
            .find(|workload| workload.name == "actor")
            .copied()
            .expect("actor benchmark workload");
        let tokens = Lexer::new(actor.source).lex().expect("actor lex");
        let ast = Parser::new(tokens).parse_module().expect("actor parse");

        assert!(
            check_capabilities(&ast.decls) >= 4,
            "actor workload must time function, behavior, and state capability analysis"
        );
    }

    #[test]
    fn nanoseconds_conversion_saturates_instead_of_panicking() {
        assert_eq!(elapsed_ns(Duration::ZERO), 0);
    }
}
