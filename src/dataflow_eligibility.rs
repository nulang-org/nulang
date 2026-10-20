//! Conservative compiler-side screening for future dataflow optimizations.
//!
//! A positive *candidate* result is not permission to optimize: it does not
//! establish input immutability, full type/capability safety, callback
//! determinism, or bytecode/WASM observational equivalence. The caller must
//! supply only compiler-validated immutable input names. This pass is not
//! connected to MIR lowering or runtime scheduling.

use crate::ast::Expr;
use crate::effect_checker::{EffectChecker, EffectContext};
use crate::types::EffectRow;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IneligibleReason {
    /// Calls, mutation, actor state, effects, unverified captures or any
    /// construct outside the deliberately small supported AST subset.
    UnsupportedShapeOrCapture,
    /// The canonical checker found a nonempty, open or unresolved effect row.
    NonPureOrUnknownEffect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataflowEligibility {
    /// A *screened candidate*, NOT an optimization or concurrency license.
    Candidate,
    Ineligible(IneligibleReason),
}

/// Screen a closed, deliberately restricted expression subset for future
/// optimization research. No source or backend behavior changes.
///
/// `immutable_inputs` are asserted by the caller; this function cannot
/// independently establish their reference capabilities or whether they are
/// mutable at the call site. A production optimizer must instead obtain
/// inputs from the canonical checked-and-resolved compiler environment.
pub fn classify_pure_candidate(
    expr: &Expr,
    immutable_inputs: &[&str],
    checker: &mut EffectChecker,
) -> DataflowEligibility {
    let inputs = immutable_inputs
        .iter()
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<_>>();
    if !supported_pure_shape(expr, &inputs) {
        return DataflowEligibility::Ineligible(IneligibleReason::UnsupportedShapeOrCapture);
    }
    // An empty row alone is *not* sufficient, because unknown/indirect
    // calls can be underrepresented by effect inference. Shape checks above
    // reject all call sites and other opaque constructs.
    match checker.infer_effects(&EffectContext::empty(), expr) {
        Ok(EffectRow::Closed(effects)) if effects.is_empty() => {
            DataflowEligibility::Candidate
        }
        _ => DataflowEligibility::Ineligible(IneligibleReason::NonPureOrUnknownEffect),
    }
}

fn supported_pure_shape(expr: &Expr, inputs: &BTreeSet<String>) -> bool {
    match expr {
        Expr::Literal(..) => true,
        Expr::Var(name, _) => inputs.contains(name),
        Expr::Binary { left, right, .. } => {
            supported_pure_shape(left, inputs) && supported_pure_shape(right, inputs)
        }
        Expr::Unary { expr, .. } => supported_pure_shape(expr, inputs),
        Expr::Tuple(items, _) | Expr::Block { exprs: items, .. } => {
            items.iter().all(|item| supported_pure_shape(item, inputs))
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            supported_pure_shape(cond, inputs)
                && supported_pure_shape(then_branch, inputs)
                && else_branch
                    .as_ref()
                    .is_none_or(|branch| supported_pure_shape(branch, inputs))
        }
        Expr::Let {
            name,
            value,
            body,
            mutable: false,
            ..
        } => {
            if !supported_pure_shape(value, inputs) {
                return false;
            }
            let mut extended = inputs.clone();
            extended.insert(name.clone());
            supported_pure_shape(body, &extended)
        }
        // Deliberately reject App even when effect inference returns an empty
        // row: unresolved callees, closures and shadowed names are not proof
        // of purity. Reject mutation, panic, consume, loops, effects, match,
        // aggregate read/write, actors, reference annotations and all other
        // source forms until their contracts can be proved separately.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_pure_candidate, DataflowEligibility, IneligibleReason};
    use crate::ast::{BinOp, Expr, Literal};
    use crate::effect_checker::EffectChecker;
    use crate::types::Span;

    fn sp() -> Span {
        Span::default()
    }

    fn num(n: i64) -> Expr {
        Expr::Literal(Literal::Int(n), sp())
    }

    fn var(name: &str) -> Expr {
        Expr::Var(name.into(), sp())
    }

    fn add(left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op: BinOp::Add,
            left: Box::new(left),
            right: Box::new(right),
            span: sp(),
        }
    }

    fn screen(expr: &Expr, immutable_inputs: &[&str]) -> DataflowEligibility {
        classify_pure_candidate(expr, immutable_inputs, &mut EffectChecker::new())
    }

    #[test]
    fn immutable_input_arithmetic_can_be_a_candidate() {
        assert_eq!(
            screen(&add(var("item"), num(1)), &["item"]),
            DataflowEligibility::Candidate
        );
    }

    #[test]
    fn unknown_outer_capture_fails_closed() {
        assert_eq!(
            screen(&add(var("item"), var("unproven")), &["item"]),
            DataflowEligibility::Ineligible(IneligibleReason::UnsupportedShapeOrCapture)
        );
    }

    #[test]
    fn immutable_local_let_is_scoped_and_allowed() {
        let expr = Expr::Let {
            name: "double".into(),
            ty: None,
            value: Box::new(add(var("item"), var("item"))),
            body: Box::new(add(var("double"), num(1))),
            mutable: false,
            let_in: true,
            span: sp(),
        };
        assert_eq!(screen(&expr, &["item"]), DataflowEligibility::Candidate);
        assert_eq!(
            screen(&expr, &[]),
            DataflowEligibility::Ineligible(IneligibleReason::UnsupportedShapeOrCapture)
        );
    }

    #[test]
    fn mutable_local_bindings_are_not_candidates() {
        let expr = Expr::Let {
            name: "counter".into(),
            ty: None,
            value: Box::new(num(1)),
            body: Box::new(var("counter")),
            mutable: true,
            let_in: true,
            span: sp(),
        };
        assert_eq!(
            screen(&expr, &[]),
            DataflowEligibility::Ineligible(IneligibleReason::UnsupportedShapeOrCapture)
        );
    }

    #[test]
    fn unknown_calls_are_rejected_even_if_effect_inference_is_empty() {
        let expr = Expr::App {
            func: Box::new(var("unknown")),
            args: vec![num(1)],
            span: sp(),
        };
        assert_eq!(
            screen(&expr, &["unknown"]),
            DataflowEligibility::Ineligible(IneligibleReason::UnsupportedShapeOrCapture)
        );
    }

    #[test]
    fn performing_effects_is_not_an_eligible_dataflow_transform() {
        let expr = Expr::Perform {
            effect: "IO".into(),
            op: "print".into(),
            args: vec![num(1)],
            span: sp(),
        };
        assert_eq!(
            screen(&expr, &[]),
            DataflowEligibility::Ineligible(IneligibleReason::UnsupportedShapeOrCapture)
        );
    }

    #[test]
    fn mutable_assignment_is_rejected() {
        let expr = Expr::Assign {
            target: Box::new(var("item")),
            value: Box::new(num(2)),
            span: sp(),
        };
        assert_eq!(
            screen(&expr, &["item"]),
            DataflowEligibility::Ineligible(IneligibleReason::UnsupportedShapeOrCapture)
        );
    }

    #[test]
    fn both_conditional_branches_must_be_pure_candidates() {
        let expr = Expr::If {
            cond: Box::new(var("enabled")),
            then_branch: Box::new(num(1)),
            else_branch: Some(Box::new(Expr::Perform {
                effect: "IO".into(),
                op: "print".into(),
                args: vec![num(9)],
                span: sp(),
            })),
            span: sp(),
        };
        assert_eq!(
            screen(&expr, &["enabled"]),
            DataflowEligibility::Ineligible(IneligibleReason::UnsupportedShapeOrCapture)
        );
    }

    fn typed_transform_module(
        input_cap: Option<crate::types::Capability>,
        body: Expr,
    ) -> crate::ast::AstModule {
        use crate::ast::{Decl, Param};
        use crate::types::{EffectRow, Type};

        crate::ast::AstModule {
            name: "DataflowScreen".into(),
            decls: vec![Decl::Function {
                name: "transform".into(),
                type_params: vec![],
                type_param_constraints: vec![],
                params: vec![Param {
                    name: "item".into(),
                    ty: Some(Type::int()),
                    cap: input_cap,
                }],
                default_values: vec![None],
                using_params: vec![],
                ret_type: Some(Type::int()),
                error_type: None,
                effect: Some(EffectRow::empty()),
                cap: None,
                requires: vec![],
                ensures: vec![],
                body,
                annotations: vec![],
                public: false,
                span: sp(),
            }],
        }
    }

    #[test]
    fn validated_immutable_function_parameters_produce_candidate() {
        let module = typed_transform_module(
            Some(crate::types::Capability::Val),
            add(var("item"), num(1)),
        );
        assert_eq!(
            super::classify_checked_function_candidate(&module, "transform"),
            DataflowEligibility::Candidate
        );
    }

    #[test]
    fn missing_immutable_annotation_is_not_proven_by_default_capability() {
        let module = typed_transform_module(None, add(var("item"), num(1)));
        assert_ne!(
            super::classify_checked_function_candidate(&module, "transform"),
            DataflowEligibility::Candidate
        );
    }

    #[test]
    fn mutable_reference_parameters_do_not_qualify() {
        let module = typed_transform_module(
            Some(crate::types::Capability::Ref),
            add(var("item"), num(1)),
        );
        assert_ne!(
            super::classify_checked_function_candidate(&module, "transform"),
            DataflowEligibility::Candidate
        );
    }

    #[test]
    fn compiler_type_errors_reject_even_pure_looking_expressions() {
        let module = typed_transform_module(
            Some(crate::types::Capability::Val),
            add(var("item"), Expr::Literal(Literal::Bool(true), sp())),
        );
        assert_ne!(
            super::classify_checked_function_candidate(&module, "transform"),
            DataflowEligibility::Candidate
        );
    }

    #[test]
    fn missing_and_ambiguous_function_declarations_fail_closed() {
        let mut module = typed_transform_module(
            Some(crate::types::Capability::Val),
            add(var("item"), num(1)),
        );
        assert_ne!(
            super::classify_checked_function_candidate(&module, "missing"),
            DataflowEligibility::Candidate
        );
        module.decls.push(module.decls[0].clone());
        assert_ne!(
            super::classify_checked_function_candidate(&module, "transform"),
            DataflowEligibility::Candidate
        );
    }
}
