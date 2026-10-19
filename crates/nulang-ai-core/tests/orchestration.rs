use nulang_ai_core::{
    Goal, GoalGraph, ManagerKind, ProgressEvent, ProgressLedger, Task, TaskStatus,
};
use uuid::Uuid;

fn graph_with_dependency() -> GoalGraph {
    let goal = Goal::new("demo", "research and synthesize", 10.0);
    let mut research = Task::new(goal.id, "research", ManagerKind::Research);
    let mut synthesize = Task::new(goal.id, "synthesize", ManagerKind::Research);
    synthesize.dependencies.push(research.id);
    research.status = TaskStatus::Created;

    GoalGraph {
        goal,
        tasks: vec![research, synthesize],
        agents: vec![],
    }
}

#[test]
fn ready_tasks_require_completed_dependencies() {
    let mut graph = graph_with_dependency();
    let research = graph.tasks[0].id;
    let synthesize = graph.tasks[1].id;

    assert_eq!(graph.ready_task_ids().unwrap(), vec![research]);

    graph.tasks[0].status = TaskStatus::Completed;
    assert_eq!(graph.ready_task_ids().unwrap(), vec![synthesize]);
}

#[test]
fn invalid_dependency_graphs_fail_closed() {
    let mut graph = graph_with_dependency();
    graph.tasks[1].dependencies = vec![Uuid::new_v4()];
    let err = graph.ready_task_ids().unwrap_err();
    assert!(err.contains("unknown dependency"));

    let first = graph.tasks[0].id;
    let second = graph.tasks[1].id;
    graph.tasks[0].dependencies = vec![second];
    graph.tasks[1].dependencies = vec![first];
    let err = graph.validate_task_graph().unwrap_err();
    assert!(err.contains("cycle"));
}

#[test]
fn task_graph_rejects_foreign_goal_and_duplicate_ids() {
    let mut graph = graph_with_dependency();
    graph.tasks[0].goal_id = Goal::new("other", "other goal", 1.0).id;
    let err = graph.validate_task_graph().unwrap_err();
    assert!(err.contains("belongs to goal"));

    let mut graph = graph_with_dependency();
    graph.tasks[1].id = graph.tasks[0].id;
    let err = graph.validate_task_graph().unwrap_err();
    assert!(err.contains("Duplicate task id"));
}

#[test]
fn progress_ledger_requests_replan_after_consecutive_stalls() {
    let graph = graph_with_dependency();
    let mut progress = ProgressLedger::new(graph.goal.id, 2);

    assert_eq!(
        progress.record_round(&graph).unwrap(),
        ProgressEvent::Stalled { consecutive: 1 }
    );
    assert_eq!(
        progress.record_round(&graph).unwrap(),
        ProgressEvent::ReplanRequired { consecutive: 2 }
    );
    assert!(progress.replan_required());

    progress.acknowledge_replan();
    assert!(!progress.replan_required());
    assert_eq!(progress.replan_count(), 1);
}

#[test]
fn progress_resets_when_completed_work_advances() {
    let mut graph = graph_with_dependency();
    let mut progress = ProgressLedger::new(graph.goal.id, 3);

    assert_eq!(
        progress.record_round(&graph).unwrap(),
        ProgressEvent::Stalled { consecutive: 1 }
    );

    graph.tasks[0].status = TaskStatus::Completed;
    assert_eq!(
        progress.record_round(&graph).unwrap(),
        ProgressEvent::Advanced
    );
    assert_eq!(progress.stall_count(), 0);

    graph.tasks[1].status = TaskStatus::Completed;
    assert_eq!(
        progress.record_round(&graph).unwrap(),
        ProgressEvent::Complete
    );
}

#[test]
fn progress_ledger_round_trips_and_is_goal_bound() {
    let graph = graph_with_dependency();
    let mut progress = ProgressLedger::new(graph.goal.id, 4);
    let _ = progress.record_round(&graph).unwrap();

    let json = serde_json::to_string(&progress).unwrap();
    let restored: ProgressLedger = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, progress);

    let other = GoalGraph {
        goal: Goal::new("other", "different goal", 1.0),
        tasks: vec![],
        agents: vec![],
    };
    let err = progress.record_round(&other).unwrap_err();
    assert!(err.contains("bound to goal"));
}
