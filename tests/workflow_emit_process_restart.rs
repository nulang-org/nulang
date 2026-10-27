#![cfg(feature = "sqlite")]
//! Prove heap-backed workflow output replay through a new OS process.
use nulang::runtime::{
    DurableTransition, JournalEntry, LibsqlStore, PersistedValue, Runtime, WorkflowActivationId,
    WorkflowEvent, WorkflowReplayEventId, DURABLE_TRANSITION_VERSION,
};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const DB: &str = "NULANG_WORKFLOW_EMIT_RESTART_DB";
const ACTOR: &str = "NULANG_WORKFLOW_EMIT_RESTART_ACTOR";
const SEQUENCE: &str = "NULANG_WORKFLOW_EMIT_RESTART_SEQUENCE";
const TEXT: &str = "verified heap string receipt";

fn receipts(rt: &Runtime, id: u64) -> Vec<WorkflowEvent> {
    rt.persistence
        .read_workflow_events(id)
        .into_iter()
        .filter(|e| matches!(e, WorkflowEvent::Custom { name, .. } if name == "WorkflowOutput"))
        .collect()
}

#[test]
fn workflow_heap_string_receipt_survives_process_restart() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-emit-restart-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("journal.db");

    let (actor_id, activation) = {
        let mut rt = Runtime::new();
        rt.persistence = Box::new(LibsqlStore::new(&db).unwrap());
        let id = rt
            .try_spawn_workflow_actor("EmitRestart", Box::new(Vec::new), HashMap::new())
            .unwrap();
        assert_ne!(id, 0);
        let previous = rt.persistence.latest_sequence(id);
        let sequence = previous + 1;
        let epoch = rt.actors[&id].activation_epoch;
        rt.persistence
            .commit_transition(DurableTransition {
                version: DURABLE_TRANSITION_VERSION,
                actor_id: id,
                activation_epoch: epoch,
                sequence,
                expected_previous_sequence: previous,
                command: Some(JournalEntry {
                    sequence,
                    behavior_id: 0,
                    payload: vec![],
                }),
                snapshot: None,
                workflow_events: vec![],
                domain_events: vec![],
                durable_effects: vec![],
                outbox: vec![],
            })
            .unwrap();
        let activation = WorkflowActivationId::new(id, sequence);
        let value = {
            let actor = rt.actors.get_mut(&id).unwrap();
            actor.sequence = sequence;
            actor.current_workflow_activation = Some(activation);
            actor.allocate_string(TEXT)
        };
        assert!(!value.is_nil());
        rt.try_emit_event(id, "WorkflowOutput", &[value]).unwrap();
        assert!(
            matches!(receipts(&rt, id).as_slice(), [WorkflowEvent::Custom {
            replay_id: Some(r), args, ..
        }] if *r == WorkflowReplayEventId::new(activation, 0)
            && args == &vec![PersistedValue::String(TEXT.into())])
        );
        (id, activation)
    }; // Original actor heap and runtime are gone.

    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "workflow_emit_restart_child", "--nocapture"])
        .env(DB, &db)
        .env(ACTOR, actor_id.to_string())
        .env(SEQUENCE, activation.command_sequence.to_string())
        .output()
        .expect("spawn independent recovery process");
    assert!(
        output.status.success(),
        "subprocess replay failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut check = Runtime::new();
    check.persistence = Box::new(LibsqlStore::new(&db).unwrap());
    assert_eq!(
        receipts(&check, actor_id).len(),
        1,
        "replay must never duplicate durable output"
    );
    drop(check);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn workflow_emit_restart_child() {
    let Some(db) = std::env::var_os(DB).map(PathBuf::from) else {
        return;
    };
    let id: u64 = std::env::var(ACTOR).unwrap().parse().unwrap();
    let sequence: u64 = std::env::var(SEQUENCE).unwrap().parse().unwrap();
    let activation = WorkflowActivationId::new(id, sequence);
    let mut rt = Runtime::new();
    rt.persistence = Box::new(LibsqlStore::new(&db).unwrap());
    assert_eq!(receipts(&rt, id).len(), 1);
    assert_eq!(rt.recover_actor(id), Some(id), "recover unfinished command");
    assert_eq!(rt.actors[&id].current_workflow_activation, Some(activation));
    let new_process_value = rt.actors.get_mut(&id).unwrap().allocate_string(TEXT);
    assert!(!new_process_value.is_nil());
    rt.try_emit_event(id, "WorkflowOutput", &[new_process_value])
        .expect("same activation and payload must consume committed receipt");
    assert_eq!(rt.actors[&id].workflow_replay_event_ordinal, 1);
    assert_eq!(receipts(&rt, id).len(), 1);
}
