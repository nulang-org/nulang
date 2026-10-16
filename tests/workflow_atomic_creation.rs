use std::collections::HashMap;

use nulang::runtime::{JsonFileStore, Runtime, WorkflowEvent};

#[test]
fn workflow_start_commits_initial_snapshot_and_start_event_in_one_transition() {
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .try_spawn_workflow_actor("AtomicStart", Box::new(Vec::new), HashMap::new())
        .expect("new workflow must atomically commit initial state");

    let committed_tail = runtime
        .persistence
        .load_durable_tail_position(actor_id)
        .unwrap()
        .expect("workflow creation must produce an atomic tail");
    let snapshot = runtime
        .persistence
        .load_snapshot(actor_id)
        .expect("workflow creation must persist initial state");
    let starts: Vec<_> = runtime
        .persistence
        .read_workflow_events(actor_id)
        .into_iter()
        .filter(|event| matches!(event, WorkflowEvent::WorkflowStarted { .. }))
        .collect();

    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].sequence(), committed_tail.sequence);
    assert_eq!(snapshot.sequence, committed_tail.sequence);
    assert_eq!(runtime.actors[&actor_id].sequence, committed_tail.sequence);
}

#[test]
fn unsupported_atomic_store_rejects_workflow_creation_without_publishing_actor() {
    use std::io::ErrorKind;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-atomic-start-unsupported-{}-{nonce}",
        std::process::id()
    ));
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(JsonFileStore::new(&directory).unwrap());

    let error = runtime
        .try_spawn_workflow_actor("UnsupportedStart", Box::new(Vec::new), HashMap::new())
        .expect_err("workflow must not fall back to separate event and snapshot writes");

    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(runtime.actors.is_empty(), "failed actor must not be published");
    assert!(
        std::fs::read_dir(&directory).unwrap().next().is_none(),
        "no partial durable actor files may survive a rejected atomic creation"
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(feature = "sqlite")]
#[test]
fn workflow_creation_recovers_from_single_committed_libsql_transition() {
    use nulang::runtime::LibsqlStore;
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(LibsqlStore::in_memory().unwrap());

    let actor_id = runtime
        .try_spawn_workflow_actor("SqlAtomicStart", Box::new(Vec::new), HashMap::new())
        .unwrap();

    let tail = runtime
        .persistence
        .load_durable_tail_position(actor_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        runtime.persistence.load_snapshot(actor_id).unwrap().sequence,
        tail.sequence
    );
    assert_eq!(
        runtime
            .persistence
            .read_workflow_events(actor_id)
            .into_iter()
            .filter(|event| matches!(event, WorkflowEvent::WorkflowStarted { .. }))
            .count(),
        1
    );
}
