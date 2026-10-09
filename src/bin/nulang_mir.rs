//! Inspect Nulang MIR before and after the existing optimization pipeline.
//!
//! This tool intentionally goes through the same frontend and `compile_mir`
//! entry point as normal bytecode compilation so the optimized view reflects
//! the transformations users actually execute.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::process::ExitCode;

use nulang::lexer::Lexer;
use nulang::mir;
use nulang::parser::Parser;
use nulang::typechecker::TypeChecker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Before,
    After,
    Both,
}

#[derive(Debug, PartialEq, Eq)]
struct Config {
    phase: Phase,
    input: Option<String>,
}

fn parse_phase(value: &str) -> Result<Phase, String> {
    match value {
        "before" => Ok(Phase::Before),
        "after" => Ok(Phase::After),
        "both" => Ok(Phase::Both),
        other => Err(format!(
            "unknown MIR phase '{other}'; expected before, after, or both"
        )),
    }
}

fn parse_args<I>(args: I) -> Result<Config, String>
where
    I: IntoIterator<Item = String>,
{
    let mut phase = Phase::Both;
    let mut input = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("--phase=") {
            phase = parse_phase(value)?;
            continue;
        }

        match arg.as_str() {
            "--phase" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--phase requires before, after, or both".to_string())?;
                phase = parse_phase(&value)?;
            }
            "-h" | "--help" => return Err("__help__".to_string()),
            "-" => {
                if input.replace(arg).is_some() {
                    return Err("only one input file may be supplied".to_string());
                }
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option '{arg}'")),
            _ => {
                if input.replace(arg).is_some() {
                    return Err("only one input file may be supplied".to_string());
                }
            }
        }
    }

    Ok(Config { phase, input })
}

fn usage() -> &'static str {
    "Usage: nulang_mir [--phase before|after|both] [FILE|-]\n\
     \n\
     Inspect target-independent Nulang MIR. With no FILE (or with '-'), source\n\
     is read from stdin. The 'after' view is produced by the real bytecode MIR\n\
     optimization pipeline, so it is suitable for optimizer regression checks."
}

fn read_input(path: Option<&str>) -> Result<String, String> {
    match path {
        Some(path) if path != "-" => {
            fs::read_to_string(path).map_err(|error| format!("failed to read '{path}': {error}"))
        }
        _ => {
            let mut source = String::new();
            io::stdin()
                .read_to_string(&mut source)
                .map_err(|error| format!("failed to read stdin: {error}"))?;
            Ok(source)
        }
    }
}

fn lower(source: &str) -> Result<mir::Module, String> {
    let tokens = Lexer::new(source)
        .lex()
        .map_err(|error| format!("lex failed: {error}"))?;
    let ast = Parser::new(tokens)
        .parse_module()
        .map_err(|error| format!("parse failed: {error}"))?;
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .map_err(|error| format!("typecheck failed: {error}"))?;
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    nulang::mir_lower::lower_module(&hir).map_err(|error| format!("MIR lowering failed: {error}"))
}

fn render_module(module: &mir::Module) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "module {}", module.name);

    for function in &module.functions {
        render_function(&mut out, function);
    }
    for function in &module.behaviors {
        let _ = writeln!(out, "behavior");
        render_function(&mut out, function);
    }

    out
}

fn render_function(out: &mut String, function: &mir::Function) {
    let params = function
        .params
        .iter()
        .map(|id| format!("%{}", id.0))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(
        out,
        "\nfn {}({params}) -> {:?} {{",
        function.name, function.ret
    );

    for block in &function.blocks {
        let _ = writeln!(out, "  bb{}:", block.id.0);
        for stmt in &block.stmts {
            match stmt {
                mir::Stmt::Assign { dst, op } => {
                    let _ = writeln!(out, "    %{} = {op:?}", dst.0);
                }
                other => {
                    let _ = writeln!(out, "    {other:?}");
                }
            }
        }
        render_terminator(out, &block.terminator);
    }

    let _ = writeln!(out, "}}");
}

fn render_terminator(out: &mut String, terminator: &mir::Terminator) {
    match terminator {
        mir::Terminator::Return(Some(value)) => {
            let _ = writeln!(out, "    return %{}", value.0);
        }
        mir::Terminator::Return(None) => {
            let _ = writeln!(out, "    return");
        }
        mir::Terminator::Jump(target) => {
            let _ = writeln!(out, "    jump bb{}", target.0);
        }
        mir::Terminator::Branch { cond, then_, else_ } => {
            let _ = writeln!(
                out,
                "    branch %{} -> bb{}, bb{}",
                cond.0, then_.0, else_.0
            );
        }
        mir::Terminator::Resume(value) => {
            let _ = writeln!(out, "    resume %{}", value.0);
        }
        mir::Terminator::Unterminated => {
            let _ = writeln!(out, "    <unterminated>");
        }
    }
}

fn run(config: Config) -> Result<(), String> {
    let source = read_input(config.input.as_deref())?;
    let mut module = lower(&source)?;

    if matches!(config.phase, Phase::Before | Phase::Both) {
        if config.phase == Phase::Both {
            println!("=== MIR before optimization ===");
        }
        print!("{}", render_module(&module));
    }

    if matches!(config.phase, Phase::After | Phase::Both) {
        nulang::mir_codegen::compile_mir(&mut module, "mir-inspect")
            .map_err(|error| format!("MIR optimization/codegen failed: {error}"))?;
        if config.phase == Phase::Both {
            println!("=== MIR after optimization ===");
        }
        print!("{}", render_module(&module));
    }

    Ok(())
}

fn main() -> ExitCode {
    let config = match parse_args(env::args().skip(1)) {
        Ok(config) => config,
        Err(error) if error == "__help__" => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("nulang_mir: {error}\n\n{}", usage());
            return ExitCode::FAILURE;
        }
    };

    match run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nulang_mir: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang::mir::{RValue, Stmt};

    #[test]
    fn phase_parser_accepts_supported_values() {
        assert_eq!(parse_phase("before").unwrap(), Phase::Before);
        assert_eq!(parse_phase("after").unwrap(), Phase::After);
        assert_eq!(parse_phase("both").unwrap(), Phase::Both);
        assert!(parse_phase("machine-code").is_err());
    }

    #[test]
    fn argument_parser_defaults_to_both_and_stdin() {
        assert_eq!(
            parse_args(Vec::<String>::new()).unwrap(),
            Config {
                phase: Phase::Both,
                input: None,
            }
        );
        assert_eq!(
            parse_args(["--phase=after".to_string(), "sample.nu".to_string()]).unwrap(),
            Config {
                phase: Phase::After,
                input: Some("sample.nu".to_string()),
            }
        );
    }

    #[test]
    fn renderer_exposes_function_and_basic_block_structure() {
        let module = lower("1 + 2").unwrap();
        let rendered = render_module(&module);

        assert!(rendered.contains("fn __main"));
        assert!(rendered.contains("bb0:"));
        assert!(rendered.contains("return"));
    }

    #[test]
    fn optimized_view_uses_the_real_mir_optimizer() {
        let mut module = lower("1 + 2").unwrap();
        let before_has_binary = module.functions.iter().any(|function| {
            function.blocks.iter().any(|block| {
                block.stmts.iter().any(|stmt| {
                    matches!(
                        stmt,
                        Stmt::Assign {
                            op: RValue::Binary(_, _, _),
                            ..
                        }
                    )
                })
            })
        });
        assert!(
            before_has_binary,
            "lowering should expose the unfused binary op"
        );

        nulang::mir_codegen::compile_mir(&mut module, "mir-inspect-test")
            .expect("MIR optimizer/codegen must succeed");

        let after_has_binary = module.functions.iter().any(|function| {
            function.blocks.iter().any(|block| {
                block.stmts.iter().any(|stmt| {
                    matches!(
                        stmt,
                        Stmt::Assign {
                            op: RValue::Binary(_, _, _),
                            ..
                        }
                    )
                })
            })
        });
        assert!(
            !after_has_binary,
            "constant binary operation should be folded in the optimized MIR view"
        );
    }
}
