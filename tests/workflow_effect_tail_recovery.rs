#![cfg(feature = "sqlite")]

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
use nulang::durable_effect_runtime::DurableEffectCoordinator;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    DurableTransition, JournalEntry, LibsqlStore, Runtime, WorkflowActivationId, WorkflowEvent,
    DURABLE_TRANSITION_VERSION,
};

fn test_store_path(label: &str) -> (PathBuf, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-workflow-effect-tail-{label}-{}-{nonce}",
        std::process::id()
    ));
    let db_path = dir.join("workflow.db");
    (dir, db_path)
}

fn admit_command_and_prepare_effect(
    runtime: &mut Runtime,
    actor_id: u64,
) -> (WorkflowActivationId, DurableEffectId) {
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
    let effect_id = DurableEffectId::derive(
        actor_id,
        &format!("workflow-activation:{}", activation.command_sequence),
        0,
        "Inference.ask",
    );
    let spec = DurableEffectSpec::new(
        effect_id,
        "Inference.ask",
        EffectBoundary::External,
        DeliverySemantics::AtLeastOnce,
    );

    let mut coordinator =
        DurableEffectCoordinator::new(runtime.persistence.as_mut(), actor_id, activation_epoch);
    coordinator.begin(spec, b"prompt").unwrap();

    (activation, effect_id)
}

fn assert_open_activation_recovers(
    db_path: &PathBuf,
    actor_id: u64,
    activation: WorkflowActivationId,
) {
    let mut recovered = Runtime::new();
    recovered.persistence = Box::new(LibsqlStore::new(db_path).unwrap());

    assert_eq!(recovered.recover_actor(actor_id), Some(actor_id));
    let actor = recovered
        .actors
        .get(&actor_id)
        .expect("recovery must publish the durable workflow actor");

    assert_eq!(
        actor.current_workflow_activation,
        Some(activation),
        "a durable effect transition must not hide the unfinished accepted command"
    );
    assert_eq!(
        actor.mailbox.len(),
        1,
        "the unfinished accepted command must be re-enqueued exactly once"
    );
}

#[test]
fn prepared_effect_tail_keeps_original_workflow_activation_recoverable() {
    let (store_dir, db_path) = test_store_path("prepared");
    let _ = fs::remove_dir_all(&store_dir);
    fs::create_dir_all(&store_dir).expect("create isolated libsql test directory");

    let (actor_id, activation) = {
        let mut runtime = Runtime::new();
        runtime.persistence = Box::new(LibsqlStore::new(&db_path).unwrap());

        let actor_id = runtime
            .try_spawn_workflow_actor("PreparedEffectTail", Box::new(Vec::new), HashMap::new())
            .unwrap();
        let (activation, _effect_id) = admit_command_and_prepare_effect(&mut runtime, actor_id);

        let tail = runtime
            .persistence
            .load_durable_tail_position(actor_id)
            .unwrap()
            .unwrap();
        assert!(
            tail.sequence > activation.command_sequence,
            "Prepared must advance the atomic tail beyond command admission"
        );

        (actor_id, activation)
    };

    assert_open_activation_recovers(&db_path, actor_id, activation);

    fs::remove_dir_all(store_dir).unwrap();
}

#[test]
fn completed_effect_tail_keeps_original_workflow_activation_recoverable() {
    let (store_dir, db_path) = test_store_path("completed");
    let _ = fs::remove_dir_all(&store_dir);
    fs::create_dir_all(&store_dir).expect("create isolated libsql test directory");

    let (actor_id, activation) = {
        let mut runtime = Runtime::new();
        runtime.persistence = Box::new(LibsqlStore::new(&db_path).unwrap());

        let actor_id = runtime
            .try_spawn_workflow_actor("CompletedEffectTail", Box::new(Vec::new), HashMap::new())
            .unwrap();
        let (activation, effect_id) = admit_command_and_prepare_effect(&mut runtime, actor_id);
        let activation_epoch = runtime.actors[&actor_id].activation_epoch;

        let mut coordinator =
            DurableEffectCoordinator::new(runtime.persistence.as_mut(), actor_id, activation_epoch);
        let recorded = coordinator
            .complete(effect_id, b"prompt", b"recorded-result".to_vec())
            .unwrap();
        assert_eq!(recorded, b"recorded-result");

        let tail = runtime
            .persistence
            .load_durable_tail_position(actor_id)
            .unwrap()
            .unwrap();
        assert!(
            tail.sequence >= activation.command_sequence + 2,
            "Completed must leave the atomic tail beyond command admission and Prepared"
        );

        (actor_id, activation)
    };

    assert_open_activation_recovers(&db_path, actor_id, activation);

    fs::remove_dir_all(store_dir).unwrap();
}

#[test]
fn recovery_refuses_multiple_commands_between_safe_snapshot_and_atomic_tail() {
    let (store_dir, db_path) = test_store_path("ambiguous-commands");
    let _ = fs::remove_dir_all(&store_dir);
    fs::create_dir_all(&store_dir).expect("create isolated libsql test directory");

    let actor_id = {
        let mut runtime = Runtime::new();
        runtime.persistence = Box::new(LibsqlStore::new(&db_path).unwrap());

        let actor_id = runtime
            .try_spawn_workflow_actor("AmbiguousCommands", Box::new(Vec::new), HashMap::new())
            .unwrap();
        let activation_epoch = runtime.actors[&actor_id].activation_epoch;

        for _ in 0..2 {
            let expected_previous_sequence = runtime.persistence.latest_sequence(actor_id);
            let sequence = expected_previous_sequence + 1;
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
        }

        actor_id
    };

    let mut recovered = Runtime::new();
    recovered.persistence = Box::new(LibsqlStore::new(&db_path).unwrap());
    assert_eq!(
        recovered.recover_actor(actor_id),
        None,
        "recovery must fail closed when more than one admitted command exists after the last safe snapshot"
    );

    fs::remove_dir_all(store_dir).unwrap();
}

#[test]
fn recovery_refuses_legacy_history_beyond_atomic_tail() {
    let (store_dir, db_path) = test_store_path("mixed-history");
    let _ = fs::remove_dir_all(&store_dir);
    fs::create_dir_all(&store_dir).expect("create isolated libsql test directory");

    let actor_id = {
        let mut runtime = Runtime::new();
        runtime.persistence = Box::new(LibsqlStore::new(&db_path).unwrap());

        let actor_id = runtime
            .try_spawn_workflow_actor("MixedHistory", Box::new(Vec::new), HashMap::new())
            .unwrap();
        let (_activation, _effect_id) = admit_command_and_prepare_effect(&mut runtime, actor_id);
        let tail = runtime
            .persistence
            .load_durable_tail_position(actor_id)
            .unwrap()
            .unwrap();

        runtime
            .persistence
            .append_workflow_event(
                actor_id,
                WorkflowEvent::Custom {
                    sequence: tail.sequence + 1,
                    replay_id: None,
                    name: "legacy-after-atomic".to_string(),
                    args: Vec::new(),
                },
            )
            .unwrap();

        assert!(
            runtime.persistence.latest_sequence(actor_id) > tail.sequence,
            "fixture must extend legacy history beyond the atomic tail"
        );
        actor_id
    };

    let mut recovered = Runtime::new();
    recovered.persistence = Box::new(LibsqlStore::new(&db_path).unwrap());
    assert_eq!(
        recovered.recover_actor(actor_id),
        None,
        "recovery must fail closed when legacy history extends beyond the RFC 0022 atomic tail"
    );

    fs::remove_dir_all(store_dir).unwrap();
}
