//! SQLite persistence for goals, tasks, and conversations.

use chrono::{DateTime, Utc};
use nulang_ai_core::{
    ConversationState, Goal, GoalGraph, GoalStatus, ManagerKind, Task, TaskStatus,
};
use nulang_workflow::{
    AppendOutcome, FenceCheck, LeaseAcquireOutcome, LeaseReleaseOutcome, LeaseRenewOutcome,
    WorkerLease, WorkerLeaseStore, WorkflowEvent, WorkflowHistory, WorkflowId,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("goal not found: {0}")]
    GoalNotFound(Uuid),
    #[error("conversation not found: {0}")]
    ConversationNotFound(Uuid),
    #[error("workflow history is corrupt: {0}")]
    WorkflowHistoryCorrupt(String),
    #[error("worker lease fencing token exhausted for {0}")]
    LeaseFenceExhausted(String),
}

pub struct SqliteStore {
    path: PathBuf,
}

impl SqliteStore {
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join("agents.db");
        let conn = Connection::open(&path)?;
        let store = Self { path };
        store.migrate(&conn)?;
        Ok(store)
    }

    fn migrate(&self, conn: &Connection) -> Result<(), StoreError> {
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
            CREATE TABLE IF NOT EXISTS workflow_histories (
                workflow_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                history_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS worker_leases (
                resource_id TEXT PRIMARY KEY,
                owner_id TEXT NOT NULL,
                fencing_token INTEGER NOT NULL,
                acquired_at_millis INTEGER NOT NULL,
                heartbeat_at_millis INTEGER NOT NULL,
                expires_at_millis INTEGER NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn db_path(&self) -> &Path {
        &self.path
    }

    pub fn upsert_goal(&self, goal: &Goal) -> Result<(), StoreError> {
        let conn = Connection::open(&self.path)?;
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
                updated_at=excluded.updated_at
            "#,
            params![
                goal.id.to_string(),
                goal.project_id,
                goal.conversation_id.map(|u| u.to_string()),
                goal.intent,
                goal.desired_state.to_string(),
                goal.constraints.to_string(),
                serde_json::to_string(&goal.success_criteria)?,
                goal.budget_usd,
                goal.deadline.map(|d| d.to_rfc3339()),
                goal_status_str(&goal.status),
                goal.created_at.to_rfc3339(),
                goal.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn upsert_task(&self, task: &Task) -> Result<(), StoreError> {
        let conn = Connection::open(&self.path)?;
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
                updated_at=excluded.updated_at
            "#,
            params![
                task.id.to_string(),
                task.goal_id.to_string(),
                task.parent_task_id.map(|u| u.to_string()),
                manager_kind_str(&task.manager),
                task.description,
                serde_json::to_string(&task.dependencies)?,
                serde_json::to_string(&task.required_capabilities)?,
                serde_json::to_string(&task.acceptance_criteria)?,
                task.budget_usd,
                task.timeout.as_secs() as i64,
                task_status_str(&task.status),
                task.assigned_agent_id,
                task.created_at.to_rfc3339(),
                task.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn upsert_conversation(&self, conv: &ConversationState) -> Result<(), StoreError> {
        let conn = Connection::open(&self.path)?;
        conn.execute(
            r#"INSERT INTO conversations (
                id, project_id, director_id, active_goal_id, messages_json, created_at, updated_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7)
            ON CONFLICT(id) DO UPDATE SET
                director_id=excluded.director_id,
                active_goal_id=excluded.active_goal_id,
                messages_json=excluded.messages_json,
                updated_at=excluded.updated_at
            "#,
            params![
                conv.id.to_string(),
                conv.project_id,
                conv.director_id,
                conv.active_goal_id.map(|u| u.to_string()),
                serde_json::to_string(&conv.messages)?,
                conv.created_at.to_rfc3339(),
                conv.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_conversation(&self, id: Uuid) -> Result<ConversationState, StoreError> {
        let conn = Connection::open(&self.path)?;
        conn.query_row(
            "SELECT project_id, director_id, active_goal_id, messages_json, created_at, updated_at FROM conversations WHERE id = ?1",
            params![id.to_string()],
            |row| {
                let messages: String = row.get(3)?;
                Ok(ConversationState {
                    id,
                    project_id: row.get(0)?,
                    director_id: row.get(1)?,
                    active_goal_id: row
                        .get::<_, Option<String>>(2)?
                        .and_then(|s| Uuid::parse_str(&s).ok()),
                    messages: serde_json::from_str(&messages).unwrap_or_default(),
                    created_at: parse_ts(row.get(4)?),
                    updated_at: parse_ts(row.get(5)?),
                })
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StoreError::ConversationNotFound(id),
            other => StoreError::Sqlite(other),
        })
    }

    pub fn get_goal_graph(&self, goal_id: Uuid) -> Result<GoalGraph, StoreError> {
        let conn = Connection::open(&self.path)?;
        let goal = conn
            .query_row(
                "SELECT project_id, conversation_id, intent, desired_state, constraints_json, success_criteria, budget_usd, deadline, status, created_at, updated_at FROM goals WHERE id = ?1",
                params![goal_id.to_string()],
                |row| {
                    Ok(Goal {
                        id: goal_id,
                        project_id: row.get(0)?,
                        conversation_id: row
                            .get::<_, Option<String>>(1)?
                            .and_then(|s| Uuid::parse_str(&s).ok()),
                        intent: row.get(2)?,
                        desired_state: serde_json::from_str(&row.get::<_, String>(3)?)
                            .unwrap_or(serde_json::json!({})),
                        constraints: serde_json::from_str(&row.get::<_, String>(4)?)
                            .unwrap_or(serde_json::json!({})),
                        success_criteria: serde_json::from_str(&row.get::<_, String>(5)?)
                            .unwrap_or_default(),
                        budget_usd: row.get(6)?,
                        deadline: row
                            .get::<_, Option<String>>(7)?
                            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                            .map(|d| d.with_timezone(&Utc)),
                        status: parse_goal_status(row.get(8)?),
                        created_at: parse_ts(row.get(9)?),
                        updated_at: parse_ts(row.get(10)?),
                    })
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => StoreError::GoalNotFound(goal_id),
                other => StoreError::Sqlite(other),
            })?;

        let mut stmt = conn.prepare(
            "SELECT id, goal_id, parent_task_id, manager, description, dependencies, required_capabilities, acceptance_criteria, budget_usd, timeout_secs, status, assigned_agent_id, created_at, updated_at FROM tasks WHERE goal_id = ?1",
        )?;
        let tasks = stmt
            .query_map(params![goal_id.to_string()], |row| {
                let id = Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_else(|_| Uuid::nil());
                Ok(Task {
                    id,
                    goal_id,
                    parent_task_id: row
                        .get::<_, Option<String>>(2)?
                        .and_then(|s| Uuid::parse_str(&s).ok()),
                    manager: parse_manager_kind(row.get(3)?),
                    description: row.get(4)?,
                    dependencies: serde_json::from_str(&row.get::<_, String>(5)?)
                        .unwrap_or_default(),
                    required_capabilities: serde_json::from_str(&row.get::<_, String>(6)?)
                        .unwrap_or_default(),
                    acceptance_criteria: serde_json::from_str(&row.get::<_, String>(7)?)
                        .unwrap_or_default(),
                    budget_usd: row.get(8)?,
                    timeout: Duration::from_secs(row.get::<_, i64>(9)? as u64),
                    status: parse_task_status(row.get(10)?),
                    assigned_agent_id: row.get(11)?,
                    created_at: parse_ts(row.get(12)?),
                    updated_at: parse_ts(row.get(13)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(GoalGraph {
            goal,
            tasks,
            agents: Vec::new(),
        })
    }

    pub fn load_workflow_history(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<WorkflowHistory, StoreError> {
        let conn = Connection::open(&self.path)?;
        let row = conn
            .query_row(
                "SELECT revision, history_json FROM workflow_histories WHERE workflow_id = ?1",
                params![workflow_id.as_str()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;

        let Some((revision, history_json)) = row else {
            return Ok(WorkflowHistory::default());
        };
        let history: WorkflowHistory = serde_json::from_str(&history_json)?;
        if history.revision != revision as u64 {
            return Err(StoreError::WorkflowHistoryCorrupt(format!(
                "workflow {} stores revision {} but history contains {}",
                workflow_id, revision, history.revision
            )));
        }
        Ok(history)
    }

    pub fn append_workflow_event(
        &self,
        workflow_id: &WorkflowId,
        expected_revision: u64,
        event: WorkflowEvent,
    ) -> Result<AppendOutcome, StoreError> {
        let mut conn = Connection::open(&self.path)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = tx
            .query_row(
                "SELECT revision, history_json FROM workflow_histories WHERE workflow_id = ?1",
                params![workflow_id.as_str()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;

        let mut history = match row {
            Some((revision, history_json)) => {
                if revision as u64 != expected_revision {
                    return Ok(AppendOutcome::Conflict);
                }
                let history: WorkflowHistory = serde_json::from_str(&history_json)?;
                if history.revision != expected_revision {
                    return Err(StoreError::WorkflowHistoryCorrupt(format!(
                        "workflow {} stores revision {} but history contains {}",
                        workflow_id, revision, history.revision
                    )));
                }
                history
            }
            None => {
                if expected_revision != 0 {
                    return Ok(AppendOutcome::Conflict);
                }
                WorkflowHistory::default()
            }
        };

        history.events.push(event);
        history.revision = expected_revision.saturating_add(1);
        let history_json = serde_json::to_string(&history)?;
        tx.execute(
            r#"INSERT INTO workflow_histories (
                workflow_id, revision, history_json, updated_at
            ) VALUES (?1,?2,?3,?4)
            ON CONFLICT(workflow_id) DO UPDATE SET
                revision=excluded.revision,
                history_json=excluded.history_json,
                updated_at=excluded.updated_at
            "#,
            params![
                workflow_id.as_str(),
                history.revision as i64,
                history_json,
                Utc::now().to_rfc3339(),
            ],
        )?;
        tx.commit()?;

        Ok(AppendOutcome::Appended {
            new_revision: history.revision,
        })
    }

    pub fn list_resumable_tasks(&self) -> Result<Vec<Task>, StoreError> {
        let conn = Connection::open(&self.path)?;
        let mut stmt = conn.prepare(
            "SELECT id, goal_id, parent_task_id, manager, description, dependencies, required_capabilities, acceptance_criteria, budget_usd, timeout_secs, status, assigned_agent_id, created_at, updated_at
             FROM tasks
             WHERE status IN ('created', 'ready', 'assigned', 'running')
             ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let id = Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_else(|_| Uuid::nil());
            let goal_id =
                Uuid::parse_str(&row.get::<_, String>(1)?).unwrap_or_else(|_| Uuid::nil());
            Ok(Task {
                id,
                goal_id,
                parent_task_id: row
                    .get::<_, Option<String>>(2)?
                    .and_then(|s| Uuid::parse_str(&s).ok()),
                manager: parse_manager_kind(row.get(3)?),
                description: row.get(4)?,
                dependencies: serde_json::from_str(&row.get::<_, String>(5)?)
                    .unwrap_or_default(),
                required_capabilities: serde_json::from_str(&row.get::<_, String>(6)?)
                    .unwrap_or_default(),
                acceptance_criteria: serde_json::from_str(&row.get::<_, String>(7)?)
                    .unwrap_or_default(),
                budget_usd: row.get(8)?,
                timeout: Duration::from_secs(row.get::<_, i64>(9)? as u64),
                status: parse_task_status(row.get(10)?),
                assigned_agent_id: row.get(11)?,
                created_at: parse_ts(row.get(12)?),
                updated_at: parse_ts(row.get(13)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn upsert_task_if_lease_current(
        &self,
        task: &Task,
        lease: &WorkerLease,
        now_millis: u64,
    ) -> Result<bool, StoreError> {
        let mut conn = Connection::open(&self.path)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = tx
            .query_row(
                "SELECT owner_id, fencing_token, expires_at_millis FROM worker_leases WHERE resource_id = ?1",
                params![lease.resource_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;

        let Some((owner_id, fencing_token, expires_at_millis)) = current else {
            return Ok(false);
        };
        let fence_is_current = owner_id == lease.owner_id
            && fencing_token >= 0
            && fencing_token as u64 == lease.fencing_token
            && expires_at_millis >= 0
            && expires_at_millis as u64 > now_millis;
        if !fence_is_current {
            return Ok(false);
        }

        upsert_task_in_tx(&tx, task)?;
        tx.commit()?;
        Ok(true)
    }

    pub fn list_goals(&self) -> Result<Vec<Goal>, StoreError> {
        let conn = Connection::open(&self.path)?;
        let mut stmt = conn.prepare(
            "SELECT id, project_id, conversation_id, intent, desired_state, constraints_json, success_criteria, budget_usd, deadline, status, created_at, updated_at FROM goals ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let id = Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_else(|_| Uuid::nil());
            Ok(Goal {
                id,
                project_id: row.get(1)?,
                conversation_id: row
                    .get::<_, Option<String>>(2)?
                    .and_then(|s| Uuid::parse_str(&s).ok()),
                intent: row.get(3)?,
                desired_state: serde_json::from_str(&row.get::<_, String>(4)?)
                    .unwrap_or(serde_json::json!({})),
                constraints: serde_json::from_str(&row.get::<_, String>(5)?)
                    .unwrap_or(serde_json::json!({})),
                success_criteria: serde_json::from_str(&row.get::<_, String>(6)?)
                    .unwrap_or_default(),
                budget_usd: row.get(7)?,
                deadline: row
                    .get::<_, Option<String>>(8)?
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|d| d.with_timezone(&Utc)),
                status: parse_goal_status(row.get(9)?),
                created_at: parse_ts(row.get(10)?),
                updated_at: parse_ts(row.get(11)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

impl WorkerLeaseStore for SqliteStore {
    type Error = StoreError;

    fn try_acquire(
        &mut self,
        resource_id: &str,
        owner_id: &str,
        now_millis: u64,
        lease_duration_millis: u64,
    ) -> Result<LeaseAcquireOutcome, Self::Error> {
        let mut conn = Connection::open(&self.path)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = tx
            .query_row(
                "SELECT owner_id, fencing_token, acquired_at_millis, heartbeat_at_millis, expires_at_millis
                 FROM worker_leases WHERE resource_id = ?1",
                params![resource_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;

        if let Some((
            current_owner,
            fencing_token,
            acquired_at_millis,
            heartbeat_at_millis,
            expires_at_millis,
        )) = row
        {
            let token = nonnegative_u64(fencing_token);
            let expires = nonnegative_u64(expires_at_millis);
            if expires > now_millis {
                let lease = WorkerLease {
                    resource_id: resource_id.to_owned(),
                    owner_id: current_owner.clone(),
                    fencing_token: token,
                    acquired_at_millis: nonnegative_u64(acquired_at_millis),
                    heartbeat_at_millis: nonnegative_u64(heartbeat_at_millis),
                    expires_at_millis: expires,
                };
                if current_owner == owner_id {
                    return Ok(LeaseAcquireOutcome::AlreadyHeldByCaller(lease));
                }
                return Ok(LeaseAcquireOutcome::HeldByOther {
                    owner_id: current_owner,
                    fencing_token: token,
                    expires_at_millis: expires,
                });
            }

            if fencing_token >= i64::MAX {
                return Err(StoreError::LeaseFenceExhausted(resource_id.to_owned()));
            }
            let next_token = fencing_token.saturating_add(1).max(1);
            let expires = lease_expiry(now_millis, lease_duration_millis);
            tx.execute(
                "UPDATE worker_leases
                 SET owner_id = ?2, fencing_token = ?3, acquired_at_millis = ?4,
                     heartbeat_at_millis = ?4, expires_at_millis = ?5
                 WHERE resource_id = ?1",
                params![
                    resource_id,
                    owner_id,
                    next_token,
                    sqlite_millis(now_millis),
                    sqlite_millis(expires),
                ],
            )?;
            tx.commit()?;
            return Ok(LeaseAcquireOutcome::Acquired(WorkerLease {
                resource_id: resource_id.to_owned(),
                owner_id: owner_id.to_owned(),
                fencing_token: next_token as u64,
                acquired_at_millis: now_millis,
                heartbeat_at_millis: now_millis,
                expires_at_millis: expires,
            }));
        }

        let expires = lease_expiry(now_millis, lease_duration_millis);
        tx.execute(
            "INSERT INTO worker_leases (
                resource_id, owner_id, fencing_token, acquired_at_millis,
                heartbeat_at_millis, expires_at_millis
             ) VALUES (?1,?2,1,?3,?3,?4)",
            params![
                resource_id,
                owner_id,
                sqlite_millis(now_millis),
                sqlite_millis(expires),
            ],
        )?;
        tx.commit()?;
        Ok(LeaseAcquireOutcome::Acquired(WorkerLease {
            resource_id: resource_id.to_owned(),
            owner_id: owner_id.to_owned(),
            fencing_token: 1,
            acquired_at_millis: now_millis,
            heartbeat_at_millis: now_millis,
            expires_at_millis: expires,
        }))
    }

    fn heartbeat(
        &mut self,
        lease: &WorkerLease,
        now_millis: u64,
        lease_duration_millis: u64,
    ) -> Result<LeaseRenewOutcome, Self::Error> {
        let mut conn = Connection::open(&self.path)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = tx
            .query_row(
                "SELECT owner_id, fencing_token, acquired_at_millis, expires_at_millis
                 FROM worker_leases WHERE resource_id = ?1",
                params![lease.resource_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;

        let Some((owner_id, fencing_token, acquired_at_millis, expires_at_millis)) = row else {
            return Ok(LeaseRenewOutcome::Lost);
        };
        if owner_id != lease.owner_id
            || nonnegative_u64(fencing_token) != lease.fencing_token
            || nonnegative_u64(expires_at_millis) <= now_millis
        {
            return Ok(LeaseRenewOutcome::Lost);
        }

        let expires = lease_expiry(now_millis, lease_duration_millis);
        tx.execute(
            "UPDATE worker_leases
             SET heartbeat_at_millis = ?2, expires_at_millis = ?3
             WHERE resource_id = ?1",
            params![
                lease.resource_id,
                sqlite_millis(now_millis),
                sqlite_millis(expires),
            ],
        )?;
        tx.commit()?;
        Ok(LeaseRenewOutcome::Renewed(WorkerLease {
            resource_id: lease.resource_id.clone(),
            owner_id,
            fencing_token: lease.fencing_token,
            acquired_at_millis: nonnegative_u64(acquired_at_millis),
            heartbeat_at_millis: now_millis,
            expires_at_millis: expires,
        }))
    }

    fn release(
        &mut self,
        lease: &WorkerLease,
        now_millis: u64,
    ) -> Result<LeaseReleaseOutcome, Self::Error> {
        if lease.fencing_token > i64::MAX as u64 {
            return Ok(LeaseReleaseOutcome::Lost);
        }
        let mut conn = Connection::open(&self.path)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE worker_leases
             SET heartbeat_at_millis = ?4, expires_at_millis = ?4
             WHERE resource_id = ?1 AND owner_id = ?2 AND fencing_token = ?3
               AND expires_at_millis > ?4",
            params![
                lease.resource_id,
                lease.owner_id,
                lease.fencing_token as i64,
                sqlite_millis(now_millis),
            ],
        )?;
        if changed == 0 {
            return Ok(LeaseReleaseOutcome::Lost);
        }
        tx.commit()?;
        Ok(LeaseReleaseOutcome::Released)
    }

    fn check_fence(
        &mut self,
        lease: &WorkerLease,
        now_millis: u64,
    ) -> Result<FenceCheck, Self::Error> {
        let conn = Connection::open(&self.path)?;
        let current = conn
            .query_row(
                "SELECT owner_id, fencing_token, expires_at_millis
                 FROM worker_leases WHERE resource_id = ?1",
                params![lease.resource_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;

        let Some((owner_id, fencing_token, expires_at_millis)) = current else {
            return Ok(FenceCheck::Lost);
        };
        if owner_id == lease.owner_id
            && nonnegative_u64(fencing_token) == lease.fencing_token
            && nonnegative_u64(expires_at_millis) > now_millis
        {
            Ok(FenceCheck::Current)
        } else {
            Ok(FenceCheck::Lost)
        }
    }
}

fn lease_expiry(now_millis: u64, lease_duration_millis: u64) -> u64 {
    now_millis
        .saturating_add(lease_duration_millis.max(1))
        .min(i64::MAX as u64)
}

fn sqlite_millis(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn nonnegative_u64(value: i64) -> u64 {
    value.max(0) as u64
}

fn upsert_task_in_tx(
    tx: &rusqlite::Transaction<'_>,
    task: &Task,
) -> Result<(), StoreError> {
    tx.execute(
        r#"INSERT INTO tasks (
            id, goal_id, parent_task_id, manager, description, dependencies,
            required_capabilities, acceptance_criteria, budget_usd, timeout_secs,
            status, assigned_agent_id, created_at, updated_at
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
        ON CONFLICT(id) DO UPDATE SET
            description=excluded.description,
            status=excluded.status,
            assigned_agent_id=excluded.assigned_agent_id,
            updated_at=excluded.updated_at
        "#,
        params![
            task.id.to_string(),
            task.goal_id.to_string(),
            task.parent_task_id.map(|u| u.to_string()),
            manager_kind_str(&task.manager),
            task.description,
            serde_json::to_string(&task.dependencies)?,
            serde_json::to_string(&task.required_capabilities)?,
            serde_json::to_string(&task.acceptance_criteria)?,
            task.budget_usd,
            task.timeout.as_secs() as i64,
            task_status_str(&task.status),
            task.assigned_agent_id,
            task.created_at.to_rfc3339(),
            task.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
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


#[cfg(test)]
mod tests {
    use super::*;
    use nulang_workflow::RetryPolicy;

    fn temp_store() -> (PathBuf, SqliteStore) {
        let dir = std::env::temp_dir().join(format!("nulang-ai-store-test-{}", Uuid::new_v4()));
        let store = SqliteStore::open(&dir).unwrap();
        (dir, store)
    }

    #[test]
    fn worker_lease_reacquire_increments_fencing_token() {
        let (dir, mut store) = temp_store();

        let first = match store
            .try_acquire("task:1", "worker:a", 100, 100)
            .unwrap()
        {
            LeaseAcquireOutcome::Acquired(lease) => lease,
            other => panic!("unexpected acquire result: {other:?}"),
        };
        assert_eq!(first.fencing_token, 1);

        assert!(matches!(
            store.try_acquire("task:1", "worker:b", 150, 100).unwrap(),
            LeaseAcquireOutcome::HeldByOther { .. }
        ));

        let second = match store
            .try_acquire("task:1", "worker:b", 200, 100)
            .unwrap()
        {
            LeaseAcquireOutcome::Acquired(lease) => lease,
            other => panic!("unexpected reacquire result: {other:?}"),
        };
        assert_eq!(second.fencing_token, 2);

        let third = match store
            .try_acquire("task:1", "worker:b", 300, 100)
            .unwrap()
        {
            LeaseAcquireOutcome::Acquired(lease) => lease,
            other => panic!("unexpected same-owner reacquire result: {other:?}"),
        };
        assert_eq!(third.fencing_token, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn worker_heartbeat_extends_current_lease_only() {
        let (dir, mut store) = temp_store();
        let lease = match store
            .try_acquire("task:1", "worker:a", 100, 100)
            .unwrap()
        {
            LeaseAcquireOutcome::Acquired(lease) => lease,
            other => panic!("unexpected acquire result: {other:?}"),
        };

        let renewed = match store.heartbeat(&lease, 150, 200).unwrap() {
            LeaseRenewOutcome::Renewed(lease) => lease,
            other => panic!("unexpected heartbeat result: {other:?}"),
        };
        assert_eq!(renewed.expires_at_millis, 350);
        assert_eq!(renewed.fencing_token, lease.fencing_token);

        assert_eq!(
            store.check_fence(&renewed, 349).unwrap(),
            FenceCheck::Current
        );
        assert_eq!(
            store.check_fence(&renewed, 350).unwrap(),
            FenceCheck::Lost
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_fence_cannot_commit_task_state() {
        let (dir, mut store) = temp_store();
        let goal = Goal::new("lease-test", "guard task commit", 1.0);
        let mut task = Task::new(goal.id, "work", ManagerKind::Engineering);
        store.upsert_goal(&goal).unwrap();
        store.upsert_task(&task).unwrap();

        let stale = match store
            .try_acquire("task:1", "worker:a", 100, 100)
            .unwrap()
        {
            LeaseAcquireOutcome::Acquired(lease) => lease,
            other => panic!("unexpected acquire result: {other:?}"),
        };
        let current = match store
            .try_acquire("task:1", "worker:b", 200, 100)
            .unwrap()
        {
            LeaseAcquireOutcome::Acquired(lease) => lease,
            other => panic!("unexpected reacquire result: {other:?}"),
        };

        task.status = TaskStatus::Completed;
        assert!(!store
            .upsert_task_if_lease_current(&task, &stale, 210)
            .unwrap());
        assert!(store
            .upsert_task_if_lease_current(&task, &current, 210)
            .unwrap());

        let graph = store.get_goal_graph(goal.id).unwrap();
        assert_eq!(graph.tasks[0].status, TaskStatus::Completed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn workflow_history_append_uses_revision_cas() {
        let (dir, store) = temp_store();
        let workflow_id = WorkflowId::new("agent-task:test");
        let event = WorkflowEvent::ActivityPrepared {
            invocation_id: nulang_workflow::ActivityInvocationId::derive(
                &workflow_id,
                "execute",
                0,
                "agent.worker.execute",
            ),
            step: "execute".into(),
            operation: "agent.worker.execute".into(),
            request: b"task".to_vec(),
            retry: RetryPolicy::none(),
        };

        assert_eq!(
            store
                .append_workflow_event(&workflow_id, 0, event.clone())
                .unwrap(),
            AppendOutcome::Appended { new_revision: 1 }
        );
        assert_eq!(
            store
                .append_workflow_event(&workflow_id, 0, event)
                .unwrap(),
            AppendOutcome::Conflict
        );

        let history = store.load_workflow_history(&workflow_id).unwrap();
        assert_eq!(history.revision, 1);
        assert_eq!(history.events.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
