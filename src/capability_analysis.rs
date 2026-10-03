//! Compiler-owned capability-analysis traversal.
//!
//! Frontends should share this driver instead of independently deciding which
//! declaration bodies participate in capability inference. The traversal
//! contract mirrors the CLI/DAP compiler pipeline: functions, actor behavior
//! bodies, actor state defaults/init expressions, and workflow step/
//! compensation bodies are all checked.

use crate::ast::{Decl, WorkflowItem, WorkflowStep};
use crate::effect_checker::{flatten_decls, CapContext, CapabilityAnalyzer};
use crate::types::NuResult;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilityAnalysisSummary {
    pub bodies_checked: usize,
}

/// Analyze every capability-bearing expression body in a declaration tree.
///
/// This is the canonical module-level traversal for compiler frontends. Keep
/// capability *inference* in `CapabilityAnalyzer`; this function only owns the
/// question of which declaration bodies must be visited and which parameter
/// capabilities seed each body context.
pub fn analyze_module_capabilities(decls: &[Decl]) -> NuResult<CapabilityAnalysisSummary> {
    let mut analyzer = CapabilityAnalyzer::new();
    let base_ctx = CapContext::new();
    let mut bodies_checked = 0usize;

    let mut analyze_body = |ctx: &CapContext, body: &crate::ast::Expr| -> NuResult<()> {
        analyzer.infer_cap(ctx, body)?;
        bodies_checked += 1;
        Ok(())
    };

    for decl in flatten_decls(decls) {
        match decl {
            Decl::Function { body, params, .. } => {
                let ctx = base_ctx.with_params(params);
                analyze_body(&ctx, body)?;
            }
            Decl::Actor {
                behaviors,
                state_fields,
                init,
                ..
            } => {
                for behavior in behaviors {
                    let ctx = base_ctx.with_params(&behavior.params);
                    analyze_body(&ctx, &behavior.body)?;
                }
                for (_, _, _, default) in state_fields {
                    analyze_body(&base_ctx, default)?;
                }
                for (_, expr) in init {
                    analyze_body(&base_ctx, expr)?;
                }
            }
            Decl::Workflow {
                items, compensate, ..
            } => {
                for item in items {
                    let steps: &[WorkflowStep] = match item {
                        WorkflowItem::Step(step) => std::slice::from_ref(step),
                        WorkflowItem::Parallel(steps) => steps,
                    };
                    for step in steps {
                        analyze_body(&base_ctx, &step.body)?;
                        if let Some(compensation) = &step.compensate {
                            analyze_body(&base_ctx, compensation)?;
                        }
                    }
                }
                if let Some(compensation) = compensate {
                    analyze_body(&base_ctx, compensation)?;
                }
            }
            _ => {}
        }
    }

    Ok(CapabilityAnalysisSummary { bodies_checked })
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
