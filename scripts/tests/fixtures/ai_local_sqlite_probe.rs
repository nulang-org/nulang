//! Independently build this fixture with pinned old/new nulang-ai-local crates.
use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use nulang_ai_core::{
    ConversationMessage, ConversationState, Goal, GoalGraph, GoalStatus, ManagerKind, Task,
    TaskStatus,
};
use nulang_ai_local::{SqliteStore, StoreError};
use serde_json::json;
use std::{env, path::Path, time::Duration};
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn fixture(upgraded: bool) -> (GoalGraph, ConversationState) {
    let created = Utc.with_ymd_and_hms(2026, 11, 26, 12, 0, 0).unwrap();
    let updated = if upgraded {
        created + ChronoDuration::seconds(60)
    } else {
        created
    };
    let mut goal = Goal::new("disposable-α", "legacy intent", 12.5);
    goal.id = id(100);
    goal.conversation_id = Some(id(200));
    goal.created_at = created;
    goal.updated_at = updated;
    goal.deadline = Some(created + ChronoDuration::days(1));
    goal.desired_state = json!({"count": 7, "label": "ação", "optional": null});
    goal.constraints = json!({"allowed": true, "regions": ["BR", "US"]});
    goal.success_criteria = vec!["preserve state".into(), "reopen safely".into()];
    goal.status = GoalStatus::Running;
    if upgraded {
        goal.intent = "candidate intent".into();
        goal.desired_state["count"] = json!(9);
        goal.status = GoalStatus::Verifying;
        goal.budget_usd = 15.75;
    }

    let mut first = Task::new(goal.id, "first", ManagerKind::Engineering);
    first.id = id(301);
    first.created_at = created;
    first.updated_at = updated;
    first.required_capabilities = vec!["read".into(), "write".into()];
    first.acceptance_criteria = vec!["typed data".into()];
    first.budget_usd = 2.5;
    first.timeout = Duration::from_secs(125);
    first.status = TaskStatus::Ready;
    if upgraded {
        first.description = "first, updated".into();
        first.status = TaskStatus::Assigned;
        first.assigned_agent_id = Some("agent-α".into());
    }

    let mut second = Task::new(goal.id, "second", ManagerKind::Research);
    second.id = id(302);
    second.created_at = created;
    second.updated_at = created;
    second.parent_task_id = Some(first.id);
    second.dependencies = vec![first.id];
    second.status = TaskStatus::Blocked;

    let mut third = Task::new(goal.id, "third", ManagerKind::Data);
    third.id = id(303);
    third.created_at = created + ChronoDuration::seconds(1);
    third.updated_at = third.created_at;
    third.dependencies = vec![first.id, second.id];
    third.status = TaskStatus::Created;

    let mut conversation = ConversationState {
        id: id(200),
        project_id: goal.project_id.clone(),
        director_id: None,
        active_goal_id: Some(goal.id),
        messages: vec![ConversationMessage {
            role: "user".into(),
            content: "Olá — preserve the conversation".into(),
            timestamp: created,
        }],
        created_at: created,
        updated_at: updated,
    };
    if upgraded {
        conversation.director_id = Some("director-α".into());
        conversation.messages.push(ConversationMessage {
            role: "assistant".into(),
            content: "Persisted candidate response".into(),
            timestamp: updated,
        });
    }
    (
        GoalGraph {
            goal,
            tasks: vec![first, second, third],
            agents: Vec::new(),
        },
        conversation,
    )
}

fn write(store: &SqliteStore, upgraded: bool) {
    let (graph, conversation) = fixture(upgraded);
    store.upsert_goal(&graph.goal).unwrap();
    // Reverse insertion order; stable task ordering is part of the contract.
    for task in graph.tasks.iter().rev() {
        store.upsert_task(task).unwrap();
    }
    store.upsert_conversation(&conversation).unwrap();
}

fn verify(store: &SqliteStore, upgraded: bool) {
    let (graph, conversation) = fixture(upgraded);
    assert_eq!(store.list_goals().unwrap(), vec![graph.goal.clone()]);
    assert_eq!(store.get_goal_graph(graph.goal.id).unwrap(), graph);
    assert_eq!(
        store.get_conversation(conversation.id).unwrap(),
        conversation
    );
    assert!(matches!(
        store.get_goal_graph(id(999)),
        Err(StoreError::GoalNotFound(_))
    ));
    assert!(matches!(
        store.get_conversation(id(999)),
        Err(StoreError::ConversationNotFound(_))
    ));
    println!("verified typed goal, 3 ordered tasks, linked conversation, updated={upgraded}");
}

fn main() {
    let args: Vec<_> = env::args().collect();
    assert_eq!(args.len(), 3);
    let store = SqliteStore::open(Path::new(&args[2])).expect("open disposable AI-local store");
    match args[1].as_str() {
        "seed" => {
            assert!(store.list_goals().unwrap().is_empty());
            write(&store, false);
            verify(&store, false);
        }
        "check-old" => verify(&store, false),
        "upgrade" => {
            verify(&store, false);
            write(&store, true);
            verify(&store, true);
        }
        "check-new" => verify(&store, true),
        _ => panic!("invalid probe mode"),
    }
}
