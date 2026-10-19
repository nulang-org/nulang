use nulang_ai::{SupervisorRuntime, SupervisorTeam};

#[derive(Default)]
struct TrackingRuntime {
    calls: Vec<(String, u64, String)>,
}

impl SupervisorRuntime for TrackingRuntime {
    fn ask_agent(&mut self, agent_id: u64, prompt: &str) -> Result<String, String> {
        self.calls
            .push(("ask".to_string(), agent_id, prompt.to_string()));
        Ok(format!("ask:{agent_id}"))
    }

    fn delegate_agent(&mut self, agent_id: u64, prompt: &str) -> Result<String, String> {
        self.calls
            .push(("delegate".to_string(), agent_id, prompt.to_string()));
        Ok(format!("delegate:{agent_id}"))
    }

    fn handoff_agent(&mut self, agent_id: u64, context: &str) -> Result<String, String> {
        self.calls
            .push(("handoff".to_string(), agent_id, context.to_string()));
        Ok(format!("handoff:{agent_id}"))
    }
}

#[test]
fn supervisor_exposes_distinct_delegate_and_handoff_semantics() {
    let team = SupervisorTeam::new()
        .worker("researcher", 10, "find evidence")
        .worker("writer", 20, "own the final response");
    let mut runtime = TrackingRuntime::default();

    let delegated = team
        .delegate(&mut runtime, "researcher", "find the constraints")
        .unwrap();
    let handed_off = team
        .handoff(&mut runtime, "writer", "take ownership from here")
        .unwrap();

    assert_eq!(delegated, "delegate:10");
    assert_eq!(handed_off, "handoff:20");
    assert_eq!(runtime.calls[0].0, "delegate");
    assert_eq!(runtime.calls[1].0, "handoff");
}

#[test]
fn supervisor_reports_unknown_worker_for_explicit_routing() {
    let team = SupervisorTeam::new().worker("researcher", 10, "find evidence");
    let mut runtime = TrackingRuntime::default();

    let err = team.delegate(&mut runtime, "missing", "task").unwrap_err();
    assert!(err.contains("Worker missing not found"));
    assert!(runtime.calls.is_empty());
}
