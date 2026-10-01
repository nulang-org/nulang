use nulang::runtime::{
    Actor, ActorSnapshot, DurableTransition, Runtime, DURABLE_TRANSITION_VERSION,
};

#[cfg(feature = "sqlite")]
use nulang::runtime::{LibsqlStore, PersistenceStore};

#[test]
fn legacy_snapshot_without_activation_epoch_defaults_to_initial_epoch() {
    let snapshot: ActorSnapshot =
        serde_json::from_str(r#"{"actor_id":41,"sequence":7,"state":{}}"#).unwrap();

    assert_eq!(snapshot.activation_epoch, 1);
}

#[test]
fn snapshot_activation_epoch_round_trips_exactly() {
    let snapshot = ActorSnapshot {
        actor_id: 42,
        sequence: 8,
        activation_epoch: 9,
        ..ActorSnapshot::default()
    };

    let encoded = serde_json::to_vec(&snapshot).unwrap();
    let decoded: ActorSnapshot = serde_json::from_slice(&encoded).unwrap();

    assert_eq!(decoded.activation_epoch, 9);
}

#[test]
fn checkpoint_persists_live_actor_activation_epoch() {
    let mut runtime = Runtime::new();
    let actor_id = 43;
    let mut actor = Actor::new(actor_id, "epoch-checkpoint", 0);
    actor.persistent = true;
    actor.activation_epoch = 11;
    runtime.actors.insert(actor_id, actor);

    runtime.checkpoint_actor(actor_id);

    let snapshot = runtime
        .persistence
        .load_snapshot(actor_id)
        .expect("checkpoint must persist a snapshot");
    assert_eq!(snapshot.activation_epoch, 11);
}

#[test]
fn recovery_rejects_explicit_zero_activation_epoch() {
    let mut runtime = Runtime::new();
    let actor_id = 48;
    runtime
        .persistence
        .save_snapshot(ActorSnapshot {
            actor_id,
            sequence: 1,
            activation_epoch: 0,
            ..ActorSnapshot::default()
        })
        .unwrap();

    assert_eq!(runtime.recover_actor(actor_id), None);
    assert!(
        !runtime.actors.contains_key(&actor_id),
        "invalid epoch-zero history must not publish a live actor"
    );
}

#[test]
fn recovery_restores_persisted_activation_epoch() {
    let mut runtime = Runtime::new();
    let actor_id = 44;
    runtime
        .persistence
        .save_snapshot(ActorSnapshot {
            actor_id,
            sequence: 3,
            activation_epoch: 13,
            ..ActorSnapshot::default()
        })
        .unwrap();

    assert_eq!(runtime.recover_actor(actor_id), Some(actor_id));

    let actor = runtime
        .actors
        .get(&actor_id)
        .expect("recovered actor must be published");
    assert_eq!(actor.activation_epoch, 13);
}

#[test]
fn durable_transition_rejects_snapshot_activation_epoch_mismatch() {
    let actor_id = 47;
    let transition = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: 5,
        sequence: 1,
        expected_previous_sequence: 0,
        command: None,
        snapshot: Some(ActorSnapshot {
            actor_id,
            sequence: 1,
            activation_epoch: 4,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    };

    let error = transition
        .digest()
        .expect_err("snapshot epoch mismatch must fail closed");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_snapshot_round_trips_activation_epoch() {
    let mut store = LibsqlStore::in_memory().unwrap();
    let actor_id = 45;
    store
        .save_snapshot(ActorSnapshot {
            actor_id,
            sequence: 1,
            activation_epoch: 17,
            ..ActorSnapshot::default()
        })
        .unwrap();

    let recovered = store.load_snapshot(actor_id).unwrap();
    assert_eq!(recovered.activation_epoch, 17);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_atomic_transition_preserves_snapshot_activation_epoch() {
    let mut store = LibsqlStore::in_memory().unwrap();
    let actor_id = 46;
    store
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: 19,
            sequence: 1,
            expected_previous_sequence: 0,
            command: None,
            snapshot: Some(ActorSnapshot {
                actor_id,
                sequence: 1,
                activation_epoch: 19,
                ..ActorSnapshot::default()
            }),
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .unwrap();

    let recovered = store.load_snapshot(actor_id).unwrap();
    assert_eq!(recovered.activation_epoch, 19);
}
