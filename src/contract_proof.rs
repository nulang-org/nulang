//! Optional, conservative static analysis of existing Nulang function contracts.
//! This tool never runs during normal compilation.
use crate::ast::AstModule;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofStatus {
    Proved,
    Unknown,
    Counterexample,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProofResult {
    pub function: String,
    pub status: ProofStatus,
    pub detail: String,
}

pub fn prove_module(_module: &AstModule) -> Vec<ProofResult> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer::Lexer, parser::Parser};

    fn proof(source: &str) -> Vec<ProofResult> {
        let tokens = Lexer::new(source).lex().expect("valid tokens");
        let module = Parser::new(tokens).parse_module().expect("valid Nulang");
        prove_module(&module)
    }

    #[test]
    fn proves_bounded_increment_without_new_syntax() {
        let results = proof("fn inc(x: Int) -> Int requires x >= 0 && x <= 10 ensures result >= 1 { x + 1 }");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, ProofStatus::Proved);
    }

    #[test]
    fn unknown_when_arithmetic_can_overflow() {
        let results = proof("fn inc(x: Int) -> Int ensures result > 0 { x + 1 }");
        assert_eq!(results[0].status, ProofStatus::Unknown);
    }

    #[test]
    fn unknown_on_unsupported_arithmetic() {
        let results = proof("fn double(x: Int) -> Int ensures result >= 0 { x * 2 }");
        assert_eq!(results[0].status, ProofStatus::Unknown);
    }

    #[test]
    fn constant_violation_has_real_counterexample() {
        let results = proof("fn bad() -> Int ensures result == 2 { 1 }");
        assert_eq!(results[0].status, ProofStatus::Counterexample);
    }

    #[test]
    fn contradictory_preconditions_are_not_misleadingly_proved() {
        let results = proof("fn bad(x: Int) -> Int requires x > 5 && x < 0 ensures result > 0 { x }");
        assert_eq!(results[0].status, ProofStatus::Unknown);
    }

    #[test]
    fn uncontracted_code_is_unaffected() {
        let results = proof("fn plain(x: Int) -> Int { x }");
        assert!(results.is_empty());
    }
}
