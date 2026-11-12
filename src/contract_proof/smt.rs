//! Optional SMT-LIB2 backend for the existing contract checker.
//!
//! We deliberately model only 48-bit signed, pure, straight-line integer
//! expressions whose *every* intermediate interval is safe. SMT `unsat`
//! then establishes a contract; sat/unknown/errors are never assumed proved.

use super::{constrain, eval_int, int_literal, original_body, Bounds, Interval, MAX_INT, MIN_INT};
use crate::ast::{BinOp, Expr, Literal, Param, UnOp};
use crate::types::{PrimitiveType, Type};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SmtOutcome {
    Proved,
    Unknown(String),
}

pub(super) fn prove_with_z3(
    params: &[Param],
    requires: &[Expr],
    ensures: &[Expr],
    wrapped_body: &Expr,
) -> SmtOutcome {
    prove_with_oracle(params, requires, ensures, wrapped_body, run_z3)
}

fn prove_with_oracle(
    params: &[Param],
    requires: &[Expr],
    ensures: &[Expr],
    wrapped_body: &Expr,
    oracle: impl FnOnce(&str) -> Result<String, String>,
) -> SmtOutcome {
    let Some(script) = build_query(params, requires, ensures, wrapped_body) else {
        return SmtOutcome::Unknown(
            "SMT proof requires pure, bounded, overflow-safe Int expressions and supported preconditions".to_owned(),
        );
    };
    match oracle(&script) {
        Ok(output) if output.trim() == "unsat" => SmtOutcome::Proved,
        Ok(output) if output.trim() == "sat" => SmtOutcome::Unknown(
            "SMT found a possible postcondition violation; no validated witness was produced"
                .to_owned(),
        ),
        Ok(output) if output.trim() == "unknown" => {
            SmtOutcome::Unknown("SMT solver could not decide the proof obligation".to_owned())
        }
        Ok(_) => {
            SmtOutcome::Unknown("SMT solver response was not a verified unsat result".to_owned())
        }
        Err(reason) => SmtOutcome::Unknown(reason),
    }
}

fn smt_int(value: i128) -> String {
    if value < 0 {
        format!("(- {})", -value)
    } else {
        value.to_string()
    }
}

fn build_query(
    params: &[Param],
    requires: &[Expr],
    ensures: &[Expr],
    wrapped_body: &Expr,
) -> Option<String> {
    if ensures.is_empty() || params.len() > 64 {
        return None;
    }
    let mut bounds = Bounds::new();
    let mut symbols: HashMap<String, String> = HashMap::new();
    for (index, param) in params.iter().enumerate() {
        if !matches!(param.ty.as_ref(), Some(Type::Primitive(PrimitiveType::Int)))
            || bounds.contains_key(&param.name)
        {
            return None;
        }
        bounds.insert(
            param.name.clone(),
            Interval {
                lo: MIN_INT,
                hi: MAX_INT,
            },
        );
        symbols.insert(param.name.clone(), format!("p{index}"));
    }
    for condition in requires {
        if !constrain(condition, &mut bounds) {
            return None; // includes contradictory assumptions and unsupported predicates
        }
    }

    let input_bounds = bounds.clone();
    let body = original_body(wrapped_body, !requires.is_empty())?;
    let result_range = eval_int(body, &bounds)?;
    let mut local_id = 0;
    let value = int_expr(body, &bounds, &symbols, &mut local_id)?;

    // Postconditions observe the return value through the special 'result'
    // binding, shadowing any parameter with that name.
    symbols.insert("result".to_owned(), "contract_result".to_owned());
    bounds.insert("result".to_owned(), result_range);
    let mut checks = Vec::with_capacity(ensures.len());
    for condition in ensures {
        checks.push(bool_expr(condition, &bounds, &symbols, &mut local_id)?);
    }

    let mut script = String::from("(set-option :print-success false)\n(set-logic QF_LIA)\n");
    for (index, param) in params.iter().enumerate() {
        let var = format!("p{index}");
        let range = input_bounds.get(&param.name)?;
        script.push_str(&format!("(declare-fun {var} () Int)\n"));
        script.push_str(&format!("(assert (>= {var} {}))\n", smt_int(range.lo)));
        script.push_str(&format!("(assert (<= {var} {}))\n", smt_int(range.hi)));
    }
    script.push_str(&format!("(define-fun contract_result () Int {value})\n"));
    let condition = if checks.len() == 1 {
        checks.pop()?
    } else {
        format!("(and {})", checks.join(" "))
    };
    script.push_str(&format!(
        "(assert (not {condition}))\n(check-sat)\n(exit)\n"
    ));

    // No large generated scripts or expressions that can make the optional
    // verifier an unexpected source of latency in developer workflows.
    (script.len() <= 8192).then_some(script)
}

fn int_expr(
    expr: &Expr,
    bounds: &Bounds,
    symbols: &HashMap<String, String>,
    next_id: &mut usize,
) -> Option<String> {
    // Never pass an integer subexpression to SMT unless conservative
    // interval analysis proves all values and intermediates are in range.
    eval_int(expr, bounds)?;
    if let Some(value) = int_literal(expr) {
        return Some(smt_int(value));
    }
    match expr {
        Expr::Var(name, _) => symbols.get(name).cloned(),
        Expr::Binary {
            op: BinOp::Add | BinOp::Sub,
            left,
            right,
            ..
        } => {
            let op = if matches!(expr, Expr::Binary { op: BinOp::Add, .. }) {
                "+"
            } else {
                "-"
            };
            let lhs = int_expr(left, bounds, symbols, next_id)?;
            let rhs = int_expr(right, bounds, symbols, next_id)?;
            Some(format!("({op} {lhs} {rhs})"))
        }
        Expr::Unary {
            op: UnOp::Neg,
            expr,
            ..
        } => Some(format!("(- {})", int_expr(expr, bounds, symbols, next_id)?)),
        Expr::Let {
            name,
            value,
            body,
            mutable: false,
            ..
        } => {
            let range = eval_int(value, bounds)?;
            let expr_value = int_expr(value, bounds, symbols, next_id)?;
            let local_name = format!("local{}", *next_id);
            *next_id += 1;
            let mut scoped_bounds = bounds.clone();
            scoped_bounds.insert(name.clone(), range);
            let mut scoped_symbols = symbols.clone();
            scoped_symbols.insert(name.clone(), local_name.clone());
            let expr_body = int_expr(body, &scoped_bounds, &scoped_symbols, next_id)?;
            Some(format!("(let (({local_name} {expr_value})) {expr_body})"))
        }
        Expr::Block { exprs, .. } if exprs.len() == 1 => {
            int_expr(&exprs[0], bounds, symbols, next_id)
        }
        _ => None,
    }
}

fn bool_expr(
    expr: &Expr,
    bounds: &Bounds,
    symbols: &HashMap<String, String>,
    next_id: &mut usize,
) -> Option<String> {
    match expr {
        Expr::Literal(Literal::Bool(true), _) => Some("true".to_owned()),
        Expr::Literal(Literal::Bool(false), _) => Some("false".to_owned()),
        Expr::Unary {
            op: UnOp::Not,
            expr,
            ..
        } => Some(format!(
            "(not {})",
            bool_expr(expr, bounds, symbols, next_id)?
        )),
        Expr::Binary {
            op: BinOp::And | BinOp::Or,
            left,
            right,
            ..
        } => {
            // Both sides must be modelable even where runtime would short-circuit.
            let lhs = bool_expr(left, bounds, symbols, next_id)?;
            let rhs = bool_expr(right, bounds, symbols, next_id)?;
            let op = if matches!(expr, Expr::Binary { op: BinOp::And, .. }) {
                "and"
            } else {
                "or"
            };
            Some(format!("({op} {lhs} {rhs})"))
        }
        Expr::Binary {
            op, left, right, ..
        } => {
            let lhs = int_expr(left, bounds, symbols, next_id)?;
            let rhs = int_expr(right, bounds, symbols, next_id)?;
            let op = match op {
                BinOp::Eq => "=",
                BinOp::Ne => "distinct",
                BinOp::Lt => "<",
                BinOp::Le => "<=",
                BinOp::Gt => ">",
                BinOp::Ge => ">=",
                _ => return None,
            };
            Some(format!("({op} {lhs} {rhs})"))
        }
        _ => None,
    }
}

fn run_z3(query: &str) -> Result<String, String> {
    let mut child = Command::new("z3")
        .args(["-smt2", "-in", "-T:2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Z3 is unavailable; install Z3 or run without --solver z3".to_owned())?;
    {
        let mut input = child.stdin.take().ok_or("Z3 stdin unavailable")?;
        input
            .write_all(query.as_bytes())
            .map_err(|e| format!("cannot send SMT query: {e}"))?;
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|e| format!("cannot read Z3 result: {e}"))?;
                if !status.success() {
                    return Err(format!("Z3 exited with status {status}"));
                }
                return String::from_utf8(output.stdout)
                    .map_err(|_| "Z3 returned invalid UTF-8".to_owned());
            }
            Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Z3 timed out; proof remains unknown".to_owned());
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Z3 process failed: {e}"));
            }
        }
    }
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
    fn all_parameter_bounds_are_encoded_in_one_satisfiable_model() {
        let source = "fn add(x: Int, y: Int) -> Int requires x >= 0 && x <= 5 && y >= 1 && y <= 8 ensures result == x + y { x + y }";
        let result = analyze(source, |script| {
            assert!(script.contains("(declare-fun p0 () Int)"));
            assert!(script.contains("(declare-fun p1 () Int)"));
            assert!(script.contains("(assert (>= p0 0))"));
            assert!(script.contains("(assert (<= p0 5))"));
            assert!(script.contains("(assert (>= p1 1))"));
            assert!(script.contains("(assert (<= p1 8))"));
            Ok("unsat".to_owned())
        });
        assert_eq!(result, SmtOutcome::Proved);
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
    fn real_z3_verifies_correlation_only_when_explicitly_requested() {
        if std::env::var_os("NULANG_TEST_Z3").is_none() {
            return; // CI does not need an installed solver for normal tests.
        }
        let source =
            "fn inc(x: Int) -> Int requires x >= 0 && x <= 10 ensures result == x + 1 { x + 1 }";
        let tokens = Lexer::new(source).lex().expect("valid tokens");
        let ast = Parser::new(tokens).parse_module().expect("valid source");
        let Decl::Function {
            params,
            requires,
            ensures,
            body,
            ..
        } = &ast.decls[0]
        else {
            panic!("expected function");
        };
        assert_eq!(
            prove_with_z3(params, requires, ensures, body),
            SmtOutcome::Proved
        );
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
