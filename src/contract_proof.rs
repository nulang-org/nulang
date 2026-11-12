//! Optional, conservative static analysis of existing Nulang function contracts.
//! This tool never runs during normal compilation.
use crate::ast::{AstModule, BinOp, Decl, Expr, Literal, Param, UnOp};
use crate::types::{PrimitiveType, Type};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

// Nulang's VM uses 48-bit signed Int payloads. Do not claim arithmetic proofs
// for values that may overflow that range (including across backend tiers).
const MIN_INT: i128 = -(1i128 << 47);
const MAX_INT: i128 = (1i128 << 47) - 1;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witness: Option<BTreeMap<String, i64>>,
}

#[derive(Debug, Clone, Copy)]
struct Interval {
    lo: i128,
    hi: i128,
}

impl Interval {
    fn new(lo: i128, hi: i128) -> Option<Self> {
        (lo >= MIN_INT && hi <= MAX_INT && lo <= hi).then_some(Self { lo, hi })
    }

    fn intersect(self, other: Self) -> Option<Self> {
        Self::new(self.lo.max(other.lo), self.hi.min(other.hi))
    }
}

type Bounds = HashMap<String, Interval>;

/// Analyze existing function postconditions; does not alter compilation or
/// runtime contracts. Only a deliberately narrow, pure, overflow-safe subset
/// can be marked `proved`. Everything else is `unknown`.
pub fn prove_module(module: &AstModule) -> Vec<ProofResult> {
    let mut results = Vec::new();
    visit_decls(&module.decls, &mut results);
    results
}

fn visit_decls(decls: &[Decl], results: &mut Vec<ProofResult>) {
    for decl in decls {
        match decl {
            Decl::Function {
                name,
                params,
                requires,
                ensures,
                body,
                ..
            } if !ensures.is_empty() => {
                results.push(prove_function(name, params, requires, ensures, body));
            }
            Decl::Module { decls, .. } => visit_decls(decls, results),
            _ => {}
        }
    }
}

fn unknown(name: &str, reason: &str) -> ProofResult {
    ProofResult {
        function: name.to_owned(),
        status: ProofStatus::Unknown,
        detail: reason.to_owned(),
        witness: None,
    }
}

/// Only remove wrappers matching the exact parser-generated contract guards.
/// An arbitrary user-written `if` or `let result` must not be discarded.
fn original_body<'a>(body: &'a Expr, has_requires: bool) -> Option<&'a Expr> {
    let post = if has_requires {
        match body {
            Expr::If {
                then_branch,
                else_branch: Some(failure),
                ..
            } if matches!(failure.as_ref(), Expr::Panic(label, _) if label == "precondition_violation") => {
                then_branch.as_ref()
            }
            _ => return None,
        }
    } else {
        body
    };
    match post {
        Expr::Let {
            name,
            mutable: false,
            value,
            body: check,
            ..
        } if name == "result" => match check.as_ref() {
            Expr::If {
                then_branch,
                else_branch: Some(failure),
                ..
            } if matches!(then_branch.as_ref(), Expr::Var(var, _) if var == "result")
                && matches!(failure.as_ref(), Expr::Panic(label, _) if label == "postcondition_violation") =>
            {
                Some(value.as_ref())
            }
            _ => None,
        },
        _ => None,
    }
}

fn prove_function(
    name: &str,
    params: &[Param],
    requires: &[Expr],
    ensures: &[Expr],
    wrapped_body: &Expr,
) -> ProofResult {
    let mut bounds = Bounds::new();
    for param in params {
        if !matches!(param.ty.as_ref(), Some(Type::Primitive(PrimitiveType::Int))) {
            return unknown(name, "only explicitly typed Int parameters are supported");
        }
        bounds.insert(
            param.name.clone(),
            Interval {
                lo: MIN_INT,
                hi: MAX_INT,
            },
        );
    }

    for requirement in requires {
        if !constrain(requirement, &mut bounds) {
            return unknown(name, "precondition is unsupported or inconsistent");
        }
    }
    let input_bounds = bounds.clone();
    let Some(body) = original_body(wrapped_body, !requires.is_empty()) else {
        return unknown(name, "contract lowering is not recognized");
    };
    let Some(result) = eval_int(body, &bounds) else {
        return unknown(
            name,
            "body is not in the pure, overflow-safe integer subset",
        );
    };
    bounds.insert("result".to_owned(), result);
    let outcomes: Vec<_> = ensures.iter().map(|e| eval_bool(e, &bounds)).collect();
    if outcomes.iter().all(|outcome| *outcome == Some(true)) {
        return ProofResult {
            function: name.to_owned(),
            status: ProofStatus::Proved,
            detail: "postconditions hold under supported preconditions; no integer overflow in analyzed body".to_owned(),
            witness: None,
        };
    }

    // Only return a counterexample when the validated input constraints fix
    // every input to one concrete value. No guessed or symbolic witnesses.
    if outcomes.contains(&Some(false))
        && params.iter().all(|param| {
            input_bounds
                .get(&param.name)
                .is_some_and(|range| range.lo == range.hi)
        })
    {
        let witness = params
            .iter()
            .map(|p| (p.name.clone(), input_bounds[&p.name].lo as i64))
            .collect();
        return ProofResult {
            function: name.to_owned(),
            status: ProofStatus::Counterexample,
            detail: "postcondition is false for the reported concrete input".to_owned(),
            witness: Some(witness),
        };
    }
    unknown(
        name,
        "postcondition not established by conservative interval analysis",
    )
}

fn int_literal(expr: &Expr) -> Option<i128> {
    let value = match expr {
        Expr::Literal(Literal::Int(value), _) => i128::from(*value),
        Expr::Unary {
            op: UnOp::Neg,
            expr,
            ..
        } => match expr.as_ref() {
            // Permit the signed minimum even though its positive magnitude
            // is outside the Int payload range.
            Expr::Literal(Literal::Int(value), _) => -i128::from(*value),
            other => -int_literal(other)?,
        },
        _ => return None,
    };
    (MIN_INT..=MAX_INT).contains(&value).then_some(value)
}

/// Constrain a named integer parameter using conjunctions of var/literal
/// comparisons. Unsupported conditions produce `unknown`, not assumptions.
fn constrain(expr: &Expr, bounds: &mut Bounds) -> bool {
    let Expr::Binary {
        op, left, right, ..
    } = expr
    else {
        return matches!(expr, Expr::Literal(Literal::Bool(true), _));
    };
    if *op == BinOp::And {
        return constrain(left, bounds) && constrain(right, bounds);
    }
    let (name, value, order) = match (left.as_ref(), right.as_ref()) {
        (Expr::Var(name, _), right) => (name, int_literal(right), false),
        (left, Expr::Var(name, _)) => (name, int_literal(left), true),
        _ => return false,
    };
    let Some(value) = value else { return false };
    let Some(old) = bounds.get(name).copied() else {
        return false;
    };
    let (lo, hi) = match (op, order) {
        (BinOp::Ge, false) | (BinOp::Le, true) => (value, MAX_INT),
        (BinOp::Gt, false) | (BinOp::Lt, true) => (value + 1, MAX_INT),
        (BinOp::Le, false) | (BinOp::Ge, true) => (MIN_INT, value),
        (BinOp::Lt, false) | (BinOp::Gt, true) => (MIN_INT, value - 1),
        (BinOp::Eq, _) => (value, value),
        _ => return false,
    };
    let Some(constrained) = Interval::new(lo, hi).and_then(|range| old.intersect(range)) else {
        return false;
    };
    bounds.insert(name.clone(), constrained);
    true
}

fn eval_int(expr: &Expr, bounds: &Bounds) -> Option<Interval> {
    if let Some(value) = int_literal(expr) {
        return Interval::new(value, value);
    }
    match expr {
        Expr::Var(name, _) => bounds.get(name).copied(),
        Expr::Binary {
            op, left, right, ..
        } => {
            let lhs = eval_int(left, bounds)?;
            let rhs = eval_int(right, bounds)?;
            match op {
                BinOp::Add => Interval::new(lhs.lo + rhs.lo, lhs.hi + rhs.hi),
                BinOp::Sub => Interval::new(lhs.lo - rhs.hi, lhs.hi - rhs.lo),
                BinOp::Mul => {
                    // Bilinear extrema over two closed intervals occur at
                    // their endpoints. Reject any intermediate or final
                    // product that cannot fit the VM's signed Int payload.
                    let products = [
                        lhs.lo.checked_mul(rhs.lo)?,
                        lhs.lo.checked_mul(rhs.hi)?,
                        lhs.hi.checked_mul(rhs.lo)?,
                        lhs.hi.checked_mul(rhs.hi)?,
                    ];
                    Interval::new(*products.iter().min()?, *products.iter().max()?)
                }
                _ => None,
            }
        }
        Expr::Unary {
            op: UnOp::Neg,
            expr,
            ..
        } => {
            let value = eval_int(expr, bounds)?;
            Interval::new(-value.hi, -value.lo)
        }
        Expr::Let {
            name,
            value,
            body,
            mutable: false,
            ..
        } => {
            let value = eval_int(value, bounds)?;
            let mut scope = bounds.clone();
            scope.insert(name.clone(), value);
            eval_int(body, &scope)
        }
        Expr::Block { exprs, .. } if exprs.len() == 1 => eval_int(&exprs[0], bounds),
        _ => None,
    }
}

fn eval_bool(expr: &Expr, bounds: &Bounds) -> Option<bool> {
    match expr {
        Expr::Literal(Literal::Bool(value), _) => Some(*value),
        Expr::Unary {
            op: UnOp::Not,
            expr,
            ..
        } => Some(!eval_bool(expr, bounds)?),
        Expr::Binary {
            op: BinOp::And,
            left,
            right,
            ..
        } => {
            // Analyze both operands even if the truth value is already
            // determined: unknown syntax can hide effects or unsafe code.
            let lhs = eval_bool(left, bounds)?;
            let rhs = eval_bool(right, bounds)?;
            Some(lhs && rhs)
        }
        Expr::Binary {
            op: BinOp::Or,
            left,
            right,
            ..
        } => {
            let lhs = eval_bool(left, bounds)?;
            let rhs = eval_bool(right, bounds)?;
            Some(lhs || rhs)
        }
        Expr::Binary {
            op, left, right, ..
        } => {
            let lhs = eval_int(left, bounds)?;
            let rhs = eval_int(right, bounds)?;
            match op {
                BinOp::Ge if lhs.lo >= rhs.hi => Some(true),
                BinOp::Ge if lhs.hi < rhs.lo => Some(false),
                BinOp::Gt if lhs.lo > rhs.hi => Some(true),
                BinOp::Gt if lhs.hi <= rhs.lo => Some(false),
                BinOp::Le if lhs.hi <= rhs.lo => Some(true),
                BinOp::Le if lhs.lo > rhs.hi => Some(false),
                BinOp::Lt if lhs.hi < rhs.lo => Some(true),
                BinOp::Lt if lhs.lo >= rhs.hi => Some(false),
                BinOp::Eq if lhs.lo == lhs.hi && rhs.lo == rhs.hi => Some(lhs.lo == rhs.lo),
                BinOp::Eq if lhs.hi < rhs.lo || rhs.hi < lhs.lo => Some(false),
                BinOp::Ne if lhs.lo == lhs.hi && rhs.lo == rhs.hi => Some(lhs.lo != rhs.lo),
                BinOp::Ne if lhs.hi < rhs.lo || rhs.hi < lhs.lo => Some(true),
                _ => None,
            }
        }
        _ => None,
    }
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
        let results =
            proof("fn inc(x: Int) -> Int requires x >= 0 && x <= 10 ensures result >= 1 { x + 1 }");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, ProofStatus::Proved);
    }

    #[test]
    fn unknown_when_arithmetic_can_overflow() {
        let results = proof("fn inc(x: Int) -> Int ensures result > 0 { x + 1 }");
        assert_eq!(results[0].status, ProofStatus::Unknown);
    }

    #[test]
    fn proves_bounded_multiplication_with_negative_operands() {
        let results = proof(
            "fn area(x: Int, y: Int) -> Int requires x >= -3 && x <= -1 && y >= -4 && y <= -2 ensures result >= 2 && result <= 12 { x * y }",
        );
        assert_eq!(results[0].status, ProofStatus::Proved);
    }

    #[test]
    fn proves_mixed_sign_multiplication_with_all_endpoint_products() {
        let results = proof(
            "fn area(x: Int, y: Int) -> Int requires x >= -2 && x <= 3 && y >= -4 && y <= 5 ensures result >= -12 && result <= 15 { x * y }",
        );
        assert_eq!(results[0].status, ProofStatus::Proved);
    }

    #[test]
    fn proves_zero_multiplication_even_for_unbounded_int() {
        let results = proof("fn zero(x: Int) -> Int ensures result == 0 { x * 0 }");
        assert_eq!(results[0].status, ProofStatus::Proved);
    }

    #[test]
    fn multiplication_beyond_int_payload_range_remains_unknown() {
        let results = proof("fn double(x: Int) -> Int ensures result >= 0 { x * 2 }");
        assert_eq!(results[0].status, ProofStatus::Unknown);
        let signed_min = proof(
            "fn negate_min(x: Int) -> Int requires x == -140737488355328 ensures result > 0 { x * -1 }",
        );
        assert_eq!(signed_min[0].status, ProofStatus::Unknown);
    }

    #[test]
    fn bounded_multiplication_can_report_a_real_counterexample() {
        let results = proof("fn bad(x: Int) -> Int requires x == 2 ensures result == 5 { x * 2 }");
        assert_eq!(results[0].status, ProofStatus::Counterexample);
        assert_eq!(results[0].witness.as_ref().unwrap()["x"], 2);
    }

    #[test]
    fn constant_violation_has_real_counterexample() {
        let results = proof("fn bad() -> Int ensures result == 2 { 1 }");
        assert_eq!(results[0].status, ProofStatus::Counterexample);
    }

    #[test]
    fn contradictory_preconditions_are_not_misleadingly_proved() {
        let results =
            proof("fn bad(x: Int) -> Int requires x > 5 && x < 0 ensures result > 0 { x }");
        assert_eq!(results[0].status, ProofStatus::Unknown);
    }

    #[test]
    fn reversed_integer_bounds_are_supported() {
        let results =
            proof("fn bounded(x: Int) -> Int requires 0 <= x && 5 >= x ensures result >= 0 { x }");
        assert_eq!(results[0].status, ProofStatus::Proved);
    }

    #[test]
    fn singleton_input_counterexample_reports_original_parameter_value() {
        let results = proof(
            "fn bad(result: Int) -> Int requires result == 1 ensures result == 3 { result + 1 }",
        );
        assert_eq!(results[0].status, ProofStatus::Counterexample);
        assert_eq!(results[0].witness.as_ref().unwrap()["result"], 1);
    }

    #[test]
    fn signed_min_negation_must_not_be_proved() {
        let results =
            proof("fn bad(x: Int) -> Int requires x == -140737488355328 ensures result > 0 { -x }");
        assert_eq!(results[0].status, ProofStatus::Unknown);
    }

    #[test]
    fn boolean_shortcut_does_not_hide_unsupported_effects_or_calls() {
        let either = proof("fn suspicious() -> Int ensures true || mystery() { 1 }");
        assert_eq!(either[0].status, ProofStatus::Unknown);
        let both = proof("fn suspicious() -> Int ensures false && mystery() { 1 }");
        assert_eq!(both[0].status, ProofStatus::Unknown);
    }

    #[test]
    fn uncontracted_code_is_unaffected() {
        let results = proof("fn plain(x: Int) -> Int { x }");
        assert!(results.is_empty());
    }
}
