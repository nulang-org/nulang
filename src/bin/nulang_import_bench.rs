//! In-process warm build benchmark for imported-module token reuse.
//!
//! To compare fairly, run the *same* binary with and without
//! NULANG_DISABLE_IMPORT_TOKEN_CACHE=1 on the same machine.
//! This creates a fresh temporary import graph, then compiles it repeatedly
//! in one process. Process launch, fixture I/O, and reporting are excluded
//! from timing, but imported-source reads and compiler work are included.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nulang::ast::Decl;
use nulang::effect_checker::{CapContext, CapabilityAnalyzer, EffectChecker};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;
use serde_json::json;

struct Config {
    modules: usize,
    functions_per_module: usize,
    warmup: usize,
    repeat: usize,
    jsonl: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            modules: 8,
            functions_per_module: 16,
            warmup: 5,
            repeat: 20,
            jsonl: false,
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut config = Config::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--modules" | "--functions" | "--warmup" | "--repeat" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a value"))?
                    .parse::<usize>()
                    .map_err(|_| format!("{arg} requires a nonnegative integer"))?;
                match arg.as_str() {
                    "--modules" if (1..=32).contains(&value) => config.modules = value,
                    "--functions" if (1..=64).contains(&value) => {
                        config.functions_per_module = value;
                    }
                    "--warmup" if value <= 1000 => config.warmup = value,
                    "--repeat" if (1..=1000).contains(&value) => config.repeat = value,
                    _ => return Err(format!("{arg} value outside supported range")),
                }
            }
            "--format" => {
                config.jsonl = match args.next().as_deref() {
                    Some("jsonl") => true,
                    Some("human") => false,
                    _ => return Err("--format requires human or jsonl".into()),
                };
            }
            "-h" | "--help" => {
                usage();
                return Ok(None);
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    Ok(Some(config))
}

fn usage() {
    eprintln!(
        "Usage: nulang-import-bench [--modules 1..32] [--functions 1..64] \
         [--warmup 0..1000] [--repeat 1..1000] [--format human|jsonl]"
    );
}

struct Fixture {
    dir: PathBuf,
    source: String,
    expected_functions: usize,
}

impl Fixture {
    fn create(modules: usize, functions_per_module: usize) -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let dir = env::temp_dir().join(format!("nulang-import-bench-{}-{nonce}", std::process::id()));
        fs::create_dir(&dir).map_err(|e| format!("cannot create fixture: {e}"))?;

        // One unchanged parent repeatedly resolves multiple imported leaves.
        // Per-leaf functions are uniquely named to avoid collision shortcuts.
        let mut parent = String::new();
        for module_idx in 0..modules {
            parent.push_str(&format!("import dep{module_idx}\n"));
            let mut leaf = String::new();
            for function_idx in 0..functions_per_module {
                leaf.push_str(&format!(
                    "fn f{module_idx}_{function_idx}(x: Int) -> Int {{ x + {function_idx} }}\n"
                ));
            }
            fs::write(dir.join(format!("dep{module_idx}.nula")), leaf)
                .map_err(|e| format!("cannot write fixture dependency: {e}"))?;
        }
        fs::write(dir.join("parent.nula"), parent)
            .map_err(|e| format!("cannot write parent: {e}"))?;
        Ok(Self {
            dir,
            source: "import parent\nf0_0(1)\n".into(),
            expected_functions: modules * functions_per_module,
        })
    }

    fn compile(&self) -> Result<(Duration, usize), String> {
        let started = Instant::now();
        let tokens = Lexer::new(&self.source)
            .lex()
            .map_err(|e| format!("lex: {e}"))?;
        let mut ast = Parser::new(tokens)
            .parse_module()
            .map_err(|e| format!("parse: {e}"))?;
        nulang::resolver::resolve_imports(
            &mut ast,
            &self.dir.join("main.nula"),
            &mut HashSet::new(),
        )
        .map_err(|e| format!("resolve: {e}"))?;

        let function_count = ast
            .decls
            .iter()
            .filter(|decl| matches!(decl, Decl::Function { .. }))
            .count();
        if function_count != self.expected_functions {
            return Err(format!(
                "resolved {function_count} functions, expected {}",
                self.expected_functions
            ));
        }

        let mut checker = TypeChecker::new();
        checker
            .check_module(&ast)
            .map_err(|e| format!("typecheck: {e}"))?;
        let mut effect_checker = EffectChecker::new();
        effect_checker
            .check_module(&ast.decls)
            .map_err(|e| format!("effectcheck: {e}"))?;
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
                    .map_err(|e| format!("capability: {e}"))?;
            }
        }

        let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
        let mut mir = nulang::mir_lower::lower_module(&hir)
            .map_err(|e| format!("MIR: {e}"))?;
        let bytecode = nulang::mir_codegen::compile_mir(&mut mir, "import-bench")
            .map_err(|e| format!("bytecode: {e}"))?;
        std::hint::black_box(&bytecode);
        let elapsed = started.elapsed();
        Ok((elapsed, function_count))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn run(config: &Config) -> Result<(), String> {
    let fixture = Fixture::create(config.modules, config.functions_per_module)?;
    let cache_disabled =
        env::var("NULANG_DISABLE_IMPORT_TOKEN_CACHE").ok().as_deref() == Some("1");
    for _ in 0..config.warmup {
        fixture.compile()?;
    }
    for iteration in 1..=config.repeat {
        let (elapsed, functions) = fixture.compile()?;
        if config.jsonl {
            println!(
                "{}",
                json!({
                    "schema": 1,
                    "workload": "transitive_imports",
                    "cache": if cache_disabled { "disabled" } else { "enabled" },
                    "iteration": iteration,
                    "modules": config.modules,
                    "functions_per_module": config.functions_per_module,
                    "resolved_functions": functions,
                    "compile_ns": elapsed.as_nanos(),
                    "scope": "in_process_import_to_bytecode",
                })
            );
        } else {
            println!(
                "cache={} iteration={iteration} modules={} functions={} compile_ms={:.3}",
                if cache_disabled { "off" } else { "on" },
                config.modules,
                functions,
                elapsed.as_secs_f64() * 1000.0
            );
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let config = match parse_args(env::args().skip(1)) {
        Ok(Some(config)) => config,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            usage();
            return ExitCode::from(2);
        }
    };
    if let Err(error) = run(&config) {
        eprintln!("benchmark failed: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_transitive_import_graph_compiles() {
        let fixture = Fixture::create(2, 3).expect("create graph");
        let (_, functions) = fixture.compile().expect("compile full graph");
        assert_eq!(functions, 6);
    }

    #[test]
    fn repeat_builds_produce_identical_exported_function_counts() {
        let fixture = Fixture::create(2, 2).expect("create graph");
        let first = fixture.compile().expect("first compile");
        let second = fixture.compile().expect("second compile");
        assert_eq!(first.1, second.1);
    }

    #[test]
    fn invalid_flags_fail_before_benchmarking() {
        assert!(parse_args(["--repeat", "0"].map(str::to_string)).is_err());
        assert!(parse_args(["--modules", "33"].map(str::to_string)).is_err());
        assert!(parse_args(["--functions", "0"].map(str::to_string)).is_err());
        assert!(parse_args(["--format", "xml"].map(str::to_string)).is_err());
    }
}
