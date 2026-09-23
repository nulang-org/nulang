//! SQLite persistence for goals, tasks, commitments, intentions, and conversations.

use chrono::{DateTime, Utc};
use nulang_ai_core::{
    Commitment, CommitmentStatus, ConversationState, Goal, GoalGraph, GoalStatus, Intention,
    IntentionRevision, IntentionRevisionDecision, IntentionStatus, ManagerKind, Task, TaskStatus,
};
use rusqlite::{params, Connection, TransactionBehavior};
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
    #[error("invalid agent state transition: {0}")]
    InvalidTransition(&'static str),
}

pub struct AgentStateTransition<'a> {
    pub terminal_task: Option<&'a Task>,
    pub intention: &'a Intention,
    pub replacement_intention: Option<&'a Intention>,
    pub revision: Option<&'a IntentionRevision>,
    pub commitment: Option<&'a Commitment>,
    pub goal: Option<&'a Goal>,
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
            CREATE TABLE IF NOT EXISTS commitments (
                id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL,
                owner_agent_id TEXT NOT NULL,
                rationale TEXT NOT NULL,
                success_criteria TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS commitments_goal_idx ON commitments(goal_id);
            CREATE TABLE IF NOT EXISTS intentions (
                id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL,
                commitment_id TEXT NOT NULL,
                owner_agent_id TEXT NOT NULL,
                description TEXT NOT NULL,
                planned_task_ids TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS intentions_goal_idx ON intentions(goal_id);
            CREATE INDEX IF NOT EXISTS intentions_commitment_idx ON intentions(commitment_id);
            CREATE TABLE IF NOT EXISTS intention_revisions (
                id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL,
                commitment_id TEXT NOT NULL,
                superseded_intention_id TEXT NOT NULL,
                replacement_intention_id TEXT,
                trigger_task_id TEXT NOT NULL,
                trigger_status TEXT NOT NULL,
                decision TEXT NOT NULL,
                reason TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS intention_revisions_goal_idx ON intention_revisions(goal_id);
            CREATE INDEX IF NOT EXISTS intention_revisions_commitment_idx ON intention_revisions(commitment_id);
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
        )?;
        Ok(())
    }

    pub fn db_path(&self) -> &Path {
        &self.path
    }

    pub fn upsert_goal(&self, goal: &Goal) -> Result<(), StoreError> {
        let conn = Connection::open(&self.path)?;
        upsert_goal_conn(&conn, goal)
    }

    pub fn upsert_task(&self, task: &Task) -> Result<(), StoreError> {
        let conn = Connection::open(&self.path)?;
        upsert_task_conn(&conn, task)
    }

    pub fn upsert_commitment(&self, commitment: &Commitment) -> Result<(), StoreError> {
        let conn = Connection::open(&self.path)?;
        upsert_commitment_conn(&conn, commitment)
    }

    pub fn upsert_intention(&self, intention: &Intention) -> Result<(), StoreError> {
        let conn = Connection::open(&self.path)?;
        upsert_intention_conn(&conn, intention)
    }

    pub fn insert_intention_revision(
        &self,
        revision: &IntentionRevision,
    ) -> Result<(), StoreError> {
        let conn = Connection::open(&self.path)?;
        insert_intention_revision_conn(&conn, revision)
    }

    pub fn commit_agent_state_transition(
        &self,
        transition: AgentStateTransition<'_>,
    ) -> Result<(), StoreError> {
        validate_agent_state_transition(&transition)?;

        let mut conn = Connection::open(&self.path)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(task) = transition.terminal_task {
            upsert_task_conn(&tx, task)?;
        }
        upsert_intention_conn(&tx, transition.intention)?;
        if let Some(replacement) = transition.replacement_intention {
            upsert_intention_conn(&tx, replacement)?;
        }
        if let Some(revision) = transition.revision {
            insert_intention_revision_conn(&tx, revision)?;
        }
        if let Some(commitment) = transition.commitment {
            upsert_commitment_conn(&tx, commitment)?;
        }
        if let Some(goal) = transition.goal {
            upsert_goal_conn(&tx, goal)?;
        }

        tx.commit()?;
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

        let mut commitment_stmt = conn.prepare(
            "SELECT id, owner_agent_id, rationale, success_criteria, status, created_at, updated_at FROM commitments WHERE goal_id = ?1 ORDER BY created_at ASC",
        )?;
        let commitments = commitment_stmt
            .query_map(params![goal_id.to_string()], |row| {
                let id = Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_else(|_| Uuid::nil());
                Ok(Commitment {
                    id,
                    goal_id,
                    owner_agent_id: row.get(1)?,
                    rationale: row.get(2)?,
                    success_criteria: serde_json::from_str(&row.get::<_, String>(3)?)
                        .unwrap_or_default(),
                    status: parse_commitment_status(row.get(4)?),
                    created_at: parse_ts(row.get(5)?),
                    updated_at: parse_ts(row.get(6)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut intention_stmt = conn.prepare(
            "SELECT id, commitment_id, owner_agent_id, description, planned_task_ids, status, created_at, updated_at FROM intentions WHERE goal_id = ?1 ORDER BY created_at ASC",
        )?;
        let intentions = intention_stmt
            .query_map(params![goal_id.to_string()], |row| {
                let id = Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_else(|_| Uuid::nil());
                let commitment_id =
                    Uuid::parse_str(&row.get::<_, String>(1)?).unwrap_or_else(|_| Uuid::nil());
                Ok(Intention {
                    id,
                    goal_id,
                    commitment_id,
                    owner_agent_id: row.get(2)?,
                    description: row.get(3)?,
                    planned_task_ids: serde_json::from_str(&row.get::<_, String>(4)?)
                        .unwrap_or_default(),
                    status: parse_intention_status(row.get(5)?),
                    created_at: parse_ts(row.get(6)?),
                    updated_at: parse_ts(row.get(7)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut revision_stmt = conn.prepare(
            "SELECT id, commitment_id, superseded_intention_id, replacement_intention_id, trigger_task_id, trigger_status, decision, reason, created_at FROM intention_revisions WHERE goal_id = ?1 ORDER BY created_at ASC",
        )?;
        let intention_revisions = revision_stmt
            .query_map(params![goal_id.to_string()], |row| {
                let id = Uuid::parse_str(&row.get::<_, String>(0)?).unwrap_or_else(|_| Uuid::nil());
                let commitment_id =
                    Uuid::parse_str(&row.get::<_, String>(1)?).unwrap_or_else(|_| Uuid::nil());
                let superseded_intention_id =
                    Uuid::parse_str(&row.get::<_, String>(2)?).unwrap_or_else(|_| Uuid::nil());
                let replacement_intention_id = row
                    .get::<_, Option<String>>(3)?
                    .and_then(|s| Uuid::parse_str(&s).ok());
                let trigger_task_id =
                    Uuid::parse_str(&row.get::<_, String>(4)?).unwrap_or_else(|_| Uuid::nil());
                Ok(IntentionRevision {
                    id,
                    goal_id,
                    commitment_id,
                    superseded_intention_id,
                    replacement_intention_id,
                    trigger_task_id,
                    trigger_status: parse_task_status(row.get(5)?),
                    decision: parse_revision_decision(row.get(6)?),
                    reason: row.get(7)?,
                    created_at: parse_ts(row.get(8)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(GoalGraph {
            goal,
            tasks,
            agents: Vec::new(),
            commitments,
            intentions,
            intention_revisions,
        })
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

fn validate_agent_state_transition(
    transition: &AgentStateTransition<'_>,
) -> Result<(), StoreError> {
    let intention = transition.intention;

    if let Some(task) = transition.terminal_task {
        if task.goal_id != intention.goal_id {
            return Err(StoreError::InvalidTransition(
                "terminal task and intention belong to different goals",
            ));
        }
        if !intention.planned_task_ids.contains(&task.id) {
            return Err(StoreError::InvalidTransition(
                "terminal task is not part of the intention plan",
            ));
        }
    }

    if let Some(revision) = transition.revision {
        if revision.goal_id != intention.goal_id
            || revision.commitment_id != intention.commitment_id
            || revision.superseded_intention_id != intention.id
        {
            return Err(StoreError::InvalidTransition(
                "revision does not describe the supplied intention",
            ));
        }

        if let Some(replacement) = transition.replacement_intention {
            if replacement.goal_id != intention.goal_id
                || replacement.commitment_id != intention.commitment_id
                || revision.replacement_intention_id != Some(replacement.id)
            {
                return Err(StoreError::InvalidTransition(
                    "replacement intention does not match the revision",
                ));
            }
        } else if revision.replacement_intention_id.is_some() {
            return Err(StoreError::InvalidTransition(
                "revision names a replacement intention that was not supplied",
            ));
        }
    } else if transition.replacement_intention.is_some() {
        return Err(StoreError::InvalidTransition(
            "replacement intention requires a revision record",
        ));
    }

    if let Some(commitment) = transition.commitment {
        if commitment.id != intention.commitment_id || commitment.goal_id != intention.goal_id {
            return Err(StoreError::InvalidTransition(
                "commitment does not match the intention",
            ));
        }
    }

    if let Some(goal) = transition.goal {
        if goal.id != intention.goal_id {
            return Err(StoreError::InvalidTransition(
                "goal does not match the intention",
            ));
        }
    }

    Ok(())
}

fn upsert_goal_conn(conn: &Connection, goal: &Goal) -> Result<(), StoreError> {
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

fn upsert_task_conn(conn: &Connection, task: &Task) -> Result<(), StoreError> {
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

fn upsert_commitment_conn(
    conn: &Connection,
    commitment: &Commitment,
) -> Result<(), StoreError> {
    conn.execute(
        r#"INSERT INTO commitments (
            id, goal_id, owner_agent_id, rationale, success_criteria, status, created_at, updated_at
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
        ON CONFLICT(id) DO UPDATE SET
            owner_agent_id=excluded.owner_agent_id,
            rationale=excluded.rationale,
            success_criteria=excluded.success_criteria,
            status=excluded.status,
            updated_at=excluded.updated_at
        "#,
        params![
            commitment.id.to_string(),
            commitment.goal_id.to_string(),
            commitment.owner_agent_id,
            commitment.rationale,
            serde_json::to_string(&commitment.success_criteria)?,
            commitment_status_str(&commitment.status),
            commitment.created_at.to_rfc3339(),
            commitment.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn upsert_intention_conn(conn: &Connection, intention: &Intention) -> Result<(), StoreError> {
    conn.execute(
        r#"INSERT INTO intentions (
            id, goal_id, commitment_id, owner_agent_id, description, planned_task_ids,
            status, created_at, updated_at
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
        ON CONFLICT(id) DO UPDATE SET
            owner_agent_id=excluded.owner_agent_id,
            description=excluded.description,
            planned_task_ids=excluded.planned_task_ids,
            status=excluded.status,
            updated_at=excluded.updated_at
        "#,
        params![
            intention.id.to_string(),
            intention.goal_id.to_string(),
            intention.commitment_id.to_string(),
            intention.owner_agent_id,
            intention.description,
            serde_json::to_string(&intention.planned_task_ids)?,
            intention_status_str(&intention.status),
            intention.created_at.to_rfc3339(),
            intention.updated_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn insert_intention_revision_conn(
    conn: &Connection,
    revision: &IntentionRevision,
) -> Result<(), StoreError> {
    conn.execute(
        r#"INSERT INTO intention_revisions (
            id, goal_id, commitment_id, superseded_intention_id, replacement_intention_id,
            trigger_task_id, trigger_status, decision, reason, created_at
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
        ON CONFLICT(id) DO NOTHING
        "#,
        params![
            revision.id.to_string(),
            revision.goal_id.to_string(),
            revision.commitment_id.to_string(),
            revision.superseded_intention_id.to_string(),
            revision.replacement_intention_id.map(|u| u.to_string()),
            revision.trigger_task_id.to_string(),
            task_status_str(&revision.trigger_status),
            revision_decision_str(&revision.decision),
            revision.reason,
            revision.created_at.to_rfc3339(),
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
        GoalStatus::Failed => "failed",
        GoalStatus::Cancelled => "cancelled",
    }
}

fn parse_goal_status(raw: String) -> GoalStatus {
    match raw.as_str() {
        "running" => GoalStatus::Running,
        "blocked" => GoalStatus::Blocked,
        "verifying" => GoalStatus::Verifying,
        "completed" => GoalStatus::Completed,
        "failed" => GoalStatus::Failed,
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

fn commitment_status_str(status: &CommitmentStatus) -> &'static str {
    match status {
        CommitmentStatus::Proposed => "proposed",
        CommitmentStatus::Active => "active",
        CommitmentStatus::Suspended => "suspended",
        CommitmentStatus::Fulfilled => "fulfilled",
        CommitmentStatus::Abandoned => "abandoned",
    }
}

fn parse_commitment_status(raw: String) -> CommitmentStatus {
    match raw.as_str() {
        "active" => CommitmentStatus::Active,
        "suspended" => CommitmentStatus::Suspended,
        "fulfilled" => CommitmentStatus::Fulfilled,
        "abandoned" => CommitmentStatus::Abandoned,
        _ => CommitmentStatus::Proposed,
    }
}

fn intention_status_str(status: &IntentionStatus) -> &'static str {
    match status {
        IntentionStatus::Planned => "planned",
        IntentionStatus::Active => "active",
        IntentionStatus::Blocked => "blocked",
        IntentionStatus::Completed => "completed",
        IntentionStatus::Failed => "failed",
        IntentionStatus::Cancelled => "cancelled",
    }
}

fn parse_intention_status(raw: String) -> IntentionStatus {
    match raw.as_str() {
        "active" => IntentionStatus::Active,
        "blocked" => IntentionStatus::Blocked,
        "completed" => IntentionStatus::Completed,
        "failed" => IntentionStatus::Failed,
        "cancelled" => IntentionStatus::Cancelled,
        _ => IntentionStatus::Planned,
    }
}

fn revision_decision_str(decision: &IntentionRevisionDecision) -> &'static str {
    match decision {
        IntentionRevisionDecision::Replan => "replan",
        IntentionRevisionDecision::Suspend => "suspend",
        IntentionRevisionDecision::Abandon => "abandon",
    }
}

fn parse_revision_decision(raw: String) -> IntentionRevisionDecision {
    match raw.as_str() {
        "suspend" => IntentionRevisionDecision::Suspend,
        "abandon" => IntentionRevisionDecision::Abandon,
        _ => IntentionRevisionDecision::Replan,
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
