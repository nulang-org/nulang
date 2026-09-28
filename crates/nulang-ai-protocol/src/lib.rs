//! NLAP v1 protocol helpers.

pub use nulang_ai_core::{
    AgentRun, AgentSession, AgentRef, AnchoredEdit, ArtifactKind, ArtifactRef, AttemptStatus,
    ChangeSet, ChangeSetStatus, Checkpoint, ConversationMessage, ConversationState, Evidence,
    EvidenceKind, EvidenceStatus, Goal, GoalGraph, ManagerKind, RunStatus, SessionStatus,
    SwarmEvent, SwarmEventEnvelope, Task, TaskAttempt, TaskStatus, ToolCall, ToolCallStatus, Usage,
    WorkspaceLease, WorkspaceLeaseStatus, NLAP_VERSION,
};

pub fn format_event_line(envelope: &SwarmEventEnvelope) -> Result<String, serde_json::Error> {
    serde_json::to_string(envelope)
}
