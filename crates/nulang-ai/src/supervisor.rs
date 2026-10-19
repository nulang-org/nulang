//! Supervisor pattern for multi-agent orchestration.
//!
//! A [`SupervisorTeam`] coordinates a set of worker agents. The legacy
//! [`SupervisorTeam::run`] path delegates through workers in order and returns
//! the final accumulated result. Explicit [`SupervisorTeam::delegate`] and
//! [`SupervisorTeam::handoff`] operations expose two different ownership
//! semantics without forcing a specific distributed runtime implementation.
//!
//! Implementors of [`SupervisorRuntime`] provide the actor behavior. Test code
//! can use a mock implementation to avoid spinning up a real actor system.

// ---------------------------------------------------------------------------
// Runtime abstraction
// ---------------------------------------------------------------------------

/// Minimal runtime capability required to execute a supervisor team.
///
/// `delegate_agent` means the supervisor retains ownership and expects a result
/// back. `handoff_agent` means the target worker becomes the logical owner of
/// subsequent execution. The default implementations preserve backward
/// compatibility by routing both operations through `ask_agent`; runtimes that
/// track ownership or sessions can override them independently.
pub trait SupervisorRuntime {
    /// Send `prompt` to `agent_id` and return the textual response.
    fn ask_agent(&mut self, agent_id: u64, prompt: &str) -> Result<String, String>;

    /// Execute bounded specialist work while the caller retains ownership.
    fn delegate_agent(&mut self, agent_id: u64, prompt: &str) -> Result<String, String> {
        self.ask_agent(agent_id, prompt)
    }

    /// Transfer logical ownership of the active work to another agent.
    fn handoff_agent(&mut self, agent_id: u64, context: &str) -> Result<String, String> {
        self.ask_agent(agent_id, context)
    }
}

// ---------------------------------------------------------------------------
// Supervisor definition
// ---------------------------------------------------------------------------

/// A single worker in a supervisor team.
#[derive(Debug, Clone)]
pub struct Worker {
    /// Logical name for the worker.
    pub name: String,
    /// Target actor id.
    pub agent_id: u64,
    /// Description used when prompting the worker.
    pub description: String,
}

/// A supervisor team that delegates tasks to a sequence of workers.
#[derive(Debug, Clone, Default)]
pub struct SupervisorTeam {
    pub workers: Vec<Worker>,
    pub max_iterations: usize,
}

impl SupervisorTeam {
    /// Create an empty supervisor team.
    pub fn new() -> Self {
        Self {
            workers: Vec::new(),
            max_iterations: 10,
        }
    }

    /// Create a team with a maximum iteration limit.
    pub fn with_max_iterations(max_iterations: usize) -> Self {
        Self {
            workers: Vec::new(),
            max_iterations,
        }
    }

    /// Append a worker and return `self` for fluent construction.
    pub fn worker(
        mut self,
        name: impl Into<String>,
        agent_id: u64,
        description: impl Into<String>,
    ) -> Self {
        self.workers.push(Worker {
            name: name.into(),
            agent_id,
            description: description.into(),
        });
        self
    }

    fn worker_named(&self, name: &str) -> Result<&Worker, String> {
        self.workers
            .iter()
            .find(|worker| worker.name == name)
            .ok_or_else(|| format!("Worker {} not found", name))
    }

    /// Delegate bounded work to a named specialist and return its result to the
    /// current owner.
    pub fn delegate<R: SupervisorRuntime>(
        &self,
        runtime: &mut R,
        worker_name: &str,
        task: &str,
    ) -> Result<String, String> {
        let worker = self.worker_named(worker_name)?;
        let prompt = format!(
            "You are {}. {}\n\nDelegated task: {}\n\nReturn a bounded result to the supervisor; do not assume ownership of unrelated work.",
            worker.name, worker.description, task
        );
        runtime.delegate_agent(worker.agent_id, &prompt)
    }

    /// Hand logical ownership of the active work to a named worker.
    ///
    /// The concrete runtime decides how ownership is represented (session,
    /// actor, thread, or remote agent). The default runtime behavior remains a
    /// synchronous request/response for compatibility.
    pub fn handoff<R: SupervisorRuntime>(
        &self,
        runtime: &mut R,
        worker_name: &str,
        context: &str,
    ) -> Result<String, String> {
        let worker = self.worker_named(worker_name)?;
        let prompt = format!(
            "You are {}. {}\n\nOwnership has been handed to you. Continue from this context:\n{}",
            worker.name, worker.description, context
        );
        runtime.handoff_agent(worker.agent_id, &prompt)
    }

    /// Run the team on `task`, returning the final worker's output.
    ///
    /// Each worker receives a prompt that includes its description and the
    /// current accumulated state. Returns an error if the team has no workers
    /// or if any worker call fails.
    pub fn run<R: SupervisorRuntime>(&self, runtime: &mut R, task: &str) -> Result<String, String> {
        if self.workers.is_empty() {
            return Err("Supervisor team has no workers".to_string());
        }

        let mut current = task.to_string();
        for worker in &self.workers {
            let prompt = format!(
                "You are {}. {}\n\nTask: {}\n\nCurrent context: {}",
                worker.name, worker.description, task, current
            );
            current = runtime.ask_agent(worker.agent_id, &prompt)?;
        }
        Ok(current)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct MockRuntime {
        responses: HashMap<u64, String>,
        calls: RefCell<Vec<(u64, String)>>,
    }

    impl MockRuntime {
        fn new(responses: HashMap<u64, String>) -> Self {
            Self {
                responses,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl SupervisorRuntime for MockRuntime {
        fn ask_agent(&mut self, agent_id: u64, prompt: &str) -> Result<String, String> {
            self.calls.borrow_mut().push((agent_id, prompt.to_string()));
            self.responses
                .get(&agent_id)
                .cloned()
                .ok_or_else(|| format!("No response configured for agent {}", agent_id))
        }
    }

    #[test]
    fn test_empty_team_errors() {
        let team = SupervisorTeam::new();
        let mut rt = MockRuntime::new(HashMap::new());
        assert_eq!(
            team.run(&mut rt, "task"),
            Err("Supervisor team has no workers".to_string())
        );
    }

    #[test]
    fn test_single_worker() {
        let team = SupervisorTeam::new().worker("writer", 1, "Writes content");
        let mut rt = MockRuntime::new(HashMap::from([(1, "article".to_string())]));

        let result = team.run(&mut rt, "Write about CRDTs").unwrap();
        assert_eq!(result, "article");

        let calls = rt.calls.into_inner();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.contains("writer"));
        assert!(calls[0].1.contains("Write about CRDTs"));
    }

    #[test]
    fn test_multiple_workers_chain() {
        let team = SupervisorTeam::new()
            .worker("researcher", 1, "Finds information")
            .worker("writer", 2, "Writes content");
        let mut rt = MockRuntime::new(HashMap::from([
            (1, "research notes".to_string()),
            (2, "final article".to_string()),
        ]));

        let result = team.run(&mut rt, "CRDTs").unwrap();
        assert_eq!(result, "final article");

        let calls = rt.calls.into_inner();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].1.contains("research notes"));
    }

    #[test]
    fn test_worker_error_propagates() {
        let team = SupervisorTeam::new()
            .worker("ok", 1, "ok")
            .worker("fail", 2, "fail");
        let mut rt = MockRuntime::new(HashMap::from([(1, "intermediate".to_string())]));

        assert_eq!(
            team.run(&mut rt, "start"),
            Err("No response configured for agent 2".to_string())
        );
    }

    #[test]
    fn test_delegate_and_handoff_default_to_ask_agent() {
        let team = SupervisorTeam::new().worker("worker", 1, "Does work");
        let mut rt = MockRuntime::new(HashMap::from([(1, "result".to_string())]));

        assert_eq!(team.delegate(&mut rt, "worker", "task").unwrap(), "result");
        assert_eq!(
            team.handoff(&mut rt, "worker", "context").unwrap(),
            "result"
        );
        assert_eq!(rt.calls.into_inner().len(), 2);
    }
}
