use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableTransition, EventEntry, JournalEntry, JsonFileStore, PersistedValue,
    PersistenceStore, WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};

const ACTOR_ID: u64 = 42;

fn fresh_dir(name: &str) -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after UNIX epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-json-atomic-{name}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("test directory must be created");
    dir
}

fn effect_spec() -> DurableEffectSpec {
    let site = effect_site_id(
        "json-atomic-persistence",
        EffectSiteOwnerKind::Behavior,
        "JsonActor.run",
        "Provider.ask",
        0,
    );
    DurableEffectSpec::new(
        DurableEffectId::derive_from_site(ACTOR_ID, "turn:1", site, 0),
        "Provider.ask",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    )
}

fn transition(
    sequence: u64,
    previous: u64,
    value: i64,
    effect: Option<DurableEffectPersistenceRecord>,
) -> DurableTransition {
    let mut state = HashMap::new();
    state.insert("value".to_string(), PersistedValue::Int(value));

    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR_ID,
        activation_epoch: 1,
        sequence,
        expected_previous_sequence: previous,
        command: Some(JournalEntry {
            sequence,
            behavior_id: 7,
            payload: vec![PersistedValue::Int(value)],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: ACTOR_ID,
            sequence,
            state,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::Custom {
            sequence,
            name: format!("step-{sequence}"),
            args: vec![PersistedValue::Int(value)],
        }],
        domain_events: vec![EventEntry {
            sequence,
            field_name: "value".to_string(),
            event_name: "Set".to_string(),
            args: vec![PersistedValue::Int(value)],
            value: PersistedValue::Int(value),
        }],
        durable_effects: effect.into_iter().collect(),
        outbox: Vec::new(),
    }
}

#[test]
fn json_atomic_transition_round_trips_after_reopen() {
    let dir = fresh_dir("round-trip");
    let mut store = JsonFileStore::new(&dir).unwrap();

    let committed = store.commit_transition(transition(1, 0, 10, None)).unwrap();
    assert_eq!(committed.sequence, 1);
    drop(store);

    let reopened = JsonFileStore::new(&dir).unwrap();
    assert_eq!(reopened.latest_sequence(ACTOR_ID), 1);
    assert_eq!(
        reopened.load_snapshot(ACTOR_ID).unwrap().state.get("value"),
        Some(&PersistedValue::Int(10))
    );
    assert_eq!(reopened.read_journal(ACTOR_ID).len(), 1);
    assert_eq!(reopened.read_workflow_events(ACTOR_ID).len(), 1);
    assert_eq!(reopened.read_events(ACTOR_ID).len(), 1);

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn json_atomic_retry_is_idempotent_and_conflict_fails_closed() {
    let dir = fresh_dir("idempotency");
    let mut store = JsonFileStore::new(&dir).unwrap();

    let original = transition(1, 0, 10, None);
    let first = store.commit_transition(original.clone()).unwrap();
    let retry = store.commit_transition(original).unwrap();
    assert_eq!(retry.digest, first.digest);

    let error = store
        .commit_transition(transition(1, 0, 99, None))
        .expect_err("conflicting same-sequence transition must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(store.latest_sequence(ACTOR_ID), 1);

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn json_durable_effect_recovery_returns_completed_receipt() {
    let dir = fresh_dir("effect");
    let mut store = JsonFileStore::new(&dir).unwrap();

    let prepared = DurableEffectRecord::prepare(effect_spec(), b"request");
    let effect_id = prepared.spec().id;
    store
        .commit_transition(transition(
            1,
            0,
            1,
            Some(DurableEffectPersistenceRecord::from_effect(
                prepared.clone(),
            )),
        ))
        .unwrap();

    let completed = prepared.complete(b"result".to_vec());
    store
        .commit_transition(transition(
            2,
            1,
            2,
            Some(DurableEffectPersistenceRecord::from_effect(
                completed.clone(),
            )),
        ))
        .unwrap();
    drop(store);

    let reopened = JsonFileStore::new(&dir).unwrap();
    let loaded = reopened
        .load_durable_effect(ACTOR_ID, effect_id)
        .unwrap()
        .expect("completed effect receipt must survive reopen");
    assert_eq!(loaded.effect(), &completed);

    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn json_atomic_store_recovers_from_torn_final_frame_before_next_commit() {
    let dir = fresh_dir("torn-tail");
    let mut store = JsonFileStore::new(&dir).unwrap();
    store.commit_transition(transition(1, 0, 10, None)).unwrap();
    drop(store);

    let log = dir
        .join(format!("actor_{ACTOR_ID}"))
        .join("transitions.log");
    let committed_len = std::fs::metadata(&log).unwrap().len();
    let mut file = OpenOptions::new().append(true).open(&log).unwrap();
    file.write_all(b"NDT1\0\0\0").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let mut reopened = JsonFileStore::new(&dir).unwrap();
    assert_eq!(
        reopened.latest_sequence(ACTOR_ID),
        1,
        "torn final frame must not advance durable history"
    );

    reopened
        .commit_transition(transition(2, 1, 20, None))
        .expect("next commit must truncate the proven torn tail and append cleanly");
    drop(reopened);

    let reopened = JsonFileStore::new(&dir).unwrap();
    assert_eq!(reopened.latest_sequence(ACTOR_ID), 2);
    assert!(std::fs::metadata(&log).unwrap().len() > committed_len);
    assert_eq!(
        reopened.load_snapshot(ACTOR_ID).unwrap().state.get("value"),
        Some(&PersistedValue::Int(20))
    );

    std::fs::remove_dir_all(dir).unwrap();
}
