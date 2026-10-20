//! Local agent runtime: Director + Manager + Worker + SQLite + NLAP events.

use crate::config::AgentConfigFile;
use crate::store::{SqliteStore, StoreError};
use chrono::Utc;
use nulang_ai_core::{
    ConversationMessage, ConversationState, GoalStatus, SwarmEvent, SwarmEventEnvelope, TaskStatus,
};
use nulang_ai_director::{Director, LocalDirector};
use nulang_ai_manager::{EngineeringManager, Manager};
use nulang_ai_protocol::format_event_line;
use nulang_ai_worker::{
    validate_evidence, EvidenceVerifier, LocalWorker, RejectUnverified, Worker, WorkerExecution,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("missing agent.toml in {0}")]
    MissingConfig(PathBuf),
}

pub struct LocalRuntime {
    project_dir: PathBuf,
    config: AgentConfigFile,
    store: SqliteStore,
    conversation_id: Uuid,
    project_id: String,
    director: LocalDirector,
    engineering: EngineeringManager,
    worker: Box<dyn Worker>,
    verifier: Box<dyn EvidenceVerifier>,
}

impl LocalRuntime {
    pub fn open(project_dir: PathBuf) -> Result<Self, RuntimeError> {
        Self::open_with_execution(
            project_dir,
            Box::new(LocalWorker::new("worker-local")),
            Box::new(RejectUnverified),
        )
    }

    /// Configure both an executor and an independent trusted evidence verifier.
    /// The default open() is deliberately non-executing and fail-closed.
    pub fn open_with_execution(
        project_dir: PathBuf,
        worker: Box<dyn Worker>,
        verifier: Box<dyn EvidenceVerifier>,
    ) -> Result<Self, RuntimeError> {
        if !project_dir.join("agent.toml").exists() {
            return Err(RuntimeError::MissingConfig(project_dir));
        }
        let config = AgentConfigFile::load(&project_dir)?;
        let data_dir = config.resolve_data_dir(&project_dir);
        let store = SqliteStore::open(&data_dir)?;
        let project_id = project_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("default")
            .to_string();
        let conversation_id = Uuid::new_v4();
        let now = Utc::now();
        let conv = ConversationState {
            id: conversation_id,
            project_id: project_id.clone(),
            director_id: Some("director-local".into()),
            active_goal_id: None,
            messages: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        store.upsert_conversation(&conv)?;
        Ok(Self {
            project_dir,
            config,
            store,
            conversation_id,
            project_id,
            director: LocalDirector::new("director-local"),
            engineering: EngineeringManager,
            worker,
            verifier,
        })
    }

    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    pub fn conversation_id(&self) -> Uuid {
        self.conversation_id
    }

    pub fn handle_user_message(
        &mut self,
        text: &str,
        out: &mut dyn Write,
    ) -> Result<Uuid, RuntimeError> {
        let now = Utc::now();
        let mut conv = self.store.get_conversation(self.conversation_id)?;
        conv.messages.push(ConversationMessage {
            role: "user".into(),
            content: text.into(),
            timestamp: now,
        });
        conv.updated_at = now;
        self.store.upsert_conversation(&conv)?;

        let mut goal = self.director.create_goal(
            &self.project_id,
            self.conversation_id,
            text,
            self.config.director.default_budget_usd,
        );
        let goal_id = goal.id;
        self.store.upsert_goal(&goal)?;
        self.emit(
            out,
            SwarmEvent::GoalCreated {
                goal_id,
                conversation_id: Some(self.conversation_id),
            },
        )?;

        goal.status = GoalStatus::Running;
        goal.updated_at = Utc::now();
        self.store.upsert_goal(&goal)?;

        conv.active_goal_id = Some(goal_id);
        conv.updated_at = Utc::now();
        self.store.upsert_conversation(&conv)?;

        let tasks =
            self.engineering
                .plan_tasks(goal_id, text, self.config.director.default_budget_usd);
        let mut all_verified = !tasks.is_empty();
        let mut block_reasons = Vec::new();
        for task in tasks {
            self.store.upsert_task(&task)?;
            self.emit(
                out,
                SwarmEvent::TaskCreated {
                    task_id: task.id,
                    goal_id,
                },
            )?;

            let agent_id = self.worker.agent_id().to_string();
            let mut running = task;
            running.assigned_agent_id = Some(agent_id.clone());
            running.status = TaskStatus::Running;
            running.updated_at = Utc::now();
            self.store.upsert_task(&running)?;
            self.emit(
                out,
                SwarmEvent::TaskStarted {
                    task_id: running.id,
                    agent_id: agent_id.clone(),
                },
            )?;

            match self.worker.execute(&running) {
                WorkerExecution::Executed { evidence } => {
                    // Worker assertions and artifact references are not proof.
                    // Both structural checks and the independent host verifier
                    // must accept them before any Completed state is persisted.
                    let verification = validate_evidence(&running, &evidence)
                        .and_then(|_| self.verifier.verify(&running, &evidence));
                    match verification {
                        Ok(()) => {
                            let mut completed = running;
                            completed.status = TaskStatus::Completed;
                            completed.updated_at = Utc::now();
                            self.store.complete_task_verified(&completed, &evidence)?;
                            self.emit(
                                out,
                                SwarmEvent::TaskVerified {
                                    task_id: completed.id,
                                    agent_id: agent_id.clone(),
                                    evidence,
                                },
                            )?;
                            self.emit(
                                out,
                                SwarmEvent::TaskCompleted {
                                    task_id: completed.id,
                                    agent_id,
                                },
                            )?;
                        }
                        Err(reason) => {
                            all_verified = false;
                            block_reasons.push(reason.clone());
                            let mut blocked = running;
                            blocked.status = TaskStatus::Blocked;
                            blocked.updated_at = Utc::now();
                            self.store.upsert_task(&blocked)?;
                            self.emit(
                                out,
                                SwarmEvent::TaskBlocked {
                                    task_id: blocked.id,
                                    agent_id,
                                    reason,
                                },
                            )?;
                        }
                    }
                }
                WorkerExecution::Blocked { reason } => {
                    all_verified = false;
                    block_reasons.push(reason.clone());
                    let mut blocked = running;
                    blocked.status = TaskStatus::Blocked;
                    blocked.updated_at = Utc::now();
                    self.store.upsert_task(&blocked)?;
                    self.emit(
                        out,
                        SwarmEvent::TaskBlocked {
                            task_id: blocked.id,
                            agent_id,
                            reason,
                        },
                    )?;
                }
                WorkerExecution::Failed { reason } => {
                    all_verified = false;
                    block_reasons.push(reason.clone());
                    let mut failed = running;
                    failed.status = TaskStatus::Failed;
                    failed.updated_at = Utc::now();
                    self.store.upsert_task(&failed)?;
                    self.emit(
                        out,
                        SwarmEvent::TaskFailed {
                            task_id: failed.id,
                            agent_id,
                            reason,
                        },
                    )?;
                }
            }
        }

        goal.status = if all_verified {
            GoalStatus::Completed
        } else {
            GoalStatus::Blocked
        };
        goal.updated_at = Utc::now();
        self.store.upsert_goal(&goal)?;
        if all_verified {
            self.emit(out, SwarmEvent::GoalCompleted { goal_id })?;
        } else {
            self.emit(
                out,
                SwarmEvent::GoalBlocked {
                    goal_id,
                    reason: if block_reasons.is_empty() {
                        "no tasks were planned".into()
                    } else {
                        block_reasons.join("; ")
                    },
                },
            )?;
        }

        Ok(goal_id)
    }

    fn emit(&self, out: &mut dyn Write, event: SwarmEvent) -> Result<(), RuntimeError> {
        let envelope = SwarmEventEnvelope::new(event, Some(self.conversation_id));
        let line = format_event_line(&envelope)?;
        writeln!(out, "{}", line)?;
        out.flush()?;
        Ok(())
    }
}

pub fn init_project(dir: &Path) -> Result<(), RuntimeError> {
    AgentConfigFile::write_init(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn unconfigured_worker_cannot_complete_goal_without_executing_work() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-unverified-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();
        let mut runtime = LocalRuntime::open(tmp.clone()).unwrap();
        let mut stream = Cursor::new(Vec::new());

        let goal_id = runtime.handle_user_message("ship feature X", &mut stream).unwrap();
        let graph = runtime.store().get_goal_graph(goal_id).unwrap();
        let events = String::from_utf8(stream.into_inner()).unwrap();

        assert_eq!(graph.goal.status, GoalStatus::Blocked);
        assert!(graph.tasks.iter().all(|task| task.status != TaskStatus::Completed));
        assert!(events.contains("task_blocked"));
        assert!(events.contains("goal_blocked"));
        assert!(!events.contains("task_completed"));
        assert!(!events.contains("goal_completed"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[derive(Clone)]
    struct FakeWorker(WorkerExecution);

    impl Worker for FakeWorker {
        fn agent_id(&self) -> &str {
            "test-worker"
        }

        fn execute(&self, _task: &nulang_ai_core::Task) -> WorkerExecution {
            self.0.clone()
        }
    }

    struct ArtifactVerifier;

    impl EvidenceVerifier for ArtifactVerifier {
        fn verify(
            &self,
            _task: &nulang_ai_core::Task,
            evidence: &[nulang_ai_core::AcceptanceEvidence],
        ) -> Result<(), String> {
            if evidence.iter().all(|item| item.artifact_ref == "test://verified") {
                Ok(())
            } else {
                Err("artifact did not pass independent validation".into())
            }
        }
    }

    fn fake_evidence(ref_value: &str) -> Vec<nulang_ai_core::AcceptanceEvidence> {
        vec![nulang_ai_core::AcceptanceEvidence {
            criterion: "Tests pass".into(),
            artifact_ref: ref_value.into(),
        }]
    }

    #[test]
    fn executed_claim_without_host_verifier_stays_blocked() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-no-verifier-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();
        let worker = FakeWorker(WorkerExecution::Executed {
            evidence: fake_evidence("test://verified"),
        });
        let mut rt = LocalRuntime::open_with_execution(
            tmp.clone(),
            Box::new(worker),
            Box::new(RejectUnverified),
        ).unwrap();
        let mut out = Cursor::new(Vec::new());

        let id = rt.handle_user_message("run tests", &mut out).unwrap();
        let graph = rt.store().get_goal_graph(id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Blocked);
        assert_eq!(graph.tasks[0].status, TaskStatus::Blocked);
        assert!(rt.store().task_evidence(graph.tasks[0].id).unwrap().is_empty());
        assert!(!String::from_utf8(out.into_inner()).unwrap().contains("task_completed"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn independently_verified_execution_completes_and_persists_evidence() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-verified-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();
        let worker = FakeWorker(WorkerExecution::Executed {
            evidence: fake_evidence("test://verified"),
        });
        let mut rt = LocalRuntime::open_with_execution(
            tmp.clone(),
            Box::new(worker),
            Box::new(ArtifactVerifier),
        ).unwrap();
        let mut out = Cursor::new(Vec::new());

        let id = rt.handle_user_message("run tests", &mut out).unwrap();
        let graph = rt.store().get_goal_graph(id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Completed);
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);
        assert_eq!(
            rt.store().task_evidence(graph.tasks[0].id).unwrap(),
            fake_evidence("test://verified"),
        );
        let events = String::from_utf8(out.into_inner()).unwrap();
        assert!(events.contains("task_verified"));
        assert!(events.contains("task_completed"));
        assert!(events.contains("goal_completed"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn missing_evidence_cannot_bypass_a_permissive_verifier() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-no-evidence-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();
        let mut rt = LocalRuntime::open_with_execution(
            tmp.clone(),
            Box::new(FakeWorker(WorkerExecution::Executed { evidence: vec![] })),
            Box::new(ArtifactVerifier),
        ).unwrap();
        let mut out = Cursor::new(Vec::new());

        let id = rt.handle_user_message("run tests", &mut out).unwrap();
        let graph = rt.store().get_goal_graph(id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Blocked);
        assert_eq!(graph.tasks[0].status, TaskStatus::Blocked);
        assert!(!String::from_utf8(out.into_inner()).unwrap().contains("task_completed"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn failed_worker_does_not_complete_goal() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-failed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();
        let mut rt = LocalRuntime::open_with_execution(
            tmp.clone(),
            Box::new(FakeWorker(WorkerExecution::Failed { reason: "test runner failed".into() })),
            Box::new(ArtifactVerifier),
        ).unwrap();
        let mut out = Cursor::new(Vec::new());

        let id = rt.handle_user_message("run tests", &mut out).unwrap();
        let graph = rt.store().get_goal_graph(id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Blocked);
        assert_eq!(graph.tasks[0].status, TaskStatus::Failed);
        let events = String::from_utf8(out.into_inner()).unwrap();
        assert!(events.contains("task_failed"));
        assert!(events.contains("goal_blocked"));
        assert!(!events.contains("task_completed"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn runtime_emits_nlap_events() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();
        let mut rt = LocalRuntime::open(tmp.clone()).unwrap();
        let mut buf = Cursor::new(Vec::new());
        let goal_id = rt.handle_user_message("ship feature X", &mut buf).unwrap();
        assert!(!goal_id.is_nil());
        let text = String::from_utf8(buf.into_inner()).unwrap();
        assert!(text.contains("goal_created"));
        assert!(text.contains("task_created"));
        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Blocked);
        let _ = std::fs::remove_dir_all(tmp);
    }
}
