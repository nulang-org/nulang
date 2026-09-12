//! Local agent runtime: Director + Manager + Worker + SQLite + NLAP events.

use crate::config::AgentConfigFile;
use crate::reservation::{ReservationError, TaskReservationStore};
use crate::store::{SqliteStore, StoreError};
use chrono::Utc;
use nulang_ai_core::{
    ConversationMessage, ConversationState, GoalStatus, SwarmEvent, SwarmEventEnvelope, TaskStatus,
};
use nulang_ai_director::{Director, LocalDirector};
use nulang_ai_manager::{EngineeringManager, Manager};
use nulang_ai_protocol::format_event_line;
use nulang_ai_worker::{LocalWorker, WorkerRegistry, WorkerError};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("reservation error: {0}")]
    Reservation(#[from] ReservationError),
    #[error("worker error: {0}")]
    Worker(#[from] WorkerError),
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
    reservations: TaskReservationStore,
    conversation_id: Uuid,
    project_id: String,
    director: LocalDirector,
    engineering: EngineeringManager,
    workers: WorkerRegistry,
}

impl LocalRuntime {
    pub fn open(project_dir: PathBuf) -> Result<Self, RuntimeError> {
        if !project_dir.join("agent.toml").exists() {
            return Err(RuntimeError::MissingConfig(project_dir));
        }
        let config = AgentConfigFile::load(&project_dir)?;
        let data_dir = config.resolve_data_dir(&project_dir);
        let store = SqliteStore::open(&data_dir)?;
        let reservations = TaskReservationStore::open(store.db_path())?;
        reservations.reclaim_expired_running_tasks(Utc::now().timestamp_millis())?;

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

        let snapshots = store.list_worker_snapshots()?;
        let mut workers = WorkerRegistry::from_snapshots(
            WorkerRegistry::DEFAULT_LEASE_TIMEOUT_MS,
            snapshots,
        );
        if workers.workers().is_empty() {
            workers.register(LocalWorker::new("worker-local"));
            store.replace_worker_snapshots(&workers.snapshots())?;
        }

        Ok(Self {
            project_dir,
            config,
            store,
            reservations,
            conversation_id,
            project_id,
            director: LocalDirector::new("director-local"),
            engineering: EngineeringManager,
            workers,
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

    pub fn register_worker(&mut self, worker: LocalWorker) -> Result<(), RuntimeError> {
        self.workers.register(worker);
        self.persist_workers()?;
        Ok(())
    }

    pub fn heartbeat_worker(&mut self, agent_id: &str) -> Result<(), RuntimeError> {
        self.workers.heartbeat(agent_id)?;
        self.persist_workers()?;
        Ok(())
    }

    pub fn set_worker_available(
        &mut self,
        agent_id: &str,
        available: bool,
    ) -> Result<(), RuntimeError> {
        self.workers.set_available(agent_id, available)?;
        self.persist_workers()?;
        Ok(())
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

        let tasks = self.engineering.plan_goal(&goal);
        for mut task in tasks {
            let agent_id = self.workers.select_agent_id(&task)?;
            let max_concurrency = self
                .workers
                .get(&agent_id)
                .map(|record| record.metadata.max_concurrency)
                .unwrap_or(1);
            let lease_duration_ms = i64::try_from(task.timeout.as_millis())
                .unwrap_or(i64::MAX)
                .max(30_000);
            let reservation = self.reservations.reserve_with_capacity(
                task.id,
                &agent_id,
                max_concurrency,
                Utc::now().timestamp_millis(),
                lease_duration_ms,
            )?;

            task.assigned_agent_id = Some(agent_id.clone());
            self.store.upsert_task(&task)?;
            self.emit(
                out,
                SwarmEvent::TaskCreated {
                    task_id: task.id,
                    goal_id,
                },
            )?;

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

            let execution = self.workers.execute_on(&agent_id, &running);
            let release = self.reservations.release(&reservation);
            self.persist_workers()?;
            let completed = execution?;
            release?;
            self.store.upsert_task(&completed)?;
            self.emit(
                out,
                SwarmEvent::TaskCompleted {
                    task_id: completed.id,
                    agent_id,
                },
            )?;
        }

        goal.status = GoalStatus::Completed;
        goal.updated_at = Utc::now();
        self.store.upsert_goal(&goal)?;
        self.emit(out, SwarmEvent::GoalCompleted { goal_id })?;

        Ok(goal_id)
    }

    fn persist_workers(&self) -> Result<(), RuntimeError> {
        self.store.replace_worker_snapshots(&self.workers.snapshots())?;
        Ok(())
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
        assert_eq!(
            graph.tasks[0].assigned_agent_id.as_deref(),
            Some("worker-local")
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn scheduler_prefers_least_privileged_eligible_worker() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();
        let mut rt = LocalRuntime::open(tmp.clone()).unwrap();
        rt.register_worker(LocalWorker::with_capabilities(
            "worker-admin",
            ["code", "test", "repo.write", "deploy.execute"],
        ))
        .unwrap();
        rt.register_worker(LocalWorker::with_capabilities(
            "worker-specialist",
            ["code", "test"],
        ))
        .unwrap();

        let mut buf = Cursor::new(Vec::new());
        let goal_id = rt.handle_user_message("ship feature X", &mut buf).unwrap();
        let graph = rt.store().get_goal_graph(goal_id).unwrap();
        assert_eq!(
            graph.tasks[0].assigned_agent_id.as_deref(),
            Some("worker-local")
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn worker_registry_survives_runtime_restart() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();

        {
            let mut rt = LocalRuntime::open(tmp.clone()).unwrap();
            rt.register_worker(LocalWorker::with_capabilities(
                "worker-persistent",
                ["code", "test", "repo.write"],
            ))
            .unwrap();
            rt.set_worker_available("worker-persistent", false).unwrap();
        }

        let reopened = LocalRuntime::open(tmp.clone()).unwrap();
        let snapshots = reopened.store().list_worker_snapshots().unwrap();
        let persisted = snapshots
            .iter()
            .find(|snapshot| snapshot.agent_id == "worker-persistent")
            .unwrap();
        assert!(persisted.capabilities.contains(&"repo.write".into()));
        assert!(!persisted.available);

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn runtime_restart_requeues_task_with_expired_reservation() {
        let tmp = std::env::temp_dir().join(format!("nulang-agent-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        init_project(&tmp).unwrap();

        let goal_id;
        let task_id;
        {
            let rt = LocalRuntime::open(tmp.clone()).unwrap();
            let goal = Goal::new("recovery-test", "recover task", 1.0);
            goal_id = goal.id;
            rt.store.upsert_goal(&goal).unwrap();

            let mut task = Task::new(goal_id, "recover me", ManagerKind::Engineering);
            task.status = TaskStatus::Running;
            task.assigned_agent_id = Some("worker-local".into());
            task_id = task.id;
            rt.store.upsert_task(&task).unwrap();
            rt.reservations
                .reserve_with_capacity(task_id, "worker-local", 1, 1_000, 100)
                .unwrap();
        }

        let reopened = LocalRuntime::open(tmp.clone()).unwrap();
        let graph = reopened.store().get_goal_graph(goal_id).unwrap();
        let recovered = graph.tasks.iter().find(|task| task.id == task_id).unwrap();
        assert_eq!(recovered.status, TaskStatus::Ready);
        assert_eq!(recovered.assigned_agent_id, None);

        let _ = std::fs::remove_dir_all(tmp);
    }
}
