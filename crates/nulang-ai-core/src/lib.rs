//! Core domain types for the NuLang Agent Runtime (NLAP v1).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}

pub const NLAP_VERSION: &str = "1.0.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Created,
    Running,
    Blocked,
    Verifying,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Ready,
    Assigned,
    Running,
    Blocked,
    Verifying,
    Failed,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerKind {
    Engineering,
    Research,
    Operations,
    Data,
    Voice,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Goal {
    pub id: Uuid,
    pub project_id: String,
    pub conversation_id: Option<Uuid>,
    pub intent: String,
    pub desired_state: serde_json::Value,
    pub constraints: serde_json::Value,
    pub success_criteria: Vec<String>,
    pub budget_usd: f64,
    pub deadline: Option<DateTime<Utc>>,
    pub status: GoalStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Goal {
    pub fn new(project_id: impl Into<String>, intent: impl Into<String>, budget_usd: f64) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            project_id: project_id.into(),
            conversation_id: None,
            intent: intent.into(),
            desired_state: serde_json::json!({}),
            constraints: serde_json::json!({}),
            success_criteria: Vec::new(),
            budget_usd,
            deadline: None,
            status: GoalStatus::Created,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: Uuid,
    pub goal_id: Uuid,
    pub parent_task_id: Option<Uuid>,
    pub manager: ManagerKind,
    pub description: String,
    pub dependencies: Vec<Uuid>,
    pub required_capabilities: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub budget_usd: f64,
    #[serde(rename = "timeout_secs", with = "duration_secs")]
    pub timeout: Duration,
    pub status: TaskStatus,
    pub assigned_agent_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn new(goal_id: Uuid, description: impl Into<String>, manager: ManagerKind) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            goal_id,
            parent_task_id: None,
            manager,
            description: description.into(),
            dependencies: Vec::new(),
            required_capabilities: Vec::new(),
            acceptance_criteria: Vec::new(),
            budget_usd: 0.0,
            timeout: Duration::from_secs(3600),
            status: TaskStatus::Created,
            assigned_agent_id: None,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentRef {
    pub id: String,
    pub name: String,
    pub manager_id: Option<String>,
    pub parent_id: Option<String>,
    pub capabilities: Vec<String>,
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Created,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Created,
    Preparing,
    Running,
    Blocked,
    Verifying,
    Completed,
    Failed,
    Cancelled,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceLeaseStatus {
    Active,
    Released,
    Expired,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSetStatus {
    Proposed,
    Applied,
    Rejected,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Prepared,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Diff,
    TestReport,
    Diagnostics,
    BuildLog,
    Screenshot,
    BrowserRecording,
    Benchmark,
    SecurityReport,
    Review,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Test,
    Diagnostics,
    Build,
    Lint,
    Typecheck,
    Browser,
    Benchmark,
    Security,
    Review,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Pending,
    Passed,
    Failed,
    Inconclusive,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentSession {
    pub id: Uuid,
    pub project_id: String,
    pub conversation_id: Option<Uuid>,
    pub parent_session_id: Option<Uuid>,
    pub agent_id: String,
    pub model_profile: String,
    pub status: SessionStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl AgentSession {
    pub fn new(
        project_id: impl Into<String>,
        agent_id: impl Into<String>,
        model_profile: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            project_id: project_id.into(),
            conversation_id: None,
            parent_session_id: None,
            agent_id: agent_id.into(),
            model_profile: model_profile.into(),
            status: SessionStatus::Created,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentRun {
    pub id: Uuid,
    pub session_id: Uuid,
    pub task_attempt_id: Option<Uuid>,
    pub sequence: u64,
    pub status: RunStatus,
    pub usage: Usage,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl AgentRun {
    pub fn new(session_id: Uuid, task_attempt_id: Option<Uuid>, sequence: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            session_id,
            task_attempt_id,
            sequence,
            status: RunStatus::Created,
            usage: Usage::default(),
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskAttempt {
    pub id: Uuid,
    pub task_id: Uuid,
    pub attempt_number: u32,
    pub session_id: Uuid,
    pub agent_id: String,
    pub workspace_id: String,
    pub workspace_lease_id: Option<Uuid>,
    pub base_revision: String,
    pub status: AttemptStatus,
    pub change_set_id: Option<Uuid>,
    pub evidence_ids: Vec<Uuid>,
    pub failure: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl TaskAttempt {
    pub fn new(
        task_id: Uuid,
        attempt_number: u32,
        session_id: Uuid,
        agent_id: impl Into<String>,
        workspace_id: impl Into<String>,
        base_revision: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            task_id,
            attempt_number,
            session_id,
            agent_id: agent_id.into(),
            workspace_id: workspace_id.into(),
            workspace_lease_id: None,
            base_revision: base_revision.into(),
            status: AttemptStatus::Created,
            change_set_id: None,
            evidence_ids: Vec::new(),
            failure: None,
            created_at: now,
            started_at: None,
            heartbeat_at: None,
            completed_at: None,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceLease {
    pub id: Uuid,
    pub project_id: String,
    pub workspace_id: String,
    pub holder_attempt_id: Uuid,
    pub base_revision: String,
    pub lease_epoch: u64,
    pub status: WorkspaceLeaseStatus,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub released_at: Option<DateTime<Utc>>,
}

impl WorkspaceLease {
    pub fn new(
        project_id: impl Into<String>,
        workspace_id: impl Into<String>,
        holder_attempt_id: Uuid,
        base_revision: impl Into<String>,
        lease_epoch: u64,
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            project_id: project_id.into(),
            workspace_id: workspace_id.into(),
            holder_attempt_id,
            base_revision: base_revision.into(),
            lease_epoch,
            status: WorkspaceLeaseStatus::Active,
            acquired_at: Utc::now(),
            expires_at,
            released_at: None,
        }
    }

    pub fn is_active_at(&self, at: DateTime<Utc>) -> bool {
        self.status == WorkspaceLeaseStatus::Active
            && self.expires_at.map(|expires| at < expires).unwrap_or(true)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnchoredEdit {
    pub file_uri: String,
    pub observed_revision: String,
    pub observed_hash: String,
    pub before_anchor: Option<String>,
    pub after_anchor: Option<String>,
    pub replacement: String,
}

impl AnchoredEdit {
    pub fn matches_observation(&self, revision: &str, hash: &str) -> bool {
        self.observed_revision == revision && self.observed_hash == hash
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChangeSet {
    pub id: Uuid,
    pub attempt_id: Uuid,
    pub workspace_id: String,
    pub base_revision: String,
    pub resulting_revision: Option<String>,
    pub edits: Vec<AnchoredEdit>,
    pub status: ChangeSetStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ChangeSet {
    pub fn new(
        attempt_id: Uuid,
        workspace_id: impl Into<String>,
        base_revision: impl Into<String>,
        edits: Vec<AnchoredEdit>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            attempt_id,
            workspace_id: workspace_id.into(),
            base_revision: base_revision.into(),
            resulting_revision: None,
            edits,
            status: ChangeSetStatus::Proposed,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactRef {
    pub id: Uuid,
    pub attempt_id: Uuid,
    pub kind: ArtifactKind,
    pub uri: String,
    pub media_type: String,
    pub digest: String,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl ArtifactRef {
    pub fn new(
        attempt_id: Uuid,
        kind: ArtifactKind,
        uri: impl Into<String>,
        media_type: impl Into<String>,
        digest: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            attempt_id,
            kind,
            uri: uri.into(),
            media_type: media_type.into(),
            digest: digest.into(),
            label: None,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub id: Uuid,
    pub attempt_id: Uuid,
    pub kind: EvidenceKind,
    pub status: EvidenceStatus,
    pub summary: String,
    pub artifact_id: Option<Uuid>,
    pub criterion: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Evidence {
    pub fn new(
        attempt_id: Uuid,
        kind: EvidenceKind,
        status: EvidenceStatus,
        summary: impl Into<String>,
        artifact_id: Option<Uuid>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            attempt_id,
            kind,
            status,
            summary: summary.into(),
            artifact_id,
            criterion: None,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: Uuid,
    pub attempt_id: Uuid,
    pub tool_name: String,
    pub arguments: serde_json::Value,
    pub idempotency_key: String,
    pub status: ToolCallStatus,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub prepared_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl ToolCall {
    pub fn new(
        attempt_id: Uuid,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
        idempotency_key: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            attempt_id,
            tool_name: tool_name.into(),
            arguments,
            idempotency_key: idempotency_key.into(),
            status: ToolCallStatus::Prepared,
            result: None,
            error: None,
            prepared_at: Utc::now(),
            started_at: None,
            completed_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checkpoint {
    pub id: Uuid,
    pub session_id: Uuid,
    pub task_attempt_id: Option<Uuid>,
    pub sequence: u64,
    pub state_uri: Option<String>,
    pub state_digest: String,
    pub created_at: DateTime<Utc>,
}

impl Checkpoint {
    pub fn new(
        session_id: Uuid,
        task_attempt_id: Option<Uuid>,
        sequence: u64,
        state_digest: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            session_id,
            task_attempt_id,
            sequence,
            state_uri: None,
            state_digest: state_digest.into(),
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationState {
    pub id: Uuid,
    pub project_id: String,
    pub director_id: Option<String>,
    pub active_goal_id: Option<Uuid>,
    pub messages: Vec<ConversationMessage>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SwarmEvent {
    GoalCreated {
        goal_id: Uuid,
        conversation_id: Option<Uuid>,
    },
    GoalCompleted {
        goal_id: Uuid,
    },
    TaskCreated {
        task_id: Uuid,
        goal_id: Uuid,
    },
    TaskStarted {
        task_id: Uuid,
        agent_id: String,
    },
    TaskProgress {
        task_id: Uuid,
        agent_id: String,
        progress: f32,
        message: String,
    },
    TaskCompleted {
        task_id: Uuid,
        agent_id: String,
    },
    DirectorThinking {
        conversation_id: Uuid,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SwarmEventEnvelope {
    pub version: String,
    pub tenant_id: Option<String>,
    pub conversation_id: Option<Uuid>,
    pub ts: DateTime<Utc>,
    pub event: SwarmEvent,
}

impl SwarmEventEnvelope {
    pub fn new(event: SwarmEvent, conversation_id: Option<Uuid>) -> Self {
        Self {
            version: NLAP_VERSION.to_string(),
            tenant_id: None,
            conversation_id,
            ts: Utc::now(),
            event,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GoalGraph {
    pub goal: Goal,
    pub tasks: Vec<Task>,
    pub agents: Vec<AgentRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_roundtrip_json() {
        let goal = Goal::new("demo", "Optimize API", 25.0);
        let json = serde_json::to_string(&goal).unwrap();
        let back: Goal = serde_json::from_str(&json).unwrap();
        assert_eq!(goal.id, back.id);
    }

    #[test]
    fn swarm_event_tagged_json() {
        let goal_id = Uuid::new_v4();
        let ev = SwarmEventEnvelope::new(
            SwarmEvent::GoalCreated {
                goal_id,
                conversation_id: None,
            },
            None,
        );
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("goal_created"));
    }
}
