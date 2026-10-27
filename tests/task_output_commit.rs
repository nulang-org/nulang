//! Semantic contract for single-winner task output publication.
//! The backing store must commit each updated state with an atomic CAS.
use nulang::task_output_commit::{CommitDecision, CommitError, TaskOutputState};

#[test]
fn staged_output_is_not_published_until_committed() {
    let mut task = TaskOutputState::new("task-7").unwrap();
    let worker = task.assign("worker-a").unwrap();
    let digest = [7; 32];

    task.stage(&worker, "staging/task-7/attempt-a", digest)
        .unwrap();
    assert!(task.committed().is_none());
    assert_eq!(task.commit(&worker, digest).unwrap(), CommitDecision::Won);
    let result = task.committed().unwrap();
    assert_eq!(result.artifact_key, "staging/task-7/attempt-a");
    assert_eq!(result.digest, digest);
    assert_eq!(
        task.commit(&worker, digest).unwrap(),
        CommitDecision::AlreadyWon
    );
}

#[test]
fn newer_attempt_fences_stale_staging_and_commit() {
    let mut task = TaskOutputState::new("task-7").unwrap();
    let old = task.assign("worker-a").unwrap();
    task.stage(&old, "staging/old", [1; 32]).unwrap();
    let new = task.assign("worker-b").unwrap();
    assert!(new.epoch > old.epoch);
    assert!(matches!(
        task.stage(&old, "staging/old", [1; 32]),
        Err(CommitError::Fenced)
    ));
    assert!(matches!(
        task.commit(&old, [1; 32]),
        Err(CommitError::Fenced)
    ));
    assert!(task.committed().is_none());
    task.stage(&new, "staging/new", [2; 32]).unwrap();
    assert_eq!(task.commit(&new, [2; 32]).unwrap(), CommitDecision::Won);
    assert_eq!(task.committed().unwrap().artifact_key, "staging/new");
}

#[test]
fn conflicting_stages_and_losing_results_are_rejected() {
    let mut task = TaskOutputState::new("task-7").unwrap();
    let owner = task.assign("worker-a").unwrap();
    task.stage(&owner, "staging/first", [3; 32]).unwrap();
    task.stage(&owner, "staging/first", [3; 32]).unwrap();
    assert!(matches!(
        task.stage(&owner, "staging/different", [3; 32]),
        Err(CommitError::StageConflict)
    ));
    assert!(matches!(
        task.commit(&owner, [4; 32]),
        Err(CommitError::StageConflict)
    ));
    task.commit(&owner, [3; 32]).unwrap();
    assert!(matches!(
        task.assign("worker-b"),
        Err(CommitError::AlreadyCommitted)
    ));
    assert!(matches!(
        task.commit(&owner, [4; 32]),
        Err(CommitError::AlreadyCommitted)
    ));
}

#[test]
fn restored_checkpoint_preserves_fencing_and_the_winner() {
    let mut task = TaskOutputState::new("task-7").unwrap();
    let old = task.assign("worker-a").unwrap();
    task.stage(&old, "staging/old", [1; 32]).unwrap();
    let new = task.assign("worker-b").unwrap();
    task.stage(&new, "staging/new", [2; 32]).unwrap();

    let mut restarted = TaskOutputState::restore(&task.checkpoint().unwrap()).unwrap();
    assert!(matches!(
        restarted.commit(&old, [1; 32]),
        Err(CommitError::Fenced)
    ));
    restarted.commit(&new, [2; 32]).unwrap();
    let mut terminal = TaskOutputState::restore(&restarted.checkpoint().unwrap()).unwrap();
    assert_eq!(terminal.committed().unwrap().artifact_key, "staging/new");
    assert_eq!(
        terminal.commit(&new, [2; 32]).unwrap(),
        CommitDecision::AlreadyWon
    );
}

#[test]
fn corrupt_or_forward_versioned_checkpoints_fail_closed() {
    let mut task = TaskOutputState::new("task-7").unwrap();
    let lease = task.assign("worker-a").unwrap();
    task.stage(&lease, "staging/one", [5; 32]).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&task.checkpoint().unwrap()).unwrap();
    value["version"] = serde_json::json!(999);
    assert!(TaskOutputState::restore(&serde_json::to_vec(&value).unwrap()).is_err());
    value["version"] = serde_json::json!(1);
    value["state"]["epoch"] = serde_json::json!(0);
    assert!(TaskOutputState::restore(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn invalid_identity_and_output_reference_do_not_mutate_state() {
    assert!(TaskOutputState::new(" ").is_err());
    let mut task = TaskOutputState::new("task-7").unwrap();
    assert!(task.assign(" ").is_err());
    let owner = task.assign("worker-a").unwrap();
    assert!(task.stage(&owner, "", [4; 32]).is_err());
    assert!(task.committed().is_none());
    assert!(matches!(
        task.commit(&owner, [4; 32]),
        Err(CommitError::NotStaged)
    ));
}

#[test]
fn an_attempt_token_from_another_task_is_not_authorized() {
    let mut task_a = TaskOutputState::new("task-a").unwrap();
    let mut task_b = TaskOutputState::new("task-b").unwrap();
    let token_a = task_a.assign("same-worker").unwrap();
    let token_b = task_b.assign("same-worker").unwrap();
    assert_ne!(token_a, token_b);
    assert!(matches!(
        task_b.stage(&token_a, "staging/task-a", [9; 32]),
        Err(CommitError::Fenced)
    ));
    task_b.stage(&token_b, "staging/task-b", [8; 32]).unwrap();
    assert_eq!(
        task_b.commit(&token_b, [8; 32]).unwrap(),
        CommitDecision::Won
    );
}

#[test]
fn repeated_assignment_for_same_attempt_is_idempotent() {
    let mut task = TaskOutputState::new("task-7").unwrap();
    let owner = task.assign("worker-a").unwrap();
    task.stage(&owner, "immutable/object", [7; 32]).unwrap();
    let checkpoint = task.checkpoint().unwrap();
    assert_eq!(task.assign("worker-a").unwrap(), owner);
    assert_eq!(task.checkpoint().unwrap(), checkpoint);
    assert_eq!(task.current_epoch(), 1);
}
