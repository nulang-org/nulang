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

/// Lifecycle of an agent's explicit commitment to pursue a goal.
///
/// A goal describes desired state. A commitment records that an agent has
/// accepted responsibility for trying to reach it. This distinction keeps
/// BDI-style semantics in the agent library rather than adding language
/// keywords to Nulang's frozen core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitmentStatus {
    Proposed,
    Active,
    Suspended,
    Fulfilled,
    Abandoned,
}

/// Lifecycle of the concrete plan currently selected to satisfy a commitment.
///
/// An intention is intentionally narrower than a goal: it is an executable
/// plan, represented today as an ordered set of task ids. Future planners can
/// replace or branch intentions without changing the underlying goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentionStatus {
    Planned,
    Active,
    Blocked,
    Completed,
    Failed,
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

/// An explicit promise by an agent to pursue a goal under the goal's
/// constraints and success criteria.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Commitment {
    pub id: Uuid,
    pub goal_id: Uuid,
    pub owner_agent_id: String,
    pub rationale: String,
    pub success_criteria: Vec<String>,
    pub status: CommitmentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Commitment {
    pub fn new(
        goal_id: Uuid,
        owner_agent_id: impl Into<String>,
        rationale: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            goal_id,
            owner_agent_id: owner_agent_id.into(),
            rationale: rationale.into(),
            success_criteria: Vec::new(),
            status: CommitmentStatus::Proposed,
            created_at: now,
            updated_at: now,
        }
    }
}

/// The currently selected executable plan for a commitment.
///
/// `planned_task_ids` is ordered. Keeping task identity separate from the
/// intention lets the planner revise an intention while preserving durable
/// task history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Intention {
    pub id: Uuid,
    pub goal_id: Uuid,
    pub commitment_id: Uuid,
    pub owner_agent_id: String,
    pub description: String,
    pub planned_task_ids: Vec<Uuid>,
    pub status: IntentionStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Intention {
    pub fn new(
        goal_id: Uuid,
        commitment_id: Uuid,
        owner_agent_id: impl Into<String>,
        description: impl Into<String>,
        planned_task_ids: Vec<Uuid>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            goal_id,
            commitment_id,
            owner_agent_id: owner_agent_id.into(),
            description: description.into(),
            planned_task_ids,
            status: IntentionStatus::Planned,
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
    CommitmentActivated {
        commitment_id: Uuid,
        goal_id: Uuid,
        owner_agent_id: String,
    },
    CommitmentFulfilled {
        commitment_id: Uuid,
        goal_id: Uuid,
    },
    IntentionActivated {
        intention_id: Uuid,
        commitment_id: Uuid,
        goal_id: Uuid,
        owner_agent_id: String,
    },
    IntentionCompleted {
        intention_id: Uuid,
        commitment_id: Uuid,
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
    #[serde(default)]
    pub commitments: Vec<Commitment>,
    #[serde(default)]
    pub intentions: Vec<Intention>,
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
    fn commitment_and_intention_roundtrip_json() {
        let goal = Goal::new("demo", "Ship feature", 10.0);
        let mut commitment = Commitment::new(
            goal.id,
            "director-local",
            "User goal accepted for execution",
        );
        commitment.status = CommitmentStatus::Active;

        let task = Task::new(goal.id, "Implement feature", ManagerKind::Engineering);
        let mut intention = Intention::new(
            goal.id,
            commitment.id,
            "manager-engineering",
            "Execute the engineering plan",
            vec![task.id],
        );
        intention.status = IntentionStatus::Active;

        let commitment_json = serde_json::to_string(&commitment).unwrap();
        let intention_json = serde_json::to_string(&intention).unwrap();
        assert_eq!(
            commitment,
            serde_json::from_str::<Commitment>(&commitment_json).unwrap()
        );
        assert_eq!(
            intention,
            serde_json::from_str::<Intention>(&intention_json).unwrap()
        );
    }

    #[test]
    fn goal_graph_accepts_legacy_json_without_bdi_fields() {
        let goal = Goal::new("demo", "Optimize API", 25.0);
        let json = serde_json::json!({
            "goal": goal,
            "tasks": [],
            "agents": []
        });
        let graph: GoalGraph = serde_json::from_value(json).unwrap();
        assert!(graph.commitments.is_empty());
        assert!(graph.intentions.is_empty());
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
