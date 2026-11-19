//! CLI actions for inspecting and exercising state-machine declarations.
//!
//! Called by main before generic flag parsing; also independently unit-testable.

use crate::ast::Decl;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::statechart::analyze_machine;
use crate::statechart_testing::{generate_transition_tests, run_transition_tests, TestFixtures};

pub const MACHINE_USAGE: &str = "\
Usage:
  nulang machine graph <file.nula> [--machine NAME] [--format mermaid|json] [--out FILE]
  nulang machine paths <file.nula> [--machine NAME] [--fixtures FILE.json] [--out FILE]
  nulang machine test  <file.nula> [--machine NAME] [--fixtures FILE.json]

Fixtures JSON: { \"pay\": [42], \"cancel\": [] }
'paths' generates runnable test cases. 'test' executes them using real
bytecode actors. Missing or unsupported fixtures cause a nonzero exit:
incomplete edge coverage is not reported as success.
State-machine syntax is experimental pending RFC 0026 approval.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Graph,
    Paths,
    Test,
}

#[derive(Debug)]
struct Options {
    action: Action,
    file: String,
    machine: Option<String>,
    format: Option<String>,
    output: Option<String>,
    fixtures_path: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    if args.len() < 2 || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        return Err(MACHINE_USAGE.into());
    }
    let action = match args[0].as_str() {
        "graph" => Action::Graph,
        "paths" => Action::Paths,
        "test" => Action::Test,
        other => return Err(format!("unknown machine action '{other}'\n{MACHINE_USAGE}")),
    };
    if args[1].starts_with('-') {
        return Err(format!(
            "machine action requires a source file\n{MACHINE_USAGE}"
        ));
    }
    let mut opts = Options {
        action,
        file: args[1].clone(),
        machine: None,
        format: None,
        output: None,
        fixtures_path: None,
    };
    let mut i = 2;
    while i < args.len() {
        let key = &args[i];
        i += 1;
        if i >= args.len() {
            return Err(format!("{key} requires a value"));
        }
        let value = args[i].clone();
        match key.as_str() {
            "--machine" => opts.machine = Some(value),
            "--format" => opts.format = Some(value),
            "--out" => opts.output = Some(value),
            "--fixtures" => opts.fixtures_path = Some(value),
            other => return Err(format!("unknown machine option '{other}'")),
        }
        i += 1;
    }

    if action != Action::Graph && opts.format.is_some() {
        return Err("--format is only supported for 'machine graph'".into());
    }
    if action == Action::Graph && opts.fixtures_path.is_some() {
        return Err("fixtures apply only to 'machine paths' or 'machine test'".into());
    }
    if action == Action::Test && opts.output.is_some() {
        return Err("'machine test' prints results; --out is not supported".into());
    }
    if let Some(format) = &opts.format {
        if format != "mermaid" && format != "json" {
            return Err("machine graph --format must be 'mermaid' or 'json'".into());
        }
    }
    if opts.output.as_deref() == Some(opts.file.as_str()) {
        return Err("refusing to overwrite the machine source file".into());
    }
    Ok(opts)
}

/// Dispatch the machine subcommand, returning an output string for main.
pub fn run(args: &[String]) -> Result<String, String> {
    let opts = parse_args(args)?;
    let source = std::fs::read_to_string(&opts.file)
        .map_err(|e| format!("cannot read '{}': {e}", opts.file))?;
    if let Some(output) = &opts.output {
        if let (Ok(source_path), Ok(output_path)) = (
            std::fs::canonicalize(&opts.file),
            std::fs::canonicalize(output),
        ) {
            if source_path == output_path {
                return Err("refusing to overwrite the machine source file".into());
            }
        }
    }
    let fixtures = match &opts.fixtures_path {
        Some(path) => {
            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read fixtures '{}': {e}", path))?;
            serde_json::from_str::<TestFixtures>(&content)
                .map_err(|e| format!("invalid fixtures '{}': {e}", path))?
        }
        None => TestFixtures::new(),
    };
    let result = render_from_source(
        &source,
        opts.action,
        opts.machine.as_deref(),
        opts.format.as_deref(),
        &fixtures,
    )?;
    if let Some(output) = &opts.output {
        std::fs::write(output, &result).map_err(|e| format!("cannot write '{}': {e}", output))?;
        return Ok(format!("Wrote state-machine output to {output}"));
    }
    Ok(result)
}

/// In-memory implementation exercised by CLI unit tests without filesystem I/O.
fn render_from_source(
    source: &str,
    action: Action,
    machine_name: Option<&str>,
    format: Option<&str>,
    fixtures: &TestFixtures,
) -> Result<String, String> {
    let mut lexer = Lexer::new(source);
    let mut parser = Parser::new(lexer.lex().map_err(|e| e.to_string())?);
    let module = parser.parse_module().map_err(|e| e.to_string())?;
    let machines: Vec<&Decl> = module
        .decls
        .iter()
        .filter(|decl| matches!(decl, Decl::StateMachine { .. }))
        .collect();
    let machine = match machine_name {
        Some(requested) => machines
            .into_iter()
            .find(|decl| matches!(decl, Decl::StateMachine { name, .. } if name == requested))
            .ok_or_else(|| format!("machine '{requested}' not found"))?,
        None if machines.len() == 1 => machines[0],
        None if machines.is_empty() => return Err("no state_machine declaration found".into()),
        None => return Err("multiple state machines: select one using --machine NAME".into()),
    };

    let graph = analyze_machine(machine).ok_or("machine has no initial state")?;
    match action {
        Action::Graph if format.unwrap_or("mermaid") == "mermaid" => Ok(graph.mermaid),
        Action::Graph => {
            let paths: Vec<_> = graph
                .shortest_paths
                .iter()
                .map(|p| serde_json::json!({"state": p.state, "events": p.events}))
                .collect();
            let mut edges = Vec::new();
            if let Decl::StateMachine { states, events, .. } = machine {
                for event in events {
                    let sources: Vec<&str> = match event.source.as_deref() {
                        Some(source) => vec![source],
                        None => states.iter().map(String::as_str).collect(),
                    };
                    for source in sources {
                        edges.push(serde_json::json!({
                            "source": source,
                            "target": event.target,
                            "event": event.name,
                            "guarded": event.guard.is_some(),
                        }));
                    }
                }
            }
            serde_json::to_string_pretty(&serde_json::json!({
                "name": graph.name,
                "initial": graph.initial,
                "states": graph.states,
                "unreachable": graph.unreachable,
                "shortest_paths": paths,
                "edges": edges,
            }))
            .map_err(|e| e.to_string())
        }
        Action::Paths => {
            let plan = generate_transition_tests(machine, fixtures)?;
            let cases: Vec<_> = plan
                .cases
                .iter()
                .map(|c| {
                    let steps: Vec<_> = c
                        .sequence
                        .iter()
                        .map(|e| serde_json::json!({"event":e.event, "args": e.args}))
                        .collect();
                    serde_json::json!({
                        "source": c.source, "event": c.event,
                        "accepted": c.accepted, "expected": c.expected, "sequence": steps,
                    })
                })
                .collect();
            let skipped: Vec<_> = plan
                .skipped
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "source": s.source, "event": s.event, "reason": s.reason,
                    })
                })
                .collect();
            serde_json::to_string_pretty(&serde_json::json!({
                "machine": plan.machine, "cases": cases,
                "skipped": skipped, "unreachable": plan.unreachable,
                "coverage": {"runnable": plan.cases.len(), "skipped": plan.skipped.len()}
            }))
            .map_err(|e| e.to_string())
        }
        Action::Test => {
            let plan = generate_transition_tests(machine, fixtures)?;
            let report = run_transition_tests(machine, &plan);
            let mut output = format!(
                "machine {}: {} passed, {} failed, {} skipped, {} unreachable state(s)\n",
                plan.machine,
                report.passed,
                report.failed.len(),
                plan.skipped.len(),
                plan.unreachable.len(),
            );
            for failed in &report.failed {
                output.push_str(&format!(
                    "FAIL {} --{}--> ?: {}\n",
                    failed.source, failed.event, failed.reason
                ));
            }
            for skipped in &plan.skipped {
                output.push_str(&format!(
                    "SKIP {} --{}--> ?: {}\n",
                    skipped.source, skipped.event, skipped.reason
                ));
            }
            if plan.cases.is_empty() || !report.failed.is_empty() || !plan.skipped.is_empty() {
                Err(format!(
                    "incomplete or failing machine test coverage\n{output}"
                ))
            } else {
                Ok(output)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACHINE: &str = r#"
        state_machine Order {
            state Pending
            state Paid
            event pay(amount: Int) from Pending: Paid if amount > 0
            event reset: Pending
        }
    "#;

    #[test]
    fn graph_mermaid_and_json_export() {
        let mermaid =
            render_from_source(MACHINE, Action::Graph, None, None, &TestFixtures::new()).unwrap();
        assert!(mermaid.starts_with("stateDiagram-v2"));
        assert!(mermaid.contains("Pending --> Paid : pay [guard]"));
        let json = render_from_source(
            MACHINE,
            Action::Graph,
            None,
            Some("json"),
            &TestFixtures::new(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["initial"], "Pending");
        assert_eq!(value["name"], "Order");
        assert_eq!(value["edges"].as_array().unwrap().len(), 3);
        assert_eq!(value["edges"][0]["event"], "pay");
    }

    #[test]
    fn paths_show_unconfigured_transitions_as_skipped() {
        let paths =
            render_from_source(MACHINE, Action::Paths, None, None, &TestFixtures::new()).unwrap();
        let json: serde_json::Value = serde_json::from_str(&paths).unwrap();
        assert_eq!(json["coverage"]["skipped"], 3);
        assert_eq!(json["coverage"]["runnable"], 1);
    }

    #[test]
    fn machine_test_fails_closed_without_fixtures() {
        let err = render_from_source(MACHINE, Action::Test, None, None, &TestFixtures::new())
            .unwrap_err();
        assert!(err.contains("skipped"), "{err}");
    }

    #[test]
    fn machine_test_executes_guarded_case_with_fixture() {
        let fixtures = TestFixtures::from([("pay".into(), vec![serde_json::json!(1)])]);
        let result = render_from_source(MACHINE, Action::Test, None, None, &fixtures).unwrap();
        assert!(result.contains("0 failed"), "{result}");
        assert!(result.contains("0 skipped"), "{result}");
    }

    #[test]
    fn ambiguous_machine_requires_selection() {
        let both = format!("{}\nstate_machine Other {{ state A }}", MACHINE);
        let err =
            render_from_source(&both, Action::Graph, None, None, &TestFixtures::new()).unwrap_err();
        assert!(err.contains("--machine NAME"), "{err}");
        assert!(render_from_source(
            &both,
            Action::Graph,
            Some("Other"),
            None,
            &TestFixtures::new()
        )
        .unwrap()
        .contains("state A"));
    }

    #[test]
    fn cli_rejects_overwriting_source_and_bad_formats() {
        let args = ["graph", "order.nula", "--out", "order.nula"].map(str::to_owned);
        assert!(parse_args(&args).unwrap_err().contains("overwrite"));
        let args = ["graph", "order.nula", "--format", "yaml"].map(str::to_owned);
        assert!(parse_args(&args).unwrap_err().contains("--format"));
    }
}
