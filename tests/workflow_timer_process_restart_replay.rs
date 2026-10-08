#![cfg(feature = "sqlite")]

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use nulang::runtime::{
    DurableTransition, JournalEntry, LibsqlStore, Runtime, WorkflowActivationId, WorkflowEvent,
    WorkflowReplayEventId, DURABLE_TRANSITION_VERSION,
};

fn test_store_path() -> (PathBuf, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-workflow-timer-restart-{}-{nonce}",
        std::process::id()
    ));
    let db_path = dir.join("workflow.db");
    (dir, db_path)
}

#[test]
fn timer_preparation_replay_survives_process_restart_without_duplicate_history_or_arm() {
    let (store_dir, db_path) = test_store_path();
    let _ = fs::remove_dir_all(&store_dir);
    fs::create_dir_all(&store_dir).expect("create isolated libsql test directory");

    let (actor_id, activation) = {
        let mut runtime = Runtime::new();
        runtime.persistence = Box::new(LibsqlStore::new(&db_path).unwrap());

        let actor_id = runtime
            .try_spawn_workflow_actor("RestartTimer", Box::new(Vec::new), HashMap::new())
            .unwrap();
        assert_ne!(actor_id, 0);

        // Admit one unfinished workflow command through the public RFC 0022
        // persistence contract. This mirrors Runtime's command-admission
        // transition while keeping the acceptance test outside private
        // workflow implementation helpers.
        let expected_previous_sequence = runtime.persistence.latest_sequence(actor_id);
        let sequence = expected_previous_sequence + 1;
        let activation_epoch = runtime.actors[&actor_id].activation_epoch;
        runtime
            .persistence
            .commit_transition(DurableTransition {
                version: DURABLE_TRANSITION_VERSION,
                actor_id,
                activation_epoch,
                sequence,
                expected_previous_sequence,
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

        let activation = WorkflowActivationId::new(actor_id, sequence);
        {
            let actor = runtime.actors.get_mut(&actor_id).unwrap();
            actor.sequence = sequence;
            actor.current_workflow_activation = Some(activation);
        }

        runtime
            .schedule_workflow_timer(actor_id, "wake", 60_000)
            .unwrap();

        let timer_sets: Vec<_> = runtime
            .persistence
            .read_workflow_events(actor_id)
            .into_iter()
            .filter(|event| matches!(event, WorkflowEvent::TimerSet { .. }))
            .collect();
        assert_eq!(timer_sets.len(), 1);
        assert!(matches!(
            &timer_sets[0],
            WorkflowEvent::TimerSet {
                replay_id: Some(id),
                name,
                duration_ms: 60_000,
                ..
            } if *id == WorkflowReplayEventId::new(activation, 0) && name == "wake"
        ));
        assert_eq!(runtime.timer_wheel.len(), 1);

        (actor_id, activation)
    }; // Drop Runtime: process-local timer-wheel state is now gone.

    let mut recovered = Runtime::new();
    recovered.persistence = Box::new(LibsqlStore::new(&db_path).unwrap());
    assert_eq!(recovered.timer_wheel.len(), 0);

    assert_eq!(recovered.recover_actor(actor_id), Some(actor_id));
    assert_eq!(
        recovered.actors[&actor_id].current_workflow_activation,
        Some(activation),
        "recovery must preserve the same unfinished accepted command"
    );
    assert_eq!(
        recovered.timer_wheel.len(),
        1,
        "recovery must rehydrate the one durable timer into the fresh process"
    );

    // Re-executing the same deterministic timer preparation must consume the
    // committed TimerSet. It must neither append another durable record nor arm
    // a second process-local timer.
    recovered
        .schedule_workflow_timer(actor_id, "wake", 60_000)
        .unwrap();

    assert_eq!(
        recovered
            .persistence
            .read_workflow_events(actor_id)
            .into_iter()
            .filter(|event| matches!(event, WorkflowEvent::TimerSet { .. }))
            .count(),
        1,
        "process-restart replay must not duplicate durable TimerSet history"
    );
    assert_eq!(
        recovered.timer_wheel.len(),
        1,
        "process-restart replay must not arm a duplicate live timer"
    );
    assert_eq!(
        recovered.actors[&actor_id].workflow_replay_activation,
        Some(activation)
    );
    assert_eq!(
        recovered.actors[&actor_id].workflow_replay_event_ordinal, 1,
        "the replay cursor must consume the recovered TimerSet exactly once"
    );

    drop(recovered);
    fs::remove_dir_all(store_dir).unwrap();
}
