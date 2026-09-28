use chrono::{Duration as ChronoDuration, Utc};
use nulang_ai_core::{
    AgentRun, AgentSession, AnchoredEdit, ArtifactKind, ArtifactRef, AttemptStatus, ChangeSet,
    ChangeSetStatus, Evidence, EvidenceKind, EvidenceStatus, RunStatus, SessionStatus, TaskAttempt,
    ToolCall, ToolCallStatus, Usage, WorkspaceLease, WorkspaceLeaseStatus,
};
use uuid::Uuid;

#[test]
fn task_attempt_roundtrips_with_workspace_change_and_evidence_references() {
    let task_id = Uuid::new_v4();
    let session = AgentSession::new("project-a", "agent-7", "smart");
    let mut attempt = TaskAttempt::new(
        task_id,
        2,
        session.id,
        "agent-7",
        "workspace-42",
        "rev-base",
    );
    attempt.status = AttemptStatus::Verifying;

    let json = serde_json::to_string(&attempt).unwrap();
    let decoded: TaskAttempt = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.task_id, task_id);
    assert_eq!(decoded.attempt_number, 2);
    assert_eq!(decoded.session_id, session.id);
    assert_eq!(decoded.workspace_id, "workspace-42");
    assert_eq!(decoded.base_revision, "rev-base");
    assert_eq!(decoded.status, AttemptStatus::Verifying);
}

#[test]
fn workspace_lease_tracks_epoch_and_expiration_without_conflating_revision_identity() {
    let now = Utc::now();
    let lease = WorkspaceLease::new(
        "project-a",
        "workspace-42",
        Uuid::new_v4(),
        "rev-base",
        7,
        Some(now + ChronoDuration::minutes(5)),
    );

    assert_eq!(lease.status, WorkspaceLeaseStatus::Active);
    assert_eq!(lease.lease_epoch, 7);
    assert_eq!(lease.base_revision, "rev-base");
    assert!(lease.is_active_at(now));
    assert!(!lease.is_active_at(now + ChronoDuration::minutes(6)));
}

#[test]
fn anchored_edit_records_the_exact_content_observed_by_the_agent() {
    let edit = AnchoredEdit {
        file_uri: "file://workspace/src/auth.rs".into(),
        observed_revision: "rev-17".into(),
        observed_hash: "blake3:abc".into(),
        before_anchor: Some("fn authenticate(".into()),
        after_anchor: Some("fn refresh_session(".into()),
        replacement: "fn authenticate() { /* replacement */ }\n".into(),
    };

    assert!(edit.matches_observation("rev-17", "blake3:abc"));
    assert!(!edit.matches_observation("rev-18", "blake3:abc"));
    assert!(!edit.matches_observation("rev-17", "blake3:def"));
}

#[test]
fn execution_artifacts_are_reference_based_and_preserve_usage_and_tool_state() {
    let attempt_id = Uuid::new_v4();
    let artifact = ArtifactRef::new(
        attempt_id,
        ArtifactKind::TestReport,
        "artifact://test-report/123",
        "application/json",
        "blake3:report",
    );
    let evidence = Evidence::new(
        attempt_id,
        EvidenceKind::Test,
        EvidenceStatus::Passed,
        "cargo nextest run passed",
        Some(artifact.id),
    );
    let tool = ToolCall::new(
        attempt_id,
        "workspace.exec",
        serde_json::json!({"argv": ["cargo", "nextest", "run"]}),
        "attempt-1:tool-1",
    );

    assert_eq!(tool.status, ToolCallStatus::Prepared);
    assert_eq!(evidence.artifact_id, Some(artifact.id));
    assert_eq!(artifact.kind, ArtifactKind::TestReport);
}

#[test]
fn sessions_runs_changesets_and_usage_have_stable_default_states() {
    let session = AgentSession::new("project-a", "agent-7", "smart");
    let run = AgentRun::new(session.id, None, 1);
    let change = ChangeSet::new(
        Uuid::new_v4(),
        "workspace-42",
        "rev-base",
        vec![],
    );
    let usage = Usage::default();

    assert_eq!(session.status, SessionStatus::Created);
    assert_eq!(run.status, RunStatus::Created);
    assert_eq!(change.status, ChangeSetStatus::Proposed);
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.output_tokens, 0);
    assert_eq!(usage.cost_usd, 0.0);
}
