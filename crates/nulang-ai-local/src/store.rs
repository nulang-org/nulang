//! Local agent persistence using libSQL's single SQLite implementation.
//! Blocking entry points run the async database driver on a scoped thread.
//! This works even if a caller is already inside a Tokio runtime, and avoids
//! another native SQLite library colliding with libsql-ffi.

use chrono::{DateTime, Utc};
use nulang_ai_core::{
    ConversationState, Goal, GoalGraph, GoalStatus, ManagerKind, Task, TaskStatus,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] libsql::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("goal not found: {0}")]
    GoalNotFound(Uuid),
    #[error("conversation not found: {0}")]
    ConversationNotFound(Uuid),
}

pub struct SqliteStore {
    path: PathBuf,
}

impl SqliteStore {
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(data_dir)?;
        let store = Self {
            path: data_dir.join("agents.db"),
        };
        store.with_connection(|conn, rt| {
            rt.block_on(async {
                conn.execute_batch(
                    r#"
            CREATE TABLE IF NOT EXISTS goals (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                conversation_id TEXT,
                intent TEXT NOT NULL,
                desired_state TEXT NOT NULL,
                constraints_json TEXT NOT NULL,
                success_criteria TEXT NOT NULL,
                budget_usd REAL NOT NULL,
                deadline TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL,
                parent_task_id TEXT,
                manager TEXT NOT NULL,
                description TEXT NOT NULL,
                dependencies TEXT NOT NULL,
                required_capabilities TEXT NOT NULL,
                acceptance_criteria TEXT NOT NULL,
                budget_usd REAL NOT NULL,
                timeout_secs INTEGER NOT NULL,
                status TEXT NOT NULL,
                assigned_agent_id TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS conversations (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                director_id TEXT,
                active_goal_id TEXT,
                messages_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
                )
                .await?;
                Ok(())
            })
        })?;
        Ok(store)
    }

    /// Keep synchronous CLI/storage callers independent of Tokio context.
    /// Each operation owns a short-lived connection on a scoped thread.
    /// Do not start a second SQLite FFI backend or call nested block_on.
    fn with_connection<T: Send>(
        &self,
        f: impl FnOnce(&libsql::Connection, &tokio::runtime::Runtime) -> Result<T, StoreError> + Send,
    ) -> Result<T, StoreError> {
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    let path = self.path.to_string_lossy().into_owned();
                    let db = rt.block_on(libsql::Builder::new_local(&path).build())?;
                    let conn = db.connect()?;
                    f(&conn, &rt)
                })
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.path
    }

    pub fn upsert_goal(&self, goal: &Goal) -> Result<(), StoreError> {
        self.with_connection(|conn, rt| {
            rt.block_on(async {
                conn.execute(
                    r#"INSERT INTO goals (
                    id, project_id, conversation_id, intent, desired_state, constraints_json,
                    success_criteria, budget_usd, deadline, status, created_at, updated_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                ON CONFLICT(id) DO UPDATE SET
                    intent=excluded.intent,
                    desired_state=excluded.desired_state,
                    constraints_json=excluded.constraints_json,
                    success_criteria=excluded.success_criteria,
                    budget_usd=excluded.budget_usd,
                    deadline=excluded.deadline,
                    status=excluded.status,
                    updated_at=excluded.updated_at"#,
                    libsql::params![
                        goal.id.to_string(),
                        goal.project_id.as_str(),
                        goal.conversation_id.map(|u| u.to_string()),
                        goal.intent.as_str(),
                        goal.desired_state.to_string(),
                        goal.constraints.to_string(),
                        serde_json::to_string(&goal.success_criteria)?,
                        goal.budget_usd,
                        goal.deadline.map(|d| d.to_rfc3339()),
                        goal_status_str(&goal.status),
                        goal.created_at.to_rfc3339(),
                        goal.updated_at.to_rfc3339(),
                    ],
                )
                .await?;
                Ok(())
            })
        })
    }

    pub fn upsert_task(&self, task: &Task) -> Result<(), StoreError> {
        self.with_connection(|conn, rt| {
            rt.block_on(async {
                conn.execute(
                    r#"INSERT INTO tasks (
                    id, goal_id, parent_task_id, manager, description, dependencies,
                    required_capabilities, acceptance_criteria, budget_usd, timeout_secs,
                    status, assigned_agent_id, created_at, updated_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                ON CONFLICT(id) DO UPDATE SET
                    description=excluded.description,
                    status=excluded.status,
                    assigned_agent_id=excluded.assigned_agent_id,
                    updated_at=excluded.updated_at"#,
                    libsql::params![
                        task.id.to_string(),
                        task.goal_id.to_string(),
                        task.parent_task_id.map(|u| u.to_string()),
                        manager_kind_str(&task.manager),
                        task.description.as_str(),
                        serde_json::to_string(&task.dependencies)?,
                        serde_json::to_string(&task.required_capabilities)?,
                        serde_json::to_string(&task.acceptance_criteria)?,
                        task.budget_usd,
                        i64::try_from(task.timeout.as_secs()).map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "task timeout exceeds i64",
                            )
                        })?,
                        task_status_str(&task.status),
                        task.assigned_agent_id.as_deref(),
                        task.created_at.to_rfc3339(),
                        task.updated_at.to_rfc3339(),
                    ],
                )
                .await?;
                Ok(())
            })
        })
    }

    pub fn upsert_conversation(&self, conv: &ConversationState) -> Result<(), StoreError> {
        self.with_connection(|conn, rt| rt.block_on(async {
            conn.execute(
                r#"INSERT INTO conversations (
                    id, project_id, director_id, active_goal_id, messages_json, created_at, updated_at
                ) VALUES (?1,?2,?3,?4,?5,?6,?7)
                ON CONFLICT(id) DO UPDATE SET
                    director_id=excluded.director_id,
                    active_goal_id=excluded.active_goal_id,
                    messages_json=excluded.messages_json,
                    updated_at=excluded.updated_at"#,
                libsql::params![
                    conv.id.to_string(),
                    conv.project_id.as_str(),
                    conv.director_id.as_deref(),
                    conv.active_goal_id.map(|u| u.to_string()),
                    serde_json::to_string(&conv.messages)?,
                    conv.created_at.to_rfc3339(),
                    conv.updated_at.to_rfc3339(),
                ],
            ).await?;
            Ok(())
        }))
    }

    pub fn get_conversation(&self, id: Uuid) -> Result<ConversationState, StoreError> {
        self.with_connection(|conn, rt| rt.block_on(async {
            let mut rows = conn.query(
                "SELECT project_id, director_id, active_goal_id, messages_json, created_at, updated_at FROM conversations WHERE id = ?1",
                [id.to_string()],
            ).await?;
            let row = rows.next().await?.ok_or(StoreError::ConversationNotFound(id))?;
            let messages: String = row.get(3)?;
            Ok(ConversationState {
                id,
                project_id: row.get(0)?,
                director_id: row.get(1)?,
                active_goal_id: row.get::<Option<String>>(2)?
                    .and_then(|s| Uuid::parse_str(&s).ok()),
                messages: serde_json::from_str(&messages)?,
                created_at: parse_ts(row.get(4)?),
                updated_at: parse_ts(row.get(5)?),
            })
        }))
    }

    pub fn get_goal_graph(&self, goal_id: Uuid) -> Result<GoalGraph, StoreError> {
        self.with_connection(|conn, rt| rt.block_on(async {
            let mut rows = conn.query(
                "SELECT project_id, conversation_id, intent, desired_state, constraints_json, success_criteria, budget_usd, deadline, status, created_at, updated_at FROM goals WHERE id = ?1",
                [goal_id.to_string()],
            ).await?;
            let row = rows.next().await?.ok_or(StoreError::GoalNotFound(goal_id))?;
            let goal = read_goal(&row, goal_id, 0)?;
            drop(rows);

            let mut rows = conn.query(
                "SELECT id, goal_id, parent_task_id, manager, description, dependencies, required_capabilities, acceptance_criteria, budget_usd, timeout_secs, status, assigned_agent_id, created_at, updated_at FROM tasks WHERE goal_id = ?1 ORDER BY created_at ASC, id ASC",
                [goal_id.to_string()],
            ).await?;
            let mut tasks = Vec::new();
            while let Some(row) = rows.next().await? {
                let id = parse_uuid_column(&row, 0)?;
                let timeout: i64 = row.get(9)?;
                let timeout = u64::try_from(timeout).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "negative task timeout")
                })?;
                tasks.push(Task {
                    id, goal_id,
                    parent_task_id: row.get::<Option<String>>(2)?
                        .and_then(|s| Uuid::parse_str(&s).ok()),
                    manager: parse_manager_kind(row.get(3)?),
                    description: row.get(4)?,
                    dependencies: serde_json::from_str(&row.get::<String>(5)?)?,
                    required_capabilities: serde_json::from_str(&row.get::<String>(6)?)?,
                    acceptance_criteria: serde_json::from_str(&row.get::<String>(7)?)?,
                    budget_usd: row.get(8)?,
                    timeout: Duration::from_secs(timeout),
                    status: parse_task_status(row.get(10)?),
                    assigned_agent_id: row.get(11)?,
                    created_at: parse_ts(row.get(12)?),
                    updated_at: parse_ts(row.get(13)?),
                });
            }
            Ok(GoalGraph { goal, tasks, agents: Vec::new() })
        }))
    }

    pub fn list_goals(&self) -> Result<Vec<Goal>, StoreError> {
        self.with_connection(|conn, rt| rt.block_on(async {
            let mut rows = conn.query(
                "SELECT id, project_id, conversation_id, intent, desired_state, constraints_json, success_criteria, budget_usd, deadline, status, created_at, updated_at FROM goals ORDER BY created_at DESC",
                (),
            ).await?;
            let mut goals = Vec::new();
            while let Some(row) = rows.next().await? {
                goals.push(read_goal(&row, parse_uuid_column(&row, 0)?, 1)?);
            }
            Ok(goals)
        }))
    }
}

fn parse_uuid_column(row: &libsql::Row, index: i32) -> Result<Uuid, StoreError> {
    let value: String = row.get(index)?;
    Uuid::parse_str(&value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e).into())
}

fn read_goal(row: &libsql::Row, id: Uuid, offset: i32) -> Result<Goal, StoreError> {
    // List queries include an ID column; point queries do not.
    Ok(Goal {
        id,
        project_id: row.get(offset)?,
        conversation_id: row
            .get::<Option<String>>(offset + 1)?
            .and_then(|s| Uuid::parse_str(&s).ok()),
        intent: row.get(offset + 2)?,
        desired_state: serde_json::from_str(&row.get::<String>(offset + 3)?)?,
        constraints: serde_json::from_str(&row.get::<String>(offset + 4)?)?,
        success_criteria: serde_json::from_str(&row.get::<String>(offset + 5)?)?,
        budget_usd: row.get(offset + 6)?,
        deadline: row
            .get::<Option<String>>(offset + 7)?
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|d| d.with_timezone(&Utc)),
        status: parse_goal_status(row.get(offset + 8)?),
        created_at: parse_ts(row.get(offset + 9)?),
        updated_at: parse_ts(row.get(offset + 10)?),
    })
}

fn parse_ts(raw: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&raw)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn goal_status_str(status: &GoalStatus) -> &'static str {
    match status {
        GoalStatus::Created => "created",
        GoalStatus::Running => "running",
        GoalStatus::Blocked => "blocked",
        GoalStatus::Verifying => "verifying",
        GoalStatus::Completed => "completed",
        GoalStatus::Cancelled => "cancelled",
    }
}

fn parse_goal_status(raw: String) -> GoalStatus {
    match raw.as_str() {
        "running" => GoalStatus::Running,
        "blocked" => GoalStatus::Blocked,
        "verifying" => GoalStatus::Verifying,
        "completed" => GoalStatus::Completed,
        "cancelled" => GoalStatus::Cancelled,
        _ => GoalStatus::Created,
    }
}

fn task_status_str(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Created => "created",
        TaskStatus::Ready => "ready",
        TaskStatus::Assigned => "assigned",
        TaskStatus::Running => "running",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Verifying => "verifying",
        TaskStatus::Failed => "failed",
        TaskStatus::Completed => "completed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn parse_task_status(raw: String) -> TaskStatus {
    match raw.as_str() {
        "ready" => TaskStatus::Ready,
        "assigned" => TaskStatus::Assigned,
        "running" => TaskStatus::Running,
        "blocked" => TaskStatus::Blocked,
        "verifying" => TaskStatus::Verifying,
        "failed" => TaskStatus::Failed,
        "completed" => TaskStatus::Completed,
        "cancelled" => TaskStatus::Cancelled,
        _ => TaskStatus::Created,
    }
}

fn manager_kind_str(kind: &ManagerKind) -> &'static str {
    match kind {
        ManagerKind::Engineering => "engineering",
        ManagerKind::Research => "research",
        ManagerKind::Operations => "operations",
        ManagerKind::Data => "data",
        ManagerKind::Voice => "voice",
    }
}

fn parse_manager_kind(raw: String) -> ManagerKind {
    match raw.as_str() {
        "research" => ManagerKind::Research,
        "operations" => ManagerKind::Operations,
        "data" => ManagerKind::Data,
        "voice" => ManagerKind::Voice,
        _ => ManagerKind::Engineering,
    }
}
