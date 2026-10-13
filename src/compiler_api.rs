//! Embeddable compiler entry points.
//!
//! This module exposes the Nulang frontend + MIR -> WASM pipeline without
//! invoking the CLI, touching output files, or spawning a compiler subprocess.
//! It is intended for hermetic embedders such as isolated build workers.
//!
//! The source-only API deliberately rejects `import` declarations. Imports are
//! a source-bundle/package-resolution concern and must be resolved explicitly by
//! the caller rather than through ambient filesystem access inside a sandbox.

#[cfg(feature = "wasm-backend")]
use crate::ast::{Decl, Expr, Param, WorkflowItem, WorkflowStep};
#[cfg(feature = "wasm-backend")]
use crate::backends::{DefaultWasmBackend, WasmBackend};
#[cfg(feature = "wasm-backend")]
use crate::effect_checker::{flatten_decls, CapContext, CapabilityAnalyzer, EffectChecker};
#[cfg(feature = "wasm-backend")]
use crate::lexer::Lexer;
#[cfg(feature = "wasm-backend")]
use crate::parser::Parser;
#[cfg(feature = "wasm-backend")]
use crate::typechecker::TypeChecker;
#[cfg(feature = "wasm-backend")]
use crate::types::{NuError, NuResult, Span};

/// Compile one self-contained Nulang source module to a core WebAssembly
/// module in-process.
///
/// This uses the same semantic gates as the CLI frontend: prelude injection,
/// type checking, effect checking, web-route validation, and capability
/// analysis. No resource capabilities are granted by default.
///
/// Filesystem imports are intentionally rejected. A build system should resolve
/// an explicit source bundle before entering this hermetic compiler boundary.
#[cfg(feature = "wasm-backend")]
pub fn compile_source_to_wasm(source: &str, module_name: &str) -> NuResult<Vec<u8>> {
    compile_source_to_wasm_with_grants(source, module_name, &[])
}

/// Compile one self-contained Nulang source module with an explicit set of
/// resource grants.
///
/// `resource_grants` has the same syntax/meaning as the CLI
/// `--with-capability`/resource-grant surface consumed by [`EffectChecker`].
/// The caller owns policy: this function never invents or broadens grants.
#[cfg(feature = "wasm-backend")]
pub fn compile_source_to_wasm_with_grants(
    source: &str,
    module_name: &str,
    resource_grants: &[String],
) -> NuResult<Vec<u8>> {
    // Parse the prelude first, matching the CLI frontend. Only variant-type
    // declarations are injected; this is the existing Nulang frontend contract.
    let prelude = crate::prelude_source::PRELUDE_SOURCE;
    let prelude_tokens = Lexer::new(prelude).lex()?;
    let prelude_ast = Parser::new(prelude_tokens).parse_module()?;

    crate::types::set_source_map_with_file(source, Some("<embedded>"));
    let tokens = Lexer::new(source).lex()?;
    let mut parser = Parser::new(tokens);
    let mut ast = parser.parse_module()?;

    // A hermetic compiler must not perform ambient filesystem resolution.
    // Reject imports even when nested inside a module so a source bundle cannot
    // accidentally compile differently depending on guest filesystem contents.
    if let Some((path, span)) = flatten_decls(&ast.decls).into_iter().find_map(|decl| {
        if let Decl::Import { path, span, .. } = decl {
            Some((path.clone(), *span))
        } else {
            None
        }
    }) {
        return Err(NuError::parse_error(
            format!(
                "embedded source-only compilation does not resolve import '{path}'; resolve an explicit source bundle before compilation"
            ),
            span,
        ));
    }

    let mut prelude_variants: Vec<Decl> = prelude_ast
        .decls
        .into_iter()
        .filter(|decl| matches!(decl, Decl::VariantType { .. }))
        .collect();
    prelude_variants.append(&mut ast.decls);
    ast.decls = prelude_variants;

    // Type checking supplies the declaration metadata consumed by HIR lowering.
    let mut type_checker = TypeChecker::new();
    type_checker.check_module(&ast)?;

    // Match the CLI effect gate. Diagnostics are advisory output in the CLI;
    // the library API remains side-effect-free and returns only hard failures.
    let flat_decls = flatten_decls(&ast.decls);
    let mut effect_checker = EffectChecker::new();
    effect_checker.set_resource_grants(resource_grants);
    effect_checker.check_module(&ast.decls)?;

    // Keep static Web.route validation part of the compile contract rather than
    // allowing an embedded build to accept a module the CLI rejects.
    let route_diagnostics = crate::web::route_check::check_module(&ast);
    if !route_diagnostics.is_empty() {
        return Err(NuError::TypeError {
            msg: format!(
                "{} route parameter mismatch(es) detected during embedded compilation",
                route_diagnostics.len()
            ),
            span: Span::default(),
            expected_type: None,
            found_type: None,
            similar_names: None,
        });
    }

    // Match the CLI capability-analysis body coverage, including actor state
    // defaults and workflow compensation bodies.
    let mut cap_analyzer = CapabilityAnalyzer::new();
    let cap_body = |analyzer: &mut CapabilityAnalyzer,
                    ctx: &CapContext,
                    body: &Expr|
     -> NuResult<()> { analyzer.infer_cap(ctx, body).map(|_| ()) };
    let seed_from_params = |ctx: &mut CapContext, params: &[Param]| {
        for param in params {
            if let Some(capability) = param.cap {
                *ctx = ctx.clone().with_binding(&param.name, capability);
            }
        }
    };

    for decl in flat_decls.iter().copied() {
        match decl {
            Decl::Function { body, params, .. } => {
                let mut ctx = CapContext::new();
                seed_from_params(&mut ctx, params);
                cap_body(&mut cap_analyzer, &ctx, body)?;
            }
            Decl::Actor {
                behaviors,
                state_fields,
                init,
                ..
            } => {
                for behavior in behaviors {
                    let mut ctx = CapContext::new();
                    seed_from_params(&mut ctx, &behavior.params);
                    cap_body(&mut cap_analyzer, &ctx, &behavior.body)?;
                }
                for (_, _, _, default) in state_fields {
                    cap_body(&mut cap_analyzer, &CapContext::new(), default)?;
                }
                for (_, expr) in init {
                    cap_body(&mut cap_analyzer, &CapContext::new(), expr)?;
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
                        let ctx = CapContext::new();
                        cap_body(&mut cap_analyzer, &ctx, &step.body)?;
                        if let Some(compensation) = &step.compensate {
                            cap_body(&mut cap_analyzer, &ctx, compensation)?;
                        }
                    }
                }
                if let Some(compensation) = compensate {
                    cap_body(&mut cap_analyzer, &CapContext::new(), compensation)?;
                }
            }
            _ => {}
        }
    }

    let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mir = crate::mir_lower::lower_module(&hir)?;
    let mut backend = DefaultWasmBackend;
    backend.compile(&mir, module_name)
}

#[cfg(all(test, feature = "wasm-backend"))]
mod tests {
    use super::*;

    #[test]
    fn inprocess_compile_produces_core_wasm() {
        let wasm = compile_source_to_wasm("fn main() { 1 + 1 }", "embedded-test").unwrap();
        assert!(wasm.len() >= 8);
        assert_eq!(&wasm[..4], b"\0asm");
    }

    #[test]
    fn inprocess_compile_propagates_frontend_errors() {
        let error = compile_source_to_wasm("fn main() { true + 1 }", "bad-types")
            .expect_err("invalid source must not compile");
        assert!(matches!(error, NuError::TypeError { .. }));
    }

    #[test]
    fn inprocess_compile_rejects_ambient_import_resolution() {
        let error = compile_source_to_wasm("import Foo::Bar\nfn main() { 1 }", "imports")
            .expect_err("source-only compiler must reject imports");
        assert!(error.to_string().contains("does not resolve import"));
    }
}
