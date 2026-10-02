use nulang_ai::{
    ProgressEvent, ProgressLedger, SupervisorRuntime, SupervisorTeam, TaskLedger, TaskStatus,
};

#[test]
fn task_ledger_releases_tasks_only_after_dependencies_complete() {
    let mut ledger = TaskLedger::new();
    let research = ledger.add_task("research", vec![]).unwrap();
    let synthesize = ledger.add_task("synthesize", vec![research]).unwrap();

    assert_eq!(
        ledger.ready_task_ids(),
        vec![research],
        "dependent work must not become ready before its prerequisites"
    );

    ledger.start(research, 7).unwrap();
    assert_eq!(ledger.task(research).unwrap().status, TaskStatus::Running);
    ledger.complete(research, "notes").unwrap();

    assert_eq!(ledger.ready_task_ids(), vec![synthesize]);
    assert_eq!(
        ledger.task(synthesize).unwrap().status,
        TaskStatus::Pending
    );
}

#[test]
fn task_ledger_rejects_unknown_dependencies() {
    let mut ledger = TaskLedger::new();
    let err = ledger.add_task("orphan", vec![99]).unwrap_err();
    assert!(err.contains("dependency 99"));
    assert!(ledger.is_empty());
}

#[test]
fn progress_ledger_requests_replan_after_consecutive_stalls() {
    let mut tasks = TaskLedger::new();
    tasks.add_task("investigate", vec![]).unwrap();
    let mut progress = ProgressLedger::new(2);

    assert_eq!(
        progress.record_round(&tasks),
        ProgressEvent::Stalled { consecutive: 1 }
    );
    assert_eq!(
        progress.record_round(&tasks),
        ProgressEvent::ReplanRequired { consecutive: 2 }
    );
    assert!(progress.replan_required());

    progress.acknowledge_replan();
    assert!(!progress.replan_required());
    assert_eq!(progress.replan_count(), 1);
}

#[test]
fn progress_ledger_resets_stall_counter_when_work_completes() {
    let mut tasks = TaskLedger::new();
    let task = tasks.add_task("inspect", vec![]).unwrap();
    let mut progress = ProgressLedger::new(3);

    assert_eq!(
        progress.record_round(&tasks),
        ProgressEvent::Stalled { consecutive: 1 }
    );

    tasks.start(task, 11).unwrap();
    tasks.complete(task, "done").unwrap();

    assert_eq!(progress.record_round(&tasks), ProgressEvent::Complete);
    assert_eq!(progress.stall_count(), 0);
}

#[test]
fn ledgers_are_checkpointable_with_stable_json_round_trips() {
    let mut tasks = TaskLedger::new();
    let first = tasks.add_task("research", vec![]).unwrap();
    let second = tasks.add_task("write", vec![first]).unwrap();
    tasks.start(first, 3).unwrap();
    tasks.complete(first, "evidence").unwrap();
    tasks.start(second, 4).unwrap();

    let mut progress = ProgressLedger::new(4);
    assert_eq!(progress.record_round(&tasks), ProgressEvent::Advanced);

    let tasks_json = serde_json::to_string(&tasks).unwrap();
    let progress_json = serde_json::to_string(&progress).unwrap();

    let restored_tasks: TaskLedger = serde_json::from_str(&tasks_json).unwrap();
    let restored_progress: ProgressLedger = serde_json::from_str(&progress_json).unwrap();

    assert_eq!(restored_tasks, tasks);
    assert_eq!(restored_progress, progress);
}

#[derive(Default)]
struct TrackingRuntime {
    calls: Vec<(String, u64, String)>,
}

impl SupervisorRuntime for TrackingRuntime {
    fn ask_agent(&mut self, agent_id: u64, prompt: &str) -> Result<String, String> {
        self.calls
            .push(("ask".to_string(), agent_id, prompt.to_string()));
        Ok(format!("ask:{agent_id}"))
    }

    fn delegate_agent(&mut self, agent_id: u64, prompt: &str) -> Result<String, String> {
        self.calls
            .push(("delegate".to_string(), agent_id, prompt.to_string()));
        Ok(format!("delegate:{agent_id}"))
    }

    fn handoff_agent(&mut self, agent_id: u64, context: &str) -> Result<String, String> {
        self.calls
            .push(("handoff".to_string(), agent_id, context.to_string()));
        Ok(format!("handoff:{agent_id}"))
    }
}

#[test]
fn supervisor_exposes_distinct_delegate_and_handoff_semantics() {
    let team = SupervisorTeam::new()
        .worker("researcher", 10, "find evidence")
        .worker("writer", 20, "own the final response");
    let mut runtime = TrackingRuntime::default();

    let delegated = team
        .delegate(&mut runtime, "researcher", "find the constraints")
        .unwrap();
    let handed_off = team
        .handoff(&mut runtime, "writer", "take ownership from here")
        .unwrap();

    assert_eq!(delegated, "delegate:10");
    assert_eq!(handed_off, "handoff:20");
    assert_eq!(runtime.calls[0].0, "delegate");
    assert_eq!(runtime.calls[1].0, "handoff");
}

#[test]
fn supervisor_reports_unknown_worker_for_explicit_routing() {
    let team = SupervisorTeam::new().worker("researcher", 10, "find evidence");
    let mut runtime = TrackingRuntime::default();

    let err = team
        .delegate(&mut runtime, "missing", "task")
        .unwrap_err();
    assert!(err.contains("Worker missing not found"));
    assert!(runtime.calls.is_empty());
}
