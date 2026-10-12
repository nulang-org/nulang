use chrono::{TimeZone, Utc};
use nulang_ai_core::{Goal, ManagerKind, Task};
use nulang_ai_local::SqliteStore;
use uuid::Uuid;

#[test]
fn goal_graph_tasks_restore_in_stable_created_at_then_id_order() {
    let dir = std::env::temp_dir().join(format!("nulang-ai-store-order-{}", Uuid::new_v4()));
    let store = SqliteStore::open(&dir).unwrap();

    let mut goal = Goal::new("demo", "stable task ordering", 10.0);
    goal.id = Uuid::parse_str("10000000-0000-0000-0000-000000000000").unwrap();
    store.upsert_goal(&goal).unwrap();

    let first_time = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).unwrap();
    let later_time = Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 1).unwrap();

    let mut first = Task::new(goal.id, "first", ManagerKind::Engineering);
    first.id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    first.created_at = first_time;
    first.updated_at = first_time;

    let mut second = Task::new(goal.id, "second", ManagerKind::Engineering);
    second.id = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
    second.created_at = first_time;
    second.updated_at = first_time;

    let mut third = Task::new(goal.id, "third", ManagerKind::Engineering);
    third.id = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();
    third.created_at = later_time;
    third.updated_at = later_time;

    // Deliberately persist in the opposite order. Replay order must be a
    // property of the store query, not an accident of SQLite row insertion.
    store.upsert_task(&third).unwrap();
    store.upsert_task(&second).unwrap();
    store.upsert_task(&first).unwrap();

    let graph = store.get_goal_graph(goal.id).unwrap();
    let ids: Vec<_> = graph.tasks.iter().map(|task| task.id).collect();
    assert_eq!(ids, vec![first.id, second.id, third.id]);

    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}
