//! Conservative compiler-side screening for future dataflow optimizations.
//!
//! An eligible *candidate* is not permission to optimize: type checking,
//! immutable/reference-capability validation, semantic equivalence and a
//! backend-specific proof are additional mandatory gates.

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
}
