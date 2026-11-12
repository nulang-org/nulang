//! Optional SMT backend for function contracts. Only pure, overflow-safe
//! expressions are admissible; solver failures never imply proof.

use crate::ast::{Expr, Param};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SmtOutcome {
    Proved,
    Unknown(String),
}

fn prove_with_oracle(
    _params: &[Param],
    _requires: &[Expr],
    _ensures: &[Expr],
    _wrapped_body: &Expr,
    _oracle: impl FnOnce(&str) -> Result<String, String>,
) -> SmtOutcome {
    SmtOutcome::Unknown("SMT backend not implemented".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Decl;
    use crate::{lexer::Lexer, parser::Parser};

    fn analyze(source: &str, oracle: impl FnOnce(&str) -> Result<String, String>) -> SmtOutcome {
        let tokens = Lexer::new(source).lex().expect("valid lexing");
        let ast = Parser::new(tokens).parse_module().expect("valid parsing");
        match &ast.decls[0] {
            Decl::Function {
                params,
                requires,
                ensures,
                body,
                ..
            } => prove_with_oracle(params, requires, ensures, body, oracle),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn proves_correlated_arithmetic_when_an_oracle_reports_unsat() {
        let source =
            "fn inc(x: Int) -> Int requires x >= 0 && x <= 10 ensures result == x + 1 { x + 1 }";
        let result = analyze(source, |script| {
            assert!(script.contains("(check-sat)"));
            assert!(script.contains("(assert (not"));
            assert!(script.contains("(declare-fun p0 () Int)"));
            Ok("unsat\n".to_string())
        });
        assert_eq!(result, SmtOutcome::Proved);
    }

    #[test]
    fn never_proves_potentially_overflowing_arithmetic_even_if_oracle_says_unsat() {
        let result = analyze(
            "fn inc(x: Int) -> Int ensures result == x + 1 { x + 1 }",
            |_| panic!("unsafe program must not call solver"),
        );
        assert!(matches!(result, SmtOutcome::Unknown(_)));
    }

    #[test]
    fn refuses_unsupported_effectful_postcondition_even_if_short_circuited() {
        let result = analyze("fn check() -> Int ensures true || mystery() { 1 }", |_| {
            panic!("unsupported expression must not call solver")
        });
        assert!(matches!(result, SmtOutcome::Unknown(_)));
    }

    #[test]
    fn refuses_unsatisfiable_or_unmodeled_preconditions() {
        let result = analyze(
            "fn bad(x: Int) -> Int requires x > 10 && x < 0 ensures result >= 0 { x }",
            |_| panic!("contradictory preconditions must not call solver"),
        );
        assert!(matches!(result, SmtOutcome::Unknown(_)));
    }

    #[test]
    fn solver_sat_never_becomes_unverified_counterexample() {
        let source =
            "fn wrong(x: Int) -> Int requires x >= 0 && x <= 10 ensures result == x + 2 { x + 1 }";
        let result = analyze(source, |_| Ok("sat\n".to_owned()));
        assert!(matches!(result, SmtOutcome::Unknown(_)));
    }

    #[test]
    fn solver_errors_and_unknown_are_never_proofs() {
        let source =
            "fn inc(x: Int) -> Int requires x >= 0 && x <= 10 ensures result == x + 1 { x + 1 }";
        assert!(matches!(
            analyze(source, |_| Err("missing z3".to_owned())),
            SmtOutcome::Unknown(_)
        ));
        assert!(matches!(
            analyze(source, |_| Ok("unknown\n".to_owned())),
            SmtOutcome::Unknown(_)
        ));
        assert!(matches!(
            analyze(source, |_| Ok("unsat\nwarning".to_owned())),
            SmtOutcome::Unknown(_)
        ));
    }

    #[test]
    fn alpha_renaming_and_let_shadowing_are_preserved_in_solver_model() {
        let source = "fn inc(x: Int) -> Int requires x >= 0 && x <= 10 ensures result == x + 2 { let x = x + 1 in x + 1 }";
        let result = analyze(source, |script| {
            assert!(script.contains("(let ((local0"));
            Ok("unsat".to_owned())
        });
        assert_eq!(result, SmtOutcome::Proved);
    }
}
