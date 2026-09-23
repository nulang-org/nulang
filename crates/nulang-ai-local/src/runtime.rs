//! Local agent runtime: Director + Manager + Worker + SQLite + NLAP events.

use crate::config::AgentConfigFile;
use crate::store::{SqliteStore, StoreError};
use chrono::Utc;
use nulang_ai_core::{
    Commitment, CommitmentStatus, ConversationMessage, ConversationState, GoalStatus, Intention,
    IntentionRevision, IntentionRevisionDecision, IntentionStatus, SwarmEvent, SwarmEventEnvelope,
    Task, TaskStatus,
};
use nulang_ai_director::{Director, LocalDirector};
use nulang_ai_manager::{EngineeringManager, Manager};
use nulang_ai_protocol::format_event_line;
use nulang_ai_worker::{LocalWorker, TaskExecution, Worker};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_AUTOMATIC_REPLANS: usize = 1;

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

enum PlanOutcome {
    Completed,
    Blocked { task_id: Uuid, reason: String },
    Failed { task_id: Uuid, reason: String },
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
}

impl LocalRuntime {
    pub fn open(project_dir: PathBuf) -> Result<Self, RuntimeError> {
        Self::open_with_worker(project_dir, Box::new(LocalWorker::new("worker-local")))
    }

    pub fn open_with_worker(
        project_dir: PathBuf,
        worker: Box<dyn Worker>,
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

        let mut commitment = Commitment::new(
            goal_id,
            "director-local",
            "User goal accepted for execution",
        );
        commitment.success_criteria = goal.success_criteria.clone();
        commitment.status = CommitmentStatus::Active;
        commitment.updated_at = Utc::now();
        self.store.upsert_commitment(&commitment)?;
        self.emit(
            out,
            SwarmEvent::CommitmentActivated {
                commitment_id: commitment.id,
                goal_id,
                owner_agent_id: commitment.owner_agent_id.clone(),
            },
        )?;

        conv.active_goal_id = Some(goal_id);
        conv.updated_at = Utc::now();
        self.store.upsert_conversation(&conv)?;

        let mut replan_count = 0usize;
        let mut tasks =
            self.engineering
                .plan_tasks(goal_id, text, self.config.director.default_budget_usd);
        let mut intention = self.new_intention(goal_id, commitment.id, 0, &tasks);
        self.activate_intention(&intention, out)?;

        loop {
            match self.execute_intention(&mut intention, tasks, out)? {
                PlanOutcome::Completed => {
                    commitment.status = CommitmentStatus::Fulfilled;
                    commitment.updated_at = Utc::now();
                    self.store.upsert_commitment(&commitment)?;
                    self.emit(
                        out,
                        SwarmEvent::CommitmentFulfilled {
                            commitment_id: commitment.id,
                            goal_id,
                        },
                    )?;

                    goal.status = GoalStatus::Completed;
                    goal.updated_at = Utc::now();
                    self.store.upsert_goal(&goal)?;
                    self.emit(out, SwarmEvent::GoalCompleted { goal_id })?;
                    return Ok(goal_id);
                }
                PlanOutcome::Blocked { task_id, reason } => {
                    let revision = IntentionRevision::new(
                        &intention,
                        None,
                        task_id,
                        TaskStatus::Blocked,
                        IntentionRevisionDecision::Suspend,
                        reason.clone(),
                    );
                    self.record_revision(&revision, out)?;

                    commitment.status = CommitmentStatus::Suspended;
                    commitment.updated_at = Utc::now();
                    self.store.upsert_commitment(&commitment)?;
                    self.emit(
                        out,
                        SwarmEvent::CommitmentSuspended {
                            commitment_id: commitment.id,
                            goal_id,
                            reason: reason.clone(),
                        },
                    )?;

                    goal.status = GoalStatus::Blocked;
                    goal.updated_at = Utc::now();
                    self.store.upsert_goal(&goal)?;
                    self.emit(out, SwarmEvent::GoalBlocked { goal_id, reason })?;
                    return Ok(goal_id);
                }
                PlanOutcome::Failed { task_id, reason } if replan_count < MAX_AUTOMATIC_REPLANS => {
                    replan_count += 1;
                    let replacement_tasks = self.engineering.plan_tasks(
                        goal_id,
                        text,
                        self.config.director.default_budget_usd,
                    );
                    let replacement = self.new_intention(
                        goal_id,
                        commitment.id,
                        replan_count,
                        &replacement_tasks,
                    );
                    self.store.upsert_intention(&replacement)?;

                    let revision = IntentionRevision::new(
                        &intention,
                        Some(replacement.id),
                        task_id,
                        TaskStatus::Failed,
                        IntentionRevisionDecision::Replan,
                        reason,
                    );
                    self.record_revision(&revision, out)?;
                    self.emit_intention_activated(&replacement, out)?;

                    intention = replacement;
                    tasks = replacement_tasks;
                }
                PlanOutcome::Failed { task_id, reason } => {
                    let revision = IntentionRevision::new(
                        &intention,
                        None,
                        task_id,
                        TaskStatus::Failed,
                        IntentionRevisionDecision::Abandon,
                        reason.clone(),
                    );
                    self.record_revision(&revision, out)?;

                    commitment.status = CommitmentStatus::Abandoned;
                    commitment.updated_at = Utc::now();
                    self.store.upsert_commitment(&commitment)?;
                    self.emit(
                        out,
                        SwarmEvent::CommitmentAbandoned {
                            commitment_id: commitment.id,
                            goal_id,
                            reason: reason.clone(),
                        },
                    )?;

                    goal.status = GoalStatus::Failed;
                    goal.updated_at = Utc::now();
                    self.store.upsert_goal(&goal)?;
                    self.emit(out, SwarmEvent::GoalFailed { goal_id, reason })?;
                    return Ok(goal_id);
                }
            }
        }
    }

    fn new_intention(
        &self,
        goal_id: Uuid,
        commitment_id: Uuid,
        attempt: usize,
        tasks: &[Task],
    ) -> Intention {
        let description = if attempt == 0 {
            "Execute the selected engineering plan".to_string()
        } else {
            format!("Execute engineering replan attempt {attempt}")
        };
        let mut intention = Intention::new(
            goal_id,
            commitment_id,
            "manager-engineering",
            description,
            tasks.iter().map(|task| task.id).collect(),
        );
        intention.status = IntentionStatus::Active;
        intention.updated_at = Utc::now();
        intention
    }

    fn activate_intention(
        &self,
        intention: &Intention,
        out: &mut dyn Write,
    ) -> Result<(), RuntimeError> {
        self.store.upsert_intention(intention)?;
        self.emit_intention_activated(intention, out)
    }

    fn emit_intention_activated(
        &self,
        intention: &Intention,
        out: &mut dyn Write,
    ) -> Result<(), RuntimeError> {
        self.emit(
            out,
            SwarmEvent::IntentionActivated {
                intention_id: intention.id,
                commitment_id: intention.commitment_id,
                goal_id: intention.goal_id,
                owner_agent_id: intention.owner_agent_id.clone(),
            },
        )
    }

    fn execute_intention(
        &self,
        intention: &mut Intention,
        tasks: Vec<Task>,
        out: &mut dyn Write,
    ) -> Result<PlanOutcome, RuntimeError> {
        for task in &tasks {
            self.store.upsert_task(task)?;
            self.emit(
                out,
                SwarmEvent::TaskCreated {
                    task_id: task.id,
                    goal_id: task.goal_id,
                },
            )?;
        }

        for task in tasks {
            let agent_id = task
                .assigned_agent_id
                .clone()
                .unwrap_or_else(|| self.worker.agent_id().to_string());
            let mut running = task;
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

            let report = self.worker.execute_with_report(&running);
            let reported_status = report.task.status;
            let worker_reason = report.reason;
            let (terminal_status, reason) = match reported_status {
                TaskStatus::Completed => (TaskStatus::Completed, None),
                TaskStatus::Blocked => (
                    TaskStatus::Blocked,
                    Some(
                        worker_reason
                            .unwrap_or_else(|| format!("task {} blocked by worker", running.id)),
                    ),
                ),
                TaskStatus::Failed => (
                    TaskStatus::Failed,
                    Some(worker_reason.unwrap_or_else(|| {
                        format!("task {} failed in worker execution", running.id)
                    })),
                ),
                other => (
                    TaskStatus::Failed,
                    Some(worker_reason.unwrap_or_else(|| {
                        format!(
                            "task {} returned non-terminal worker status {:?}",
                            running.id, other
                        )
                    })),
                ),
            };
            // Worker output is an outcome carrier, not authority to rewrite the
            // scheduled task's identity, description, assignment, or plan links.
            let mut result = running;
            result.status = terminal_status;
            result.updated_at = Utc::now();
            self.store.upsert_task(&result)?;

            match terminal_status {
                TaskStatus::Completed => {
                    self.emit(
                        out,
                        SwarmEvent::TaskCompleted {
                            task_id: result.id,
                            agent_id,
                        },
                    )?;
                }
                TaskStatus::Blocked => {
                    let reason = reason.expect("blocked outcome has reason");
                    self.emit(
                        out,
                        SwarmEvent::TaskBlocked {
                            task_id: result.id,
                            agent_id,
                            reason: reason.clone(),
                        },
                    )?;
                    intention.status = IntentionStatus::Blocked;
                    intention.updated_at = Utc::now();
                    self.store.upsert_intention(intention)?;
                    self.emit(
                        out,
                        SwarmEvent::IntentionBlocked {
                            intention_id: intention.id,
                            commitment_id: intention.commitment_id,
                            goal_id: intention.goal_id,
                            reason: reason.clone(),
                        },
                    )?;
                    return Ok(PlanOutcome::Blocked {
                        task_id: result.id,
                        reason,
                    });
                }
                TaskStatus::Failed => {
                    let reason = reason.expect("failed outcome has reason");
                    self.emit(
                        out,
                        SwarmEvent::TaskFailed {
                            task_id: result.id,
                            agent_id,
                            reason: reason.clone(),
                        },
                    )?;
                    intention.status = IntentionStatus::Failed;
                    intention.updated_at = Utc::now();
                    self.store.upsert_intention(intention)?;
                    self.emit(
                        out,
                        SwarmEvent::IntentionFailed {
                            intention_id: intention.id,
                            commitment_id: intention.commitment_id,
                            goal_id: intention.goal_id,
                            reason: reason.clone(),
                        },
                    )?;
                    return Ok(PlanOutcome::Failed {
                        task_id: result.id,
                        reason,
                    });
                }
                _ => unreachable!("worker result is normalized to a terminal status"),
            }
        }

        intention.status = IntentionStatus::Completed;
        intention.updated_at = Utc::now();
        self.store.upsert_intention(intention)?;
        self.emit(
            out,
            SwarmEvent::IntentionCompleted {
                intention_id: intention.id,
                commitment_id: intention.commitment_id,
                goal_id: intention.goal_id,
            },
        )?;
        Ok(PlanOutcome::Completed)
    }

    fn record_revision(
        &self,
        revision: &IntentionRevision,
        out: &mut dyn Write,
    ) -> Result<(), RuntimeError> {
        self.store.insert_intention_revision(revision)?;
        self.emit(
            out,
            SwarmEvent::IntentionRevised {
                revision_id: revision.id,
                superseded_intention_id: revision.superseded_intention_id,
                replacement_intention_id: revision.replacement_intention_id,
                decision: revision.decision,
                reason: revision.reason.clone(),
            },
        )
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
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct ScriptedWorker {
        statuses: Mutex<VecDeque<TaskStatus>>,
    }

    impl ScriptedWorker {
        fn new(statuses: impl IntoIterator<Item = TaskStatus>) -> Self {
            Self {
                statuses: Mutex::new(statuses.into_iter().collect()),
            }
        }
    }

    impl Worker for ScriptedWorker {
        fn agent_id(&self) -> &str {
            "worker-scripted"
        }

        fn execute(&self, task: &Task) -> Task {
            let mut task = task.clone();
            task.status = self
                .statuses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(TaskStatus::Completed);
            task.updated_at = Utc::now();
            task
        }

        fn execute_with_report(&self, task: &Task) -> TaskExecution {
            let task = self.execute(task);
            match task.status {
                TaskStatus::Blocked => {
                    TaskExecution::with_reason(task, "scripted external dependency is unavailable")
                }
                TaskStatus::Failed => {
                    TaskExecution::with_reason(task, "scripted worker execution failed")
                }
                _ => TaskExecution::new(task),
            }
        }
    }

    fn runtime_with_worker(tmp: &Path, statuses: Vec<TaskStatus>) -> LocalRuntime {
        init_project(tmp).unwrap();
        LocalRuntime::open_with_worker(tmp.to_path_buf(), Box::new(ScriptedWorker::new(statuses)))
            .unwrap()
    }

    #[test]
    fn runtime_emits_nlap_events() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();
        let mut rt = LocalRuntime::open(tmp.clone()).unwrap();
        let mut buf = std::io::Cursor::new(Vec::new());
        let goal_id = rt.handle_user_message("ship feature X", &mut buf).unwrap();
        assert!(!goal_id.is_nil());
        let text = String::from_utf8(buf.into_inner()).unwrap();
        assert!(text.contains("goal_created"));
        assert!(text.contains("commitment_activated"));
        assert!(text.contains("intention_activated"));
        assert!(text.contains("task_created"));
        assert!(text.contains("intention_completed"));
        assert!(text.contains("commitment_fulfilled"));
        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Completed);
        assert_eq!(graph.commitments.len(), 1);
        assert_eq!(graph.commitments[0].status, CommitmentStatus::Fulfilled);
        assert_eq!(graph.intentions.len(), 1);
        assert_eq!(graph.intentions[0].status, IntentionStatus::Completed);
        assert!(graph.intention_revisions.is_empty());
        assert_eq!(
            graph.intentions[0].planned_task_ids.len(),
            graph.tasks.len()
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn failed_task_replans_once_and_can_recover() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-replan-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut rt = runtime_with_worker(&tmp, vec![TaskStatus::Failed, TaskStatus::Completed]);
        let mut buf = std::io::Cursor::new(Vec::new());
        let goal_id = rt.handle_user_message("ship feature X", &mut buf).unwrap();
        let text = String::from_utf8(buf.into_inner()).unwrap();
        assert!(text.contains("task_failed"));
        assert!(text.contains("intention_revised"));
        assert!(text.contains("commitment_fulfilled"));

        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Completed);
        assert_eq!(graph.intentions.len(), 2);
        assert_eq!(graph.intentions[0].status, IntentionStatus::Failed);
        assert_eq!(graph.intentions[1].status, IntentionStatus::Completed);
        assert_eq!(graph.intention_revisions.len(), 1);
        assert_eq!(
            graph.intention_revisions[0].decision,
            IntentionRevisionDecision::Replan
        );
        assert_eq!(
            graph.intention_revisions[0].reason,
            "scripted worker execution failed"
        );
        assert_eq!(
            graph.intention_revisions[0].replacement_intention_id,
            Some(graph.intentions[1].id)
        );
        assert_eq!(graph.commitments[0].status, CommitmentStatus::Fulfilled);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn blocked_task_suspends_commitment_and_goal() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-blocked-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut rt = runtime_with_worker(&tmp, vec![TaskStatus::Blocked]);
        let mut buf = std::io::Cursor::new(Vec::new());
        let goal_id = rt.handle_user_message("ship feature X", &mut buf).unwrap();
        let text = String::from_utf8(buf.into_inner()).unwrap();
        assert!(text.contains("task_blocked"));
        assert!(text.contains("commitment_suspended"));
        assert!(text.contains("goal_blocked"));

        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Blocked);
        assert_eq!(graph.intentions[0].status, IntentionStatus::Blocked);
        assert_eq!(graph.commitments[0].status, CommitmentStatus::Suspended);
        assert_eq!(graph.intention_revisions.len(), 1);
        assert_eq!(
            graph.intention_revisions[0].decision,
            IntentionRevisionDecision::Suspend
        );
        assert_eq!(
            graph.intention_revisions[0].reason,
            "scripted external dependency is unavailable"
        );
        assert!(graph.intention_revisions[0]
            .replacement_intention_id
            .is_none());

        let mut conflicting = graph.intention_revisions[0].clone();
        conflicting.reason = "attempted revision rewrite".into();
        rt.store().insert_intention_revision(&conflicting).unwrap();
        let graph_after_retry = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(
            graph_after_retry.intention_revisions[0].reason,
            "scripted external dependency is unavailable"
        );

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn repeated_failure_abandons_commitment_after_bounded_replan() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-failed-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut rt = runtime_with_worker(&tmp, vec![TaskStatus::Failed, TaskStatus::Failed]);
        let mut buf = std::io::Cursor::new(Vec::new());
        let goal_id = rt.handle_user_message("ship feature X", &mut buf).unwrap();
        let text = String::from_utf8(buf.into_inner()).unwrap();
        assert!(text.contains("commitment_abandoned"));
        assert!(text.contains("goal_failed"));

        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Failed);
        assert_eq!(graph.intentions.len(), 2);
        assert_eq!(graph.intentions[0].status, IntentionStatus::Failed);
        assert_eq!(graph.intentions[1].status, IntentionStatus::Failed);
        assert_eq!(graph.intention_revisions.len(), 2);
        assert_eq!(
            graph.intention_revisions[0].decision,
            IntentionRevisionDecision::Replan
        );
        assert_eq!(
            graph.intention_revisions[1].decision,
            IntentionRevisionDecision::Abandon
        );
        assert_eq!(graph.commitments[0].status, CommitmentStatus::Abandoned);
        let _ = std::fs::remove_dir_all(tmp);
    }
}
