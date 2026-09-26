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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Budget {
    pub max_cost_usd: Option<f64>,
    pub max_duration_secs: Option<u64>,
    pub max_model_calls: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_parallel_agents: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Artifact {
    pub id: Uuid,
    pub goal_id: Uuid,
    pub task_id: Option<Uuid>,
    pub artifact_type: String,
    pub uri: String,
    pub mime_type: Option<String>,
    pub digest: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl Artifact {
    pub fn new(
        goal_id: Uuid,
        task_id: Option<Uuid>,
        artifact_type: impl Into<String>,
        uri: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            goal_id,
            task_id,
            artifact_type: artifact_type.into(),
            uri: uri.into(),
            mime_type: None,
            digest: None,
            metadata: serde_json::json!({}),
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub id: Uuid,
    pub goal_id: Uuid,
    pub task_id: Option<Uuid>,
    pub artifact_id: Option<Uuid>,
    pub evidence_type: String,
    pub summary: String,
    pub passed: Option<bool>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl Evidence {
    pub fn new(
        goal_id: Uuid,
        task_id: Option<Uuid>,
        evidence_type: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            goal_id,
            task_id,
            artifact_id: None,
            evidence_type: evidence_type.into(),
            summary: summary.into(),
            passed: None,
            metadata: serde_json::json!({}),
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
    fn artifact_and_evidence_roundtrip_json() {
        let goal_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();
        let artifact = Artifact::new(goal_id, Some(task_id), "patch", "file:///tmp/change.diff");
        let evidence = Evidence::new(goal_id, Some(task_id), "test", "unit tests passed");
        let budget = Budget {
            max_cost_usd: Some(10.0),
            max_duration_secs: Some(1800),
            max_model_calls: Some(20),
            max_tool_calls: Some(100),
            max_parallel_agents: Some(8),
        };

        let artifact_json = serde_json::to_string(&artifact).unwrap();
        let evidence_json = serde_json::to_string(&evidence).unwrap();
        let budget_json = serde_json::to_string(&budget).unwrap();

        assert_eq!(artifact, serde_json::from_str(&artifact_json).unwrap());
        assert_eq!(evidence, serde_json::from_str(&evidence_json).unwrap());
        assert_eq!(budget, serde_json::from_str(&budget_json).unwrap());
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
