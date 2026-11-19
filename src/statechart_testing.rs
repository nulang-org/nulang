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
                reason,
            }),
        }
    }
    report
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
