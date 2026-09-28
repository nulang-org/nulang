use chrono::{Duration as ChronoDuration, Utc};
use nulang_ai_core::{
    AgentSession, ArtifactKind, ArtifactRef, Evidence, EvidenceKind, EvidenceStatus, TaskAttempt,
    WorkspaceLease,
};
use nulang_ai_local::SqliteStore;
use uuid::Uuid;

#[test]
fn execution_state_roundtrips_through_local_sqlite_store() {
    let tmp = std::env::temp_dir().join(format!("nulang-ai-store-test-{}", Uuid::new_v4()));
    let store = SqliteStore::open(&tmp).unwrap();

    let session = AgentSession::new("project-a", "agent-7", "smart");
    store.upsert_session(&session).unwrap();

    let task_id = Uuid::new_v4();
    let mut attempt = TaskAttempt::new(
        task_id,
        1,
        session.id,
        "agent-7",
        "workspace-42",
        "rev-base",
    );

    let lease = WorkspaceLease::new(
        "project-a",
        "workspace-42",
        attempt.id,
        "rev-base",
        3,
        Some(Utc::now() + ChronoDuration::minutes(10)),
    );
    attempt.workspace_lease_id = Some(lease.id);

    store.upsert_workspace_lease(&lease).unwrap();
    store.upsert_task_attempt(&attempt).unwrap();

    let artifact = ArtifactRef::new(
        attempt.id,
        ArtifactKind::TestReport,
        "artifact://test-report/123",
        "application/json",
        "blake3:report",
    );
    store.upsert_artifact(&artifact).unwrap();

    let evidence = Evidence::new(
        attempt.id,
        EvidenceKind::Test,
        EvidenceStatus::Passed,
        "focused execution-contract tests passed",
        Some(artifact.id),
    );
    store.upsert_evidence(&evidence).unwrap();

    let loaded_session = store.get_session(session.id).unwrap();
    let loaded_attempt = store.get_task_attempt(attempt.id).unwrap();
    let loaded_lease = store.get_workspace_lease(lease.id).unwrap();
    let artifacts = store.list_artifacts_for_attempt(attempt.id).unwrap();
    let evidence_rows = store.list_evidence_for_attempt(attempt.id).unwrap();

    assert_eq!(loaded_session.agent_id, "agent-7");
    assert_eq!(loaded_attempt.task_id, task_id);
    assert_eq!(loaded_attempt.workspace_lease_id, Some(lease.id));
    assert_eq!(loaded_lease.lease_epoch, 3);
    assert_eq!(artifacts, vec![artifact]);
    assert_eq!(evidence_rows, vec![evidence]);

    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn task_attempt_listing_is_scoped_to_one_task_and_ordered_by_attempt_number() {
    let tmp = std::env::temp_dir().join(format!("nulang-ai-store-test-{}", Uuid::new_v4()));
    let store = SqliteStore::open(&tmp).unwrap();

    let session = AgentSession::new("project-a", "agent-7", "smart");
    store.upsert_session(&session).unwrap();

    let task_id = Uuid::new_v4();
    let other_task_id = Uuid::new_v4();
    let attempt_two = TaskAttempt::new(task_id, 2, session.id, "agent-7", "workspace-2", "rev-2");
    let attempt_one = TaskAttempt::new(task_id, 1, session.id, "agent-7", "workspace-1", "rev-1");
    let other = TaskAttempt::new(
        other_task_id,
        1,
        session.id,
        "agent-7",
        "workspace-other",
        "rev-other",
    );

    store.upsert_task_attempt(&attempt_two).unwrap();
    store.upsert_task_attempt(&attempt_one).unwrap();
    store.upsert_task_attempt(&other).unwrap();

    let attempts = store.list_task_attempts_for_task(task_id).unwrap();

    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].attempt_number, 1);
    assert_eq!(attempts[1].attempt_number, 2);
    assert!(attempts.iter().all(|attempt| attempt.task_id == task_id));

    let _ = std::fs::remove_dir_all(tmp);
}
