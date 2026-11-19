//! Model-based testing of the existing state-machine actor semantics.
//!
//! Generate shortest executable transition-edge paths and run each case with
//! a fresh bytecode VM + real actor runtime. No new scheduler is introduced.
//! Guard feasibility is checked by executing code with explicit test fixtures,
//! never assumed from structural reachability.

use std::collections::{HashMap, VecDeque};

use crate::ast::{Decl, StateMachineEvent};
use crate::statechart::analyze_machine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestEvent {
    pub event: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionTestCase {
    pub source: String,
    pub event: String,
    /// True for accepted transitions, false for wrong-source rejection tests.
    pub accepted: bool,
    pub expected: String,
    pub sequence: Vec<TestEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedTransition {
    pub source: String,
    pub event: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineTestPlan {
    pub machine: String,
    pub cases: Vec<TransitionTestCase>,
    pub skipped: Vec<SkippedTransition>,
    pub unreachable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTransitionTest {
    pub source: String,
    pub event: String,
    /// Exact ordered event sequence and fixture literals to replay the failure.
    pub sequence: Vec<TestEvent>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineTestReport {
    pub passed: usize,
    pub failed: Vec<FailedTransitionTest>,
}

/// Per-event argument fixtures: { "pay": [42, "USD"], "cancel": [] }.
/// Guarded transitions are actually executed with these literals; a false
/// guard is a test failure, not a fabricated successful path.
pub type TestFixtures = HashMap<String, Vec<serde_json::Value>>;

/// Generate an edge-covering suite using shortest *executable* prefixes.
///
/// Unsupported/unconfigured paths are explicitly skipped, never passed.
/// Limits bound the graph's potential global event fan-out. A path is an
/// *expected* sequence; a guarded edge may reject its fixture at runtime.
pub fn generate_transition_tests(
    decl: &Decl,
    fixtures: &TestFixtures,
) -> Result<MachineTestPlan, String> {
    const MAX_STATES: usize = 256;
    const MAX_EDGES: usize = 1024;

    let Decl::StateMachine {
        name,
        states,
        events,
        ..
    } = decl
    else {
        return Err("expected state_machine declaration".into());
    };
    let initial = states.first().ok_or("state machine has no initial state")?;
    if states.len() > MAX_STATES || events.len().saturating_mul(states.len()) > MAX_EDGES {
        return Err(format!(
            "machine exceeds test generation limit ({} states or {} possible edges)",
            MAX_STATES, MAX_EDGES
        ));
    }
    for fixture_name in fixtures.keys() {
        if !events.iter().any(|event| &event.name == fixture_name) {
            return Err(format!("fixture for undeclared event '{}'", fixture_name));
        }
    }

    let invocations: HashMap<&str, Result<TestEvent, String>> = events
        .iter()
        .map(|event| (event.name.as_str(), fixture_event(event, fixtures)))
        .collect();

    let mut paths: HashMap<String, Vec<TestEvent>> = HashMap::new();
    let mut queue = VecDeque::new();
    paths.insert(initial.clone(), Vec::new());
    queue.push_back(initial.clone());

    while let Some(state) = queue.pop_front() {
        for event in events {
            if event.source.as_ref().is_some_and(|source| source != &state) {
                continue;
            }
            if paths.contains_key(&event.target) {
                continue;
            }
            if let Some(Ok(step)) = invocations.get(event.name.as_str()) {
                let mut path = paths[&state].clone();
                path.push(step.clone());
                paths.insert(event.target.clone(), path);
                queue.push_back(event.target.clone());
            }
        }
    }

    let mut cases = Vec::new();
    let mut skipped = Vec::new();
    for source in states {
        for event in events {
            // Also generate negative tests for source-scoped events delivered
            // while another state is active. These must be harmless no-ops.
            let accepted = event.source.as_ref().is_none_or(|s| s == source);
            let Some(prefix) = paths.get(source) else {
                skipped.push(SkippedTransition {
                    source: source.clone(),
                    event: event.name.clone(),
                    reason:
                        "no executable path to source state; supply fixtures for preceding events"
                            .into(),
                });
                continue;
            };
            match &invocations[event.name.as_str()] {
                Ok(step) => {
                    let mut sequence = prefix.clone();
                    sequence.push(step.clone());
                    cases.push(TransitionTestCase {
                        source: source.clone(),
                        event: event.name.clone(),
                        accepted,
                        expected: if accepted {
                            event.target.clone()
                        } else {
                            source.clone()
                        },
                        sequence,
                    });
                }
                Err(reason) => skipped.push(SkippedTransition {
                    source: source.clone(),
                    event: event.name.clone(),
                    reason: reason.clone(),
                }),
            }
        }
    }

    Ok(MachineTestPlan {
        machine: name.clone(),
        cases,
        skipped,
        unreachable: analyze_machine(decl)
            .map(|a| a.unreachable)
            .unwrap_or_default(),
    })
}

fn fixture_literal(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => Some(value.to_string()),
        serde_json::Value::String(s) => serde_json::to_string(s).ok(),
        _ => None,
    }
}

fn fixture_event(event: &StateMachineEvent, fixtures: &TestFixtures) -> Result<TestEvent, String> {
    let values = fixtures.get(&event.name);
    if values.is_none() && !event.params.is_empty() {
        return Err(format!(
            "event '{}' needs {} argument fixture(s)",
            event.name,
            event.params.len()
        ));
    }
    let values = values.map(Vec::as_slice).unwrap_or_default();
    if values.len() != event.params.len() {
        return Err(format!(
            "event '{}' needs {} argument fixture(s), got {}",
            event.name,
            event.params.len(),
            values.len()
        ));
    }
    let mut args = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        args.push(fixture_literal(value).ok_or_else(|| {
            format!(
                "event '{}' argument {} must be a number, boolean, or string literal",
                event.name,
                index + 1
            )
        })?);
    }
    Ok(TestEvent {
        event: event.name.clone(),
        args,
    })
}

/// Execute generated cases against the actual bytecode actor implementation.
/// Every test has a fresh runtime and no state leakage between scenarios.
pub fn run_transition_tests(decl: &Decl, plan: &MachineTestPlan) -> MachineTestReport {
    let mut report = MachineTestReport {
        passed: 0,
        failed: Vec::new(),
    };
    for case in &plan.cases {
        match execute_case(decl, case) {
            Ok(()) => report.passed += 1,
            Err(reason) => report.failed.push(FailedTransitionTest {
                source: case.source.clone(),
                event: case.event.clone(),
                sequence: case.sequence.clone(),
                reason,
            }),
        }
    }
    report
}

/// Deterministic *suggestions*, not proof of guard satisfiability.
/// Simple primitive comparisons are supported; complex or untyped guards
/// require explicit user fixtures. The actual runner still executes guards.
pub fn suggest_fixtures(decl: &Decl) -> Result<TestFixtures, String> {
    use crate::types::{PrimitiveType, Type};
    let Decl::StateMachine { events, .. } = decl else {
        return Err("expected state_machine declaration".into());
    };
    let mut result = TestFixtures::new();
    for event in events {
        let mut values = Vec::new();
        for param in &event.params {
            let value = match param.ty.as_ref() {
                Some(Type::Primitive(PrimitiveType::Int)) => serde_json::json!(0),
                Some(Type::Primitive(PrimitiveType::Float)) => serde_json::json!(0.0),
                Some(Type::Primitive(PrimitiveType::Bool)) => serde_json::json!(false),
                Some(Type::Primitive(PrimitiveType::String)) => serde_json::json!(""),
                _ => {
                    return Err(format!(
                        "event '{}' requires manual fixture for untyped or complex parameter '{}'",
                        event.name, param.name
                    ))
                }
            };
            values.push(value);
        }
        if let Some(guard) = &event.guard {
            let manual = || {
                format!(
                "event '{}' requires manual fixture: guard is not a supported simple comparison",
                event.name
            )
            };
            match guard {
                crate::ast::Expr::Var(name, _) => {
                    let idx = event
                        .params
                        .iter()
                        .position(|p| &p.name == name)
                        .ok_or_else(manual)?;
                    if !matches!(
                        event.params[idx].ty,
                        Some(Type::Primitive(PrimitiveType::Bool))
                    ) {
                        return Err(manual());
                    }
                    values[idx] = serde_json::json!(true);
                }
                crate::ast::Expr::Unary {
                    op: crate::ast::UnOp::Not,
                    expr,
                    ..
                } => {
                    let crate::ast::Expr::Var(name, _) = expr.as_ref() else {
                        return Err(manual());
                    };
                    let idx = event
                        .params
                        .iter()
                        .position(|p| &p.name == name)
                        .ok_or_else(manual)?;
                    if !matches!(
                        event.params[idx].ty,
                        Some(Type::Primitive(PrimitiveType::Bool))
                    ) {
                        return Err(manual());
                    }
                    values[idx] = serde_json::json!(false);
                }
                crate::ast::Expr::Binary {
                    op, left, right, ..
                } => {
                    use crate::ast::{BinOp, Expr, Literal};
                    let (name, literal, operator) = match (left.as_ref(), right.as_ref()) {
                        (Expr::Var(name, _), Expr::Literal(lit, _)) => (name, lit, *op),
                        (Expr::Literal(lit, _), Expr::Var(name, _)) => {
                            let flipped = match op {
                                BinOp::Lt => BinOp::Gt,
                                BinOp::Le => BinOp::Ge,
                                BinOp::Gt => BinOp::Lt,
                                BinOp::Ge => BinOp::Le,
                                BinOp::Eq => BinOp::Eq,
                                BinOp::Ne => BinOp::Ne,
                                _ => return Err(manual()),
                            };
                            (name, lit, flipped)
                        }
                        _ => return Err(manual()),
                    };
                    let idx = event
                        .params
                        .iter()
                        .position(|p| &p.name == name)
                        .ok_or_else(manual)?;
                    let suggested = match (event.params[idx].ty.as_ref(), literal, operator) {
                        (Some(Type::Primitive(PrimitiveType::Int)), Literal::Int(n), op) => {
                            let n = match op {
                                BinOp::Eq | BinOp::Le | BinOp::Ge => Some(*n),
                                BinOp::Gt => n.checked_add(1),
                                BinOp::Lt => n.checked_sub(1),
                                BinOp::Ne => Some(if *n == 0 { 1 } else { 0 }),
                                _ => None,
                            }
                            .ok_or_else(manual)?;
                            serde_json::json!(n)
                        }
                        (
                            Some(Type::Primitive(PrimitiveType::Bool)),
                            Literal::Bool(b),
                            BinOp::Eq,
                        ) => serde_json::json!(b),
                        (
                            Some(Type::Primitive(PrimitiveType::Bool)),
                            Literal::Bool(b),
                            BinOp::Ne,
                        ) => serde_json::json!(!b),
                        (
                            Some(Type::Primitive(PrimitiveType::String)),
                            Literal::String(s),
                            BinOp::Eq,
                        ) => serde_json::json!(s),
                        (
                            Some(Type::Primitive(PrimitiveType::String)),
                            Literal::String(s),
                            BinOp::Ne,
                        ) => serde_json::json!(if s.is_empty() { "x" } else { "" }),
                        _ => return Err(manual()),
                    };
                    values[idx] = suggested;
                }
                _ => return Err(manual()),
            }
        }
        result.insert(event.name.clone(), values);
    }
    Ok(result)
}

#[cfg(test)]
mod fixture_tests {
    use super::*;
    use crate::{lexer::Lexer, parser::Parser};

    fn machine(source: &str) -> Decl {
        let mut lexer = Lexer::new(source);
        Parser::new(lexer.lex().unwrap())
            .parse_module()
            .unwrap()
            .decls
            .remove(0)
    }

    #[test]
    fn suggests_deterministic_guard_satisfying_integer_fixture() {
        let decl = machine("state_machine M { state Pending\n state Paid\n event pay(amount: Int) from Pending: Paid if amount > 0 }");
        let fixtures = suggest_fixtures(&decl).unwrap();
        assert_eq!(fixtures["pay"], vec![serde_json::json!(1)]);
        let plan = generate_transition_tests(&decl, &fixtures).unwrap();
        let report = run_transition_tests(&decl, &plan);
        assert!(report.failed.is_empty(), "{report:?}");
        assert!(report.passed > 0);
    }

    #[test]
    fn refuses_guards_that_cannot_be_proven_satisfiable() {
        let decl = machine(
            "state_machine M { state A\n state B\n event go(x: Int) from A: B if x > 0 && x < 0 }",
        );
        assert!(suggest_fixtures(&decl)
            .unwrap_err()
            .contains("manual fixture"));
    }

    #[test]
    fn fixtures_reproducible_and_keep_declared_event_names() {
        let decl = machine("state_machine M { state A\n state B\n event reset: A\n event toggle(ok: Bool) from A: B if ok }");
        let first = suggest_fixtures(&decl).unwrap();
        assert_eq!(first["reset"], Vec::<serde_json::Value>::new());
        assert_eq!(first["toggle"], vec![serde_json::json!(true)]);
        assert_eq!(first, suggest_fixtures(&decl).unwrap());
    }
}

fn execute_case(decl: &Decl, case: &TransitionTestCase) -> Result<(), String> {
    use std::{cell::RefCell, rc::Rc};

    use crate::ast::AstModule;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::runtime::{PersistedValue, Runtime, RuntimeVmCallbacks};
    use crate::typechecker::TypeChecker;
    use crate::vm::VM;

    let Decl::StateMachine { name, .. } = decl else {
        return Err("expected state_machine declaration".into());
    };

    let mut sends = String::new();
    for step in &case.sequence {
        sends.push_str(&format!(
            "send __statechart_test_actor {}({})\n",
            step.event,
            step.args.join(", ")
        ));
    }
    let entry = format!(
        "fn main() {{\nlet __statechart_test_actor = spawn {} {{}} in {{\n{}__statechart_test_actor\n}}\n}}",
        name, sends
    );
    let mut lexer = Lexer::new(&entry);
    let mut parser = Parser::new(lexer.lex().map_err(|e| e.to_string())?);
    let entry_ast = parser.parse_module().map_err(|e| e.to_string())?;
    let mut module = AstModule {
        name: "statechart_test".into(),
        decls: vec![decl.clone()],
    };
    module.decls.extend(entry_ast.decls);

    let mut typechecker = TypeChecker::new();
    typechecker
        .check_module(&module)
        .map_err(|e| e.to_string())?;
    let hir = crate::hir_lower::lower_module(&module, &typechecker.inferred_decl_types);
    let mut mir = crate::mir_lower::lower_module(&hir).map_err(|e| e.to_string())?;
    let bytecode =
        crate::mir_codegen::compile_mir(&mut mir, "statechart_test").map_err(|e| e.to_string())?;

    let runtime = Rc::new(RefCell::new(Runtime::new()));
    runtime.borrow_mut().register_module_grains(&bytecode);
    let mut vm = VM::new();
    vm.load_module(bytecode);
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime.clone())));
    let actor_ref = vm.run().map_err(|e| e.to_string())?;
    let id = actor_ref
        .as_actor_id()
        .ok_or("test main did not return actor reference")?;
    runtime.borrow_mut().run_scheduler();

    let rt = runtime.borrow();
    let actor = rt.actors.get(&id).ok_or("test actor terminated")?;
    let state = actor
        .get_state_field("_sm_state")
        .ok_or("state field missing")?;
    match actor.persist_value(&state) {
        PersistedValue::String(ref state) if state == &case.expected => Ok(()),
        other => Err(format!("expected {}, got {:?}", case.expected, other)),
    }
}
