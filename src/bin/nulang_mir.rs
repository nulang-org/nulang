//! Inspect Nulang MIR before and after the existing optimization pipeline.
//!
//! This tool intentionally goes through the same frontend and `compile_mir`
//! entry point as normal bytecode compilation so the optimized view reflects
//! the transformations users actually execute.

use std::process::ExitCode;

use nulang::mir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Before,
    After,
    Both,
}

fn parse_phase(_value: &str) -> Result<Phase, String> {
    todo!("implemented after the RED tests")
}

fn render_module(_module: &mir::Module) -> String {
    todo!("implemented after the RED tests")
}

fn main() -> ExitCode {
    eprintln!("nulang_mir: implementation pending");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang::lexer::Lexer;
    use nulang::mir::{RValue, Stmt};
    use nulang::parser::Parser;
    use nulang::typechecker::TypeChecker;

    fn lower(source: &str) -> mir::Module {
        let tokens = Lexer::new(source).lex().expect("test source must lex");
        let ast = Parser::new(tokens)
            .parse_module()
            .expect("test source must parse");
        let mut type_checker = TypeChecker::new();
        type_checker
            .check_module(&ast)
            .expect("test source must typecheck");
        let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        nulang::mir_lower::lower_module(&hir).expect("test source must lower to MIR")
    }

    #[test]
    fn phase_parser_accepts_supported_values() {
        assert_eq!(parse_phase("before").unwrap(), Phase::Before);
        assert_eq!(parse_phase("after").unwrap(), Phase::After);
        assert_eq!(parse_phase("both").unwrap(), Phase::Both);
        assert!(parse_phase("machine-code").is_err());
    }

    #[test]
    fn renderer_exposes_function_and_basic_block_structure() {
        let module = lower("1 + 2");
        let rendered = render_module(&module);

        assert!(rendered.contains("fn __main"));
        assert!(rendered.contains("bb0:"));
        assert!(rendered.contains("return"));
    }

    #[test]
    fn optimized_view_uses_the_real_mir_optimizer() {
        let mut module = lower("1 + 2");
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
        assert!(before_has_binary, "lowering should expose the unfused binary op");

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
