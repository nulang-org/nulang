//! Local agent runtime: Director + Manager + Worker + SQLite + NLAP events.

use crate::config::AgentConfigFile;
use crate::store::{SqliteStore, StoreError};
use crate::task_workflow::TaskWorkflowRuntime;
use chrono::Utc;
use nulang_ai_core::{
    ConversationMessage, ConversationState, GoalStatus, SwarmEvent, SwarmEventEnvelope, Task,
    TaskStatus,
};
use nulang_ai_director::{Director, LocalDirector};
use nulang_ai_manager::{EngineeringManager, Manager};
use nulang_ai_protocol::format_event_line;
use nulang_ai_worker::{LocalWorker, Worker};
use nulang_workflow::{
    ActivityProgress, ActivitySpec, DurableWorkflowExecutor, LeaseAcquireOutcome,
    LeaseReleaseOutcome, RetryPolicy, WorkerLease, WorkerLeaseStore, WorkflowId,
};
use std::collections::HashSet;
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
    #[error("workflow error: {0}")]
    Workflow(String),
    #[error("task {task_id} failed: {error}")]
    TaskFailed { task_id: Uuid, error: String },
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
    worker: LocalWorker,
    worker_session_id: String,
}

enum TaskExecutionOutcome {
    Completed,
    Deferred,
}

impl LocalRuntime {
    pub fn open(project_dir: PathBuf) -> Result<Self, RuntimeError> {
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
            worker: LocalWorker::new("worker-local"),
            worker_session_id: format!("worker-session:{}", Uuid::new_v4()),
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

    /// Resume tasks that were persisted before a previous process stopped.
    ///
    /// Each task reuses its original workflow/activity identity, so a worker
    /// that propagates the supplied idempotency key can make external effects
    /// replay-safe across crashes.
    pub fn resume_pending_tasks(
        &mut self,
        out: &mut dyn Write,
    ) -> Result<usize, RuntimeError> {
        let tasks = self.store.list_resumable_tasks()?;
        let mut completed = 0usize;
        let mut touched_goals = HashSet::new();

        for task in tasks {
            touched_goals.insert(task.goal_id);
            if matches!(
                self.execute_task_durably(task, out)?,
                TaskExecutionOutcome::Completed
            ) {
                completed += 1;
            }
        }

        for goal_id in touched_goals {
            self.complete_goal_if_ready(goal_id, out)?;
        }

        Ok(completed)
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
        for task in tasks {
            self.store.upsert_task(&task)?;
            self.emit(
                out,
                SwarmEvent::TaskCreated {
                    task_id: task.id,
                    goal_id,
                },
            )?;
            let _ = self.execute_task_durably(task, out)?;
        }

        self.complete_goal_if_ready(goal_id, out)?;
        Ok(goal_id)
    }

    fn execute_task_durably(
        &mut self,
        task: Task,
        out: &mut dyn Write,
    ) -> Result<TaskExecutionOutcome, RuntimeError> {
        let resource_id = format!("agent-task:{}", task.id);
        let lease_duration_millis = task_lease_duration_millis(&task);
        let now_millis = unix_millis();
        let lease = match self.store.try_acquire(
            &resource_id,
            &self.worker_session_id,
            now_millis,
            lease_duration_millis,
        )? {
            LeaseAcquireOutcome::Acquired(lease)
            | LeaseAcquireOutcome::AlreadyHeldByCaller(lease) => lease,
            LeaseAcquireOutcome::HeldByOther { .. } => {
                return Ok(TaskExecutionOutcome::Deferred);
            }
        };

        let agent_id = task
            .assigned_agent_id
            .clone()
            .unwrap_or_else(|| self.worker.agent_id().to_string());

        let mut running = task;
        if running.status != TaskStatus::Running {
            running.status = TaskStatus::Running;
            running.updated_at = Utc::now();
            if !self
                .store
                .upsert_task_if_lease_current(&running, &lease, unix_millis())?
            {
                return Ok(TaskExecutionOutcome::Deferred);
            }
            self.emit(
                out,
                SwarmEvent::TaskStarted {
                    task_id: running.id,
                    agent_id: agent_id.clone(),
                },
            )?;
        }

        let workflow_id = WorkflowId::new(resource_id);
        let spec = ActivitySpec::new(
            "execute",
            "agent.worker.execute",
            serde_json::to_vec(&running)?,
        )
        .retry(RetryPolicy::exponential(3, 500, 5_000, 2));
        let mut workflow_runtime = TaskWorkflowRuntime::new(&self.store, &self.worker);
        let progress = DurableWorkflowExecutor
            .execute_activity(&mut workflow_runtime, &workflow_id, &spec)
            .map_err(|error| RuntimeError::Workflow(error.to_string()))?;

        match progress {
            ActivityProgress::Completed(payload) => {
                let completed: Task = serde_json::from_slice(&payload)?;
                if completed.id != running.id || completed.goal_id != running.goal_id {
                    return Err(RuntimeError::Workflow(format!(
                        "durable task result identity mismatch for {}",
                        running.id
                    )));
                }
                if !self
                    .store
                    .upsert_task_if_lease_current(&completed, &lease, unix_millis())?
                {
                    return Ok(TaskExecutionOutcome::Deferred);
                }
                release_task_lease(&mut self.store, &lease)?;
                self.emit(
                    out,
                    SwarmEvent::TaskCompleted {
                        task_id: completed.id,
                        agent_id,
                    },
                )?;
                Ok(TaskExecutionOutcome::Completed)
            }
            ActivityProgress::WaitingForRetry { .. } => {
                release_task_lease(&mut self.store, &lease)?;
                Ok(TaskExecutionOutcome::Deferred)
            }
            ActivityProgress::Failed { error, .. } => {
                running.status = TaskStatus::Failed;
                running.updated_at = Utc::now();
                if !self
                    .store
                    .upsert_task_if_lease_current(&running, &lease, unix_millis())?
                {
                    return Ok(TaskExecutionOutcome::Deferred);
                }
                release_task_lease(&mut self.store, &lease)?;
                Err(RuntimeError::TaskFailed {
                    task_id: running.id,
                    error,
                })
            }
        }
    }

    fn complete_goal_if_ready(
        &mut self,
        goal_id: Uuid,
        out: &mut dyn Write,
    ) -> Result<bool, RuntimeError> {
        let graph = self.store.get_goal_graph(goal_id)?;
        if graph.goal.status == GoalStatus::Completed
            || !graph
                .tasks
                .iter()
                .all(|task| task.status == TaskStatus::Completed)
        {
            return Ok(false);
        }

        let mut goal = graph.goal;
        goal.status = GoalStatus::Completed;
        goal.updated_at = Utc::now();
        self.store.upsert_goal(&goal)?;
        self.emit(out, SwarmEvent::GoalCompleted { goal_id })?;
        Ok(true)
    }

    fn emit(&self, out: &mut dyn Write, event: SwarmEvent) -> Result<(), RuntimeError> {
        let envelope = SwarmEventEnvelope::new(event, Some(self.conversation_id));
        let line = format_event_line(&envelope)?;
        writeln!(out, "{}", line)?;
        out.flush()?;
        Ok(())
    }
}

fn unix_millis() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

fn task_lease_duration_millis(task: &Task) -> u64 {
    let timeout_millis = u64::try_from(task.timeout.as_millis()).unwrap_or(u64::MAX);
    timeout_millis.max(30_000)
}

fn release_task_lease(
    store: &mut SqliteStore,
    lease: &WorkerLease,
) -> Result<(), RuntimeError> {
    match store.release(lease, unix_millis())? {
        LeaseReleaseOutcome::Released | LeaseReleaseOutcome::Lost => Ok(()),
    }
}

pub fn init_project(dir: &Path) -> Result<(), RuntimeError> {
    AgentConfigFile::write_init(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nulang_ai_core::{Goal, ManagerKind, Task};
    use std::io::Cursor;

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
        assert_eq!(graph.goal.status, GoalStatus::Completed);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn resume_pending_task_reuses_durable_history() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-resume-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();

        let goal = Goal::new("resume-test", "finish persisted task", 1.0);
        let goal_id = goal.id;
        let task = Task::new(goal_id, "persist me", ManagerKind::Engineering);
        let task_id = task.id;

        {
            let rt = LocalRuntime::open(tmp.clone()).unwrap();
            rt.store().upsert_goal(&goal).unwrap();
            rt.store().upsert_task(&task).unwrap();
        }

        let mut rt = LocalRuntime::open(tmp.clone()).unwrap();
        let mut out = Cursor::new(Vec::new());
        assert_eq!(rt.resume_pending_tasks(&mut out).unwrap(), 1);

        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(graph.goal.status, GoalStatus::Completed);
        assert_eq!(
            graph.tasks.iter().find(|task| task.id == task_id).unwrap().status,
            TaskStatus::Completed
        );

        let workflow_id = WorkflowId::new(format!("agent-task:{}", task_id));
        let history = rt.store().load_workflow_history(&workflow_id).unwrap();
        assert_eq!(history.revision, 2);
        assert_eq!(history.events.len(), 2);
        let _ = std::fs::remove_dir_all(tmp);
    }
}
