use nulang::reconcile::{
    ReconcileController, ReconcileRetryDecision, ReconcileRetryPolicy, ReconcileTimerAdmission,
};
use nulang::runtime::{JsonFileStore, Runtime, WorkflowEvent};
use nulang::vm::Value;
use std::collections::HashMap;

fn retry_policy(max_retries: u32) -> ReconcileRetryPolicy {
    ReconcileRetryPolicy::new(100, 1_000, 2, max_retries, 0).unwrap()
}

fn workflow_runtime() -> (Runtime, u64) {
    let mut runtime = Runtime::new();
    let actor_id = runtime.spawn_workflow_actor(
        "ReconcileController",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        HashMap::new(),
    );
    (runtime, actor_id)
}

fn broken_json_store() -> (JsonFileStore, std::path::PathBuf) {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "nulang-reconcile-controller-fail-{}-{nonce}",
        std::process::id()
    ));
    let store = JsonFileStore::new(&path).unwrap();
    std::fs::remove_dir_all(&path).unwrap();
    std::fs::write(&path, b"not a directory").unwrap();
    (store, path)
}

#[test]
fn retry_schedule_uses_existing_durable_workflow_timer_path() {
    let (mut runtime, actor_id) = workflow_runtime();
    let mut controller = ReconcileController::new("replicas=3".to_string(), retry_policy(3), 7);
    let attempt = controller.begin_attempt().unwrap();

    let decision = controller
        .schedule_retry(&mut runtime, actor_id, attempt)
        .unwrap();
    let ReconcileRetryDecision::Scheduled(ticket) = decision else {
        panic!("expected scheduled retry");
    };

    assert_eq!(runtime.timer_wheel.len(), 1);
    let events = runtime.persistence.read_workflow_events(actor_id);
    assert!(events.iter().any(|event| {
        matches!(
            event,
            WorkflowEvent::TimerSet { name, duration_ms, .. }
                if name == &ticket.timer_name() && *duration_ms == ticket.delay_ms()
        )
    }));
}

#[test]
fn persistence_failure_rolls_controller_state_back_and_never_arms_timer() {
    let (mut runtime, actor_id) = workflow_runtime();
    let mut controller = ReconcileController::new("replicas=3".to_string(), retry_policy(3), 11);
    let attempt = controller.begin_attempt().unwrap();
    let before = controller.snapshot();

    let (store, path) = broken_json_store();
    runtime.persistence = Box::new(store);

    assert!(controller
        .schedule_retry(&mut runtime, actor_id, attempt)
        .is_err());
    assert_eq!(controller.snapshot(), before);
    assert!(runtime.timer_wheel.is_empty());

    let _ = std::fs::remove_file(path);
}

#[test]
fn non_workflow_actor_is_rejected_before_retry_state_or_timer_mutation() {
    let mut runtime = Runtime::new();
    let actor_id = runtime.spawn_actor(Box::new(Vec::new));
    let mut controller = ReconcileController::new("replicas=3".to_string(), retry_policy(3), 13);
    let attempt = controller.begin_attempt().unwrap();
    let before = controller.snapshot();

    assert!(controller
        .schedule_retry(&mut runtime, actor_id, attempt)
        .is_err());
    assert_eq!(controller.snapshot(), before);
    assert!(runtime.timer_wheel.is_empty());
}

#[test]
fn current_retry_timer_starts_a_new_fenced_attempt() {
    let (mut runtime, actor_id) = workflow_runtime();
    let mut controller = ReconcileController::new("replicas=3".to_string(), retry_policy(3), 17);
    let first_attempt = controller.begin_attempt().unwrap();
    let ReconcileRetryDecision::Scheduled(ticket) = controller
        .schedule_retry(&mut runtime, actor_id, first_attempt)
        .unwrap()
    else {
        panic!("expected scheduled retry");
    };

    let admission = controller.admit_retry_timer(&ticket.timer_name()).unwrap();
    let ReconcileTimerAdmission::Ready(next_attempt) = admission else {
        panic!("expected current timer to start a retry attempt");
    };

    assert_eq!(next_attempt.generation(), 1);
    assert_eq!(next_attempt.ordinal(), 2);
}

#[test]
fn desired_state_change_makes_an_old_retry_timer_stale_without_mutation() {
    let (mut runtime, actor_id) = workflow_runtime();
    let mut controller = ReconcileController::new("replicas=3".to_string(), retry_policy(3), 19);
    let attempt = controller.begin_attempt().unwrap();
    let ReconcileRetryDecision::Scheduled(ticket) = controller
        .schedule_retry(&mut runtime, actor_id, attempt)
        .unwrap()
    else {
        panic!("expected scheduled retry");
    };

    controller.update_desired("replicas=5".to_string()).unwrap();
    let before = controller.snapshot();

    let admission = controller.admit_retry_timer(&ticket.timer_name()).unwrap();
    assert!(matches!(admission, ReconcileTimerAdmission::Stale(identity) if identity == ticket.identity()));
    assert_eq!(controller.snapshot(), before);
}

#[test]
fn unrelated_timer_context_is_ignored_without_mutation() {
    let mut controller = ReconcileController::new("replicas=3".to_string(), retry_policy(3), 23);
    let before = controller.snapshot();

    assert_eq!(
        controller.admit_retry_timer("payment_timeout").unwrap(),
        ReconcileTimerAdmission::Unrelated
    );
    assert_eq!(controller.snapshot(), before);
}

#[test]
fn restored_controller_accepts_the_same_current_retry_identity() {
    let (mut runtime, actor_id) = workflow_runtime();
    let mut controller = ReconcileController::new("replicas=3".to_string(), retry_policy(3), 29);
    let attempt = controller.begin_attempt().unwrap();
    let ReconcileRetryDecision::Scheduled(ticket) = controller
        .schedule_retry(&mut runtime, actor_id, attempt)
        .unwrap()
    else {
        panic!("expected scheduled retry");
    };

    let snapshot = controller.snapshot();
    let mut restored = ReconcileController::restore(snapshot, retry_policy(3), 29).unwrap();

    let admission = restored.admit_retry_timer(&ticket.timer_name()).unwrap();
    let ReconcileTimerAdmission::Ready(attempt) = admission else {
        panic!("restored controller must admit its current retry timer");
    };
    assert_eq!(attempt.generation(), ticket.generation());
}

#[test]
fn retry_budget_exhaustion_never_arms_an_extra_timer() {
    let (mut runtime, actor_id) = workflow_runtime();
    let mut controller = ReconcileController::new("replicas=3".to_string(), retry_policy(1), 31);

    let first = controller.begin_attempt().unwrap();
    assert!(matches!(
        controller.schedule_retry(&mut runtime, actor_id, first).unwrap(),
        ReconcileRetryDecision::Scheduled(_)
    ));
    assert_eq!(runtime.timer_wheel.len(), 1);

    let current_timer = controller
        .snapshot()
        .retry_ordinal;
    assert_eq!(current_timer, 1);

    let second = controller.begin_attempt().unwrap();
    assert_eq!(
        controller.schedule_retry(&mut runtime, actor_id, second).unwrap(),
        ReconcileRetryDecision::Exhausted
    );
    assert_eq!(runtime.timer_wheel.len(), 1);
    assert!(!controller.state().needs_reconcile());
}
