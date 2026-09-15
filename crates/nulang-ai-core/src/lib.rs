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

/// Runtime lifecycle for a mission.
///
/// A mission is a higher-level Cloud SDK concept built on top of a durable
/// [`Goal`]. It deliberately does not add a new Nulang language keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionStatus {
    Created,
    Running,
    Blocked,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

/// Hard resource ceilings for one autonomous mission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MissionBudget {
    /// Maximum total provider/tool spend for the mission.
    pub max_cost_usd: f64,
    /// Maximum number of tasks the planner may materialize.
    pub max_tasks: u32,
    /// Maximum number of tasks that may execute concurrently.
    pub max_parallelism: u16,
    /// Wall-clock budget for the mission.
    #[serde(rename = "max_duration_secs", with = "duration_secs")]
    pub max_duration: Duration,
    /// Optional aggregate model-token ceiling. `None` leaves token accounting
    /// to the provider budget while cost/task/time limits still apply.
    pub max_tokens: Option<u64>,
}

impl Default for MissionBudget {
    fn default() -> Self {
        Self {
            max_cost_usd: 25.0,
            max_tasks: 64,
            max_parallelism: 8,
            max_duration: Duration::from_secs(60 * 60),
            max_tokens: None,
        }
    }
}

/// Human-approval policy for side-effecting mission steps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// The mission may execute without human approval.
    Never,
    /// Every task requires approval before execution.
    Always,
    /// Only tasks that request one of the listed capabilities require approval.
    CapabilityGated { capabilities: Vec<String> },
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self::CapabilityGated {
            capabilities: vec![
                "deploy.production".into(),
                "secrets.write".into(),
                "billing.write".into(),
            ],
        }
    }
}

impl ApprovalPolicy {
    pub fn requires_approval(&self, required_capabilities: &[String]) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::CapabilityGated { capabilities } => required_capabilities
                .iter()
                .any(|required| capabilities.iter().any(|gated| gated == required)),
        }
    }
}

/// Verification and repair policy applied before a mission may complete.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationPolicy {
    pub required: bool,
    pub min_successful_checks: u16,
    pub max_repair_attempts: u16,
}

impl Default for VerificationPolicy {
    fn default() -> Self {
        Self {
            required: true,
            min_successful_checks: 1,
            max_repair_attempts: 2,
        }
    }
}

/// Typed execution contract for long-running autonomous work.
///
/// `MissionSpec` is intentionally a library/domain type rather than language
/// syntax. Nulang programs can represent missions with ordinary durable
/// entities/actors while Nulang Cloud interprets this policy at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MissionSpec {
    pub goal: Goal,
    pub budget: MissionBudget,
    pub approval: ApprovalPolicy,
    pub verification: VerificationPolicy,
    pub required_capabilities: Vec<String>,
}

impl MissionSpec {
    pub fn from_goal(goal: Goal) -> Self {
        let max_cost_usd = goal.budget_usd.max(0.0);
        Self {
            goal,
            budget: MissionBudget {
                max_cost_usd,
                ..MissionBudget::default()
            },
            approval: ApprovalPolicy::default(),
            verification: VerificationPolicy::default(),
            required_capabilities: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), MissionValidationError> {
        if !self.budget.max_cost_usd.is_finite() || self.budget.max_cost_usd < 0.0 {
            return Err(MissionValidationError::InvalidCostBudget);
        }
        if !self.goal.budget_usd.is_finite()
            || self.goal.budget_usd < 0.0
            || self.goal.budget_usd > self.budget.max_cost_usd
        {
            return Err(MissionValidationError::GoalBudgetExceedsMissionBudget);
        }
        if self.budget.max_tasks == 0 {
            return Err(MissionValidationError::ZeroTaskBudget);
        }
        if self.budget.max_parallelism == 0
            || u32::from(self.budget.max_parallelism) > self.budget.max_tasks
        {
            return Err(MissionValidationError::InvalidParallelism);
        }
        if self.budget.max_duration.is_zero() {
            return Err(MissionValidationError::ZeroDurationBudget);
        }
        if self.verification.required && self.verification.min_successful_checks == 0 {
            return Err(MissionValidationError::InvalidVerificationPolicy);
        }
        if self
            .required_capabilities
            .iter()
            .any(|capability| capability.trim().is_empty())
        {
            return Err(MissionValidationError::EmptyCapability);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionValidationError {
    InvalidCostBudget,
    GoalBudgetExceedsMissionBudget,
    ZeroTaskBudget,
    InvalidParallelism,
    ZeroDurationBudget,
    InvalidVerificationPolicy,
    EmptyCapability,
}

impl std::fmt::Display for MissionValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidCostBudget => "mission max_cost_usd must be finite and non-negative",
            Self::GoalBudgetExceedsMissionBudget => {
                "goal budget must be finite, non-negative, and within the mission cost budget"
            }
            Self::ZeroTaskBudget => "mission max_tasks must be greater than zero",
            Self::InvalidParallelism => {
                "mission max_parallelism must be greater than zero and no larger than max_tasks"
            }
            Self::ZeroDurationBudget => "mission max_duration must be greater than zero",
            Self::InvalidVerificationPolicy => {
                "required verification needs at least one successful check"
            }
            Self::EmptyCapability => "mission capabilities cannot contain empty names",
        };
        f.write_str(message)
    }
}

impl std::error::Error for MissionValidationError {}

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

    #[test]
    fn mission_spec_roundtrip_and_validation() {
        let goal = Goal::new("demo", "Ship a production feature", 10.0);
        let mut mission = MissionSpec::from_goal(goal);
        mission.required_capabilities = vec!["code".into(), "deploy.production".into()];

        mission.validate().unwrap();
        assert!(mission
            .approval
            .requires_approval(&mission.required_capabilities));

        let json = serde_json::to_string(&mission).unwrap();
        let back: MissionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(mission, back);
    }

    #[test]
    fn mission_rejects_parallelism_above_task_budget() {
        let goal = Goal::new("demo", "Do work", 5.0);
        let mut mission = MissionSpec::from_goal(goal);
        mission.budget.max_tasks = 2;
        mission.budget.max_parallelism = 3;

        assert_eq!(
            mission.validate(),
            Err(MissionValidationError::InvalidParallelism)
        );
    }
}