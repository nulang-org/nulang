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
use nulang_ai_worker::{LocalWorker, Worker};
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
    worker: LocalWorker,
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
        for task in tasks {
            self.store.upsert_task(&task)?;
            self.emit(
                out,
                SwarmEvent::TaskCreated {
                    task_id: task.id,
                    goal_id,
                },
            )?;

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

            let completed = self.worker.execute(&running);
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
}
