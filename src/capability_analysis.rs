//! Compiler-owned capability-analysis traversal.
//!
//! Frontends should share this driver instead of independently deciding which
//! declaration bodies participate in capability inference. The traversal
//! contract mirrors the CLI/DAP compiler pipeline: functions, actor behavior
//! bodies, actor state defaults/init expressions, and workflow step/
//! compensation bodies are all checked.

use crate::ast::Decl;
use crate::types::NuResult;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilityAnalysisSummary {
    pub bodies_checked: usize,
}

/// Analyze every capability-bearing expression body in a declaration tree.
///
/// RED phase: tests below specify the traversal contract before the shared
/// implementation replaces the duplicated frontend loops.
pub fn analyze_module_capabilities(_decls: &[Decl]) -> NuResult<CapabilityAnalysisSummary> {
    Ok(CapabilityAnalysisSummary::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> crate::ast::Module {
        let tokens = Lexer::new(source).lex().expect("lex capability fixture");
        Parser::new(tokens)
            .parse_module()
            .expect("parse capability fixture")
    }

    #[test]
    fn traverses_function_actor_state_and_workflow_bodies() {
        let source = r#"
            fn helper(x: Int) -> Int { x }

            actor Counter {
                state count: Int = 0
                behavior add(n: Int) { self.count = self.count + n }
            }

            workflow Saga {
                step reserve {
                    1
                } compensate {
                    2
                }
                step ship {
                    3
                }
            }
        "#;
        let ast = parse(source);

        let summary = analyze_module_capabilities(&ast.decls).expect("capability analysis");
        // helper + state default + behavior + 2 step bodies + 1 compensation
        assert_eq!(summary.bodies_checked, 6);
    }

    #[test]
    fn empty_module_checks_no_bodies() {
        let ast = parse("");
        let summary = analyze_module_capabilities(&ast.decls).expect("empty analysis");
        assert_eq!(summary.bodies_checked, 0);
    }
}
