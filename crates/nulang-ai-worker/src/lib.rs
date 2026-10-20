pub mod sandbox;

//! Worker agents execute tasks and return evidence for independent verification.

use nulang_ai_core::{AcceptanceEvidence, Task};

/// An executor reports what happened. It cannot authorize its own completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerExecution {
    Executed { evidence: Vec<AcceptanceEvidence> },
    Blocked { reason: String },
    Failed { reason: String },
}

pub trait Worker: Send + Sync {
    fn agent_id(&self) -> &str;
    fn execute(&self, task: &Task) -> WorkerExecution;
}

/// The host, rather than the model or the worker, is responsible for validating
/// evidence against the real artifact (test run, commit, transaction, etc.).
pub trait EvidenceVerifier: Send + Sync {
    fn verify(&self, task: &Task, evidence: &[AcceptanceEvidence]) -> Result<(), String>;
}

/// Fail-closed default: the local scaffold has no trusted artifact verifier.
pub struct RejectUnverified;

impl EvidenceVerifier for RejectUnverified {
    fn verify(&self, _task: &Task, _evidence: &[AcceptanceEvidence]) -> Result<(), String> {
        Err("no trusted acceptance-evidence verifier is configured".into())
    }
}

/// Structural checks are necessary but insufficient for verification.
/// Call the trusted EvidenceVerifier even after these checks succeed.
pub fn validate_evidence(
    task: &Task,
    evidence: &[AcceptanceEvidence],
) -> Result<(), String> {
    if task.acceptance_criteria.is_empty() {
        return Err("task has no acceptance criteria".into());
    }
    for criterion in &task.acceptance_criteria {
        if criterion.trim().is_empty() {
            return Err("task has an empty acceptance criterion".into());
        }
        if !evidence.iter().any(|item| {
            item.criterion == *criterion && !item.artifact_ref.trim().is_empty()
        }) {
            return Err(format!("missing artifact evidence for criterion: {criterion}"));
        }
    }
    Ok(())
}

/// The default local worker is a scaffold, not a real tool executor.
/// Returning Blocked is intentional until a trusted execution backend is wired.
pub struct LocalWorker {
    id: String,
}

impl LocalWorker {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

impl Worker for LocalWorker {
    fn agent_id(&self) -> &str {
        &self.id
    }

    fn execute(&self, _task: &Task) -> WorkerExecution {
        WorkerExecution::Blocked {
            reason: "no task execution backend is configured".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_core::{ManagerKind, Task};
    use uuid::Uuid;

    fn task() -> Task {
        let mut task = Task::new(Uuid::new_v4(), "run tests", ManagerKind::Engineering);
        task.acceptance_criteria = vec!["Tests pass".into()];
        task
    }

    #[test]
    fn unconfigured_worker_does_not_claim_completion() {
        assert!(matches!(
            LocalWorker::new("local").execute(&task()),
            WorkerExecution::Blocked { .. }
        ));
    }

    #[test]
    fn missing_or_blank_artifact_evidence_is_rejected() {
        let task = task();
        assert!(validate_evidence(&task, &[]).is_err());
        assert!(validate_evidence(
            &task,
            &[AcceptanceEvidence {
                criterion: "Tests pass".into(),
                artifact_ref: " ".into(),
            }],
        )
        .is_err());
        assert!(validate_evidence(
            &task,
            &[AcceptanceEvidence {
                criterion: "Wrong criterion".into(),
                artifact_ref: "test://run-1".into(),
            }],
        )
        .is_err());
    }

    #[test]
    fn valid_references_pass_structural_checks_only() {
        let task = task();
        let evidence = [AcceptanceEvidence {
            criterion: "Tests pass".into(),
            artifact_ref: "test://run-1".into(),
        }];
        assert!(validate_evidence(&task, &evidence).is_ok());
        assert!(RejectUnverified.verify(&task, &evidence).is_err());
    }

    #[test]
    fn tasks_without_acceptance_criteria_cannot_complete() {
        let mut task = task();
        task.acceptance_criteria.clear();
        assert!(validate_evidence(&task, &[]).is_err());
    }
}
