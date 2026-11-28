//! Static state-machine graph inspection.
//!
//! Guards are treated as potentially satisfiable for reachability analysis.
//! Paths are structural witnesses, not proofs that runtime guard values permit
//! traversal. No runtime dependency or new language/bytecode construct.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::ast::{Decl, StateMachineEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePath {
    pub state: String,
    pub events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineAnalysis {
    pub name: String,
    pub initial: String,
    pub states: Vec<String>,
    pub unreachable: Vec<String>,
    /// One shortest event sequence from the initial state to each reachable state.
    /// Guard predicates are assumed satisfiable.
    pub shortest_paths: Vec<StatePath>,
    pub mermaid: String,
}

/// Analyze a parsed state-machine declaration, returning None for other declarations.
pub fn analyze_machine(decl: &Decl) -> Option<MachineAnalysis> {
    let Decl::StateMachine {
        name,
        states,
        events,
        ..
    } = decl
    else {
        return None;
    };
    let initial = states.first()?.clone();

    let mut visited: HashSet<String> = HashSet::new();
    let mut paths: HashMap<String, Vec<String>> = HashMap::new();
    let mut queue = VecDeque::new();
    visited.insert(initial.clone());
    paths.insert(initial.clone(), Vec::new());
    queue.push_back(initial.clone());

    while let Some(state) = queue.pop_front() {
        for event in events {
            if event
                .source
                .as_deref()
                .is_some_and(|source| source != state)
            {
                continue;
            }
            if visited.insert(event.target.clone()) {
                let mut path = paths[&state].clone();
                path.push(event.name.clone());
                paths.insert(event.target.clone(), path);
                queue.push_back(event.target.clone());
            }
        }
    }

    let unreachable = states
        .iter()
        .filter(|state| !visited.contains(*state))
        .cloned()
        .collect();
    let shortest_paths = states
        .iter()
        .filter_map(|state| {
            paths.get(state).map(|events| StatePath {
                state: state.clone(),
                events: events.clone(),
            })
        })
        .collect();

    Some(MachineAnalysis {
        name: name.clone(),
        initial: initial.clone(),
        states: states.clone(),
        unreachable,
        shortest_paths,
        mermaid: mermaid_graph(&initial, states, events),
    })
}

fn mermaid_graph(initial: &str, states: &[String], events: &[StateMachineEvent]) -> String {
    let mut out = format!("stateDiagram-v2\n    [*] --> {}\n", initial);
    for state in states {
        out.push_str(&format!("    state {}\n", state));
    }
    for event in events {
        let sources: Vec<&str> = match &event.source {
            Some(source) => vec![source.as_str()],
            None => states.iter().map(String::as_str).collect(),
        };
        for source in sources {
            let guarded = if event.guard.is_some() {
                " [guard]"
            } else {
                ""
            };
            out.push_str(&format!(
                "    {} --> {} : {}{}\n",
                source, event.target, event.name, guarded
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer::Lexer, parser::Parser};

    fn parse_machine(source: &str) -> Decl {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        Parser::new(tokens).parse_module().unwrap().decls.remove(0)
    }

    #[test]
    fn analysis_finds_unreachable_states_and_shortest_paths() {
        let machine = parse_machine(
            r#"
            state_machine Order {
                state Pending
                state Reserved
                state Paid
                state Orphan
                event reserve from Pending: Reserved
                event pay from Reserved: Paid
            }
        "#,
        );
        let report = analyze_machine(&machine).unwrap();
        assert_eq!(report.unreachable, vec!["Orphan"]);
        assert_eq!(
            report
                .shortest_paths
                .iter()
                .find(|p| p.state == "Paid")
                .unwrap()
                .events,
            vec!["reserve", "pay"]
        );
        assert!(report.mermaid.contains("Pending --> Reserved : reserve"));
        assert!(!report.mermaid.contains("Orphan --> Paid"));
    }

    #[test]
    fn legacy_global_events_are_available_from_every_state() {
        let machine =
            parse_machine("state_machine M { state A\n state B\n event reset: A\n event go: B }");
        let report = analyze_machine(&machine).unwrap();
        assert!(report.unreachable.is_empty());
        assert!(report.mermaid.contains("B --> A : reset"));
    }

    #[test]
    fn guarded_reachability_is_an_overapproximation() {
        let machine = parse_machine(
            "state_machine M { state A\n state B\n event go(x: Int) from A: B if x > 0 }",
        );
        let report = analyze_machine(&machine).unwrap();
        assert!(report.unreachable.is_empty());
        assert!(report.mermaid.contains("A --> B : go [guard]"));
    }
}
