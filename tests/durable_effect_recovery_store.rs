use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::{DurableEffectPersistenceRecord, WorkflowEffectBinding};
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableTransition, JournalEntry, JsonFileStore, MemoryStore, PersistenceStore,
    WorkflowActivationId, DURABLE_TRANSITION_VERSION,
};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};

fn spec() -> DurableEffectSpec {
    let site = effect_site_id(
        "durable-effect-recovery-store-tests",
        EffectSiteOwnerKind::Behavior,
        "TestActor.run",
        "Provider.ask",
        0,
    );
    DurableEffectSpec::new(
        DurableEffectId::derive_from_site(42, "turn:7", site, 0),
        "Provider.ask",
        EffectBoundary::External,
        DeliverySemantics::AtLeastOnce,
    )
}

fn transition(
    store: &dyn PersistenceStore,
    actor_id: u64,
    epoch: u64,
    record: DurableEffectPersistenceRecord,
) -> DurableTransition {
    let previous = store.latest_sequence(actor_id);
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: epoch,
        sequence: previous + 1,
        expected_previous_sequence: previous,
        command: None,
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![record],
        outbox: vec![],
    }
}

#[test]
fn memory_store_loads_newest_record_for_logical_effect() {
    let mut store = MemoryStore::new();
    let prepared = DurableEffectRecord::prepare(spec(), b"request");
    let id = prepared.spec().id;

    let first = transition(
        &store,
        42,
        1,
        DurableEffectPersistenceRecord::from_effect(prepared.clone()),
    );
    store.commit_transition(first).unwrap();

    let completed = prepared.complete(b"result".to_vec());
    let second = transition(
        &store,
        42,
        1,
        DurableEffectPersistenceRecord::from_effect(completed.clone()),
    );
    store.commit_transition(second).unwrap();

    let loaded = store.load_durable_effect(42, id).unwrap().unwrap();
    assert_eq!(loaded.effect(), &completed);
}

#[test]
fn unsupported_store_fails_closed_instead_of_reporting_missing_effect() {
    let base = std::env::temp_dir().join(format!(
        "nulang_durable_effect_recovery_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let store = JsonFileStore::new(&base).unwrap();

    let error = store
        .load_durable_effect(42, spec().id)
        .expect_err("legacy non-atomic backend must not pretend recovery history is empty");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);

    assert_eq!(
        store
            .load_workflow_effect_occurrence(WorkflowActivationId::new(42, 1), 0)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_store_loads_newest_record_for_logical_effect() {
    use nulang::runtime::LibsqlStore;

    let mut store = LibsqlStore::in_memory().unwrap();
    let prepared = DurableEffectRecord::prepare(spec(), b"request");
    let id = prepared.spec().id;

    let first = transition(
        &store,
        42,
        1,
        DurableEffectPersistenceRecord::from_effect(prepared.clone()),
    );
    store.commit_transition(first).unwrap();

    let completed = prepared.complete(b"result".to_vec());
    let second = transition(
        &store,
        42,
        1,
        DurableEffectPersistenceRecord::from_effect(completed.clone()),
    );
    store.commit_transition(second).unwrap();

    let loaded = store.load_durable_effect(42, id).unwrap().unwrap();
    assert_eq!(loaded.effect(), &completed);
}

fn slot(occurrence: u32, site: u8) -> WorkflowEffectBinding {
    WorkflowEffectBinding {
        activation: WorkflowActivationId::new(42, 1),
        occurrence,
        site_id: [site; 32],
    }
}

fn bound_prepared(
    binding: WorkflowEffectBinding,
    request: &[u8],
) -> DurableEffectPersistenceRecord {
    let spec = DurableEffectSpec::new(
        binding.effect_id(),
        "Provider.ask",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    DurableEffectPersistenceRecord::from_workflow_occurrence(
        binding,
        DurableEffectRecord::prepare(spec, request),
    )
    .unwrap()
}

fn bound_completed(
    record: &DurableEffectPersistenceRecord,
    result: &[u8],
) -> DurableEffectPersistenceRecord {
    DurableEffectPersistenceRecord::from_workflow_occurrence(
        record.workflow_binding().unwrap(),
        record.effect().clone().complete(result.to_vec()),
    )
    .unwrap()
}

fn admit(store: &mut dyn PersistenceStore) {
    store
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: 42,
            activation_epoch: 1,
            sequence: 1,
            expected_previous_sequence: 0,
            command: Some(JournalEntry {
                sequence: 1,
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

fn assert_slot_contract(store: &mut dyn PersistenceStore) {
    admit(store);
    let binding = slot(0, 1);
    let prepared = bound_prepared(binding, b"request");
    let completed = bound_completed(&prepared, b"result");
    assert!(store
        .load_workflow_effect_occurrence(binding.activation, 0)
        .unwrap()
        .is_none());

    // No completion can invent an intent.
    let missing_intent = transition(store, 42, 1, completed.clone());
    assert!(store.commit_transition(missing_intent).is_err());
    assert_eq!(store.latest_sequence(42), 1);

    let skipped = transition(store, 42, 1, bound_prepared(slot(1, 1), b"request"));
    assert!(store.commit_transition(skipped).is_err());
    assert_eq!(store.latest_sequence(42), 1);
    let first = transition(store, 42, 1, prepared.clone());
    let initial = store.commit_transition(first.clone()).unwrap();
    assert_eq!(
        store.commit_transition(first).unwrap(),
        initial,
        "exact-tail retry remains idempotent"
    );
    assert_eq!(
        store
            .load_workflow_effect_occurrence(binding.activation, 0)
            .unwrap(),
        Some(prepared.clone())
    );
    // Even a caller that bypasses the adapter and picks the current
    // predecessor cannot bind a second semantic site to the same slot.
    let other = bound_prepared(slot(0, 2), b"request");
    let mut conflict = transition(store, 42, 1, other.clone());
    conflict.snapshot = Some(ActorSnapshot {
        actor_id: 42,
        sequence: 3,
        activation_epoch: 1,
        ..ActorSnapshot::default()
    });
    assert!(store.commit_transition(conflict).is_err());
    assert!(store.load_snapshot(42).is_none());
    assert!(store
        .load_durable_effect(42, other.effect().spec().id)
        .unwrap()
        .is_none());
    let skipped_pending = transition(store, 42, 1, bound_prepared(slot(1, 1), b"request"));
    assert!(store.commit_transition(skipped_pending).is_err());
    assert!(store
        .load_workflow_effect_occurrence(binding.activation, 1)
        .is_err());
    let changed_request = bound_completed(&bound_prepared(binding, b"changed"), b"result");
    let drift = transition(store, 42, 1, changed_request);
    assert!(store.commit_transition(drift).is_err());
    let duplicate = transition(store, 42, 1, prepared.clone());
    assert!(store.commit_transition(duplicate).is_err());
    assert_eq!(store.latest_sequence(42), 2);

    let finish = transition(store, 42, 1, completed.clone());
    store.commit_transition(finish).unwrap();
    assert_eq!(
        store
            .load_workflow_effect_occurrence(binding.activation, 0)
            .unwrap(),
        Some(completed.clone())
    );
    let changed_result = transition(store, 42, 1, bound_completed(&prepared, b"changed result"));
    assert!(store.commit_transition(changed_result).is_err());
    let later = bound_prepared(slot(1, 1), b"request");
    let later_transition = transition(store, 42, 1, later.clone());
    store.commit_transition(later_transition).unwrap();
    assert_eq!(
        store
            .load_workflow_effect_occurrence(binding.activation, 1)
            .unwrap(),
        Some(later)
    );
    assert_eq!(
        store
            .load_workflow_effect_occurrence(binding.activation, 0)
            .unwrap(),
        Some(completed)
    );
    assert_eq!(store.latest_sequence(42), 4);
}

#[test]
fn memory_workflow_slot_commit_is_immutable_and_atomic() {
    assert_slot_contract(&mut MemoryStore::new());
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_workflow_slot_commit_is_immutable_and_atomic() {
    assert_slot_contract(&mut nulang::runtime::LibsqlStore::in_memory().unwrap());
}

fn assert_unbound_history_rejected(store: &mut dyn PersistenceStore) {
    admit(store);
    let old = transition(
        store,
        42,
        1,
        DurableEffectPersistenceRecord::from_effect(
            bound_prepared(slot(0, 1), b"request").effect().clone(),
        ),
    );
    store.commit_transition(old).unwrap();
    let binding = slot(0, 2);
    assert_eq!(
        store
            .load_workflow_effect_occurrence(binding.activation, 0)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
    let new = transition(store, 42, 1, bound_prepared(binding, b"request"));
    assert!(store.commit_transition(new).is_err());
    assert_eq!(store.latest_sequence(42), 2);
}

#[test]
fn memory_old_unbound_workflow_history_fails_closed() {
    assert_unbound_history_rejected(&mut MemoryStore::new());
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_old_unbound_workflow_history_fails_closed() {
    assert_unbound_history_rejected(&mut nulang::runtime::LibsqlStore::in_memory().unwrap());
}

#[test]
fn workflow_binding_codec_keeps_v1_unchanged_and_validates_v2_identity() {
    let binding = slot(0, 1);
    let prepared = bound_prepared(binding, b"request");
    let completed = bound_completed(&prepared, b"result");
    for record in [prepared, completed] {
        let data = record.to_json().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert_eq!(value["version"], 2);
        assert_eq!(
            DurableEffectPersistenceRecord::from_json(&data).unwrap(),
            record
        );
        value["record"]["value"]["occurrence"] = 1.into();
        assert!(
            DurableEffectPersistenceRecord::from_json(&serde_json::to_vec(&value).unwrap())
                .is_err()
        );
        let mut value: serde_json::Value = serde_json::from_slice(&data).unwrap();
        value["version"] = 1.into();
        assert!(
            DurableEffectPersistenceRecord::from_json(&serde_json::to_vec(&value).unwrap())
                .is_err()
        );
        let legacy = DurableEffectPersistenceRecord::from_effect(record.effect().clone());
        let legacy_bytes = legacy.to_json().unwrap();
        let mut legacy_value: serde_json::Value = serde_json::from_slice(&legacy_bytes).unwrap();
        assert_eq!(legacy_value["version"], 1);
        assert_eq!(
            DurableEffectPersistenceRecord::from_json(&legacy_bytes).unwrap(),
            legacy
        );
        legacy_value["version"] = 2.into();
        assert!(DurableEffectPersistenceRecord::from_json(
            &serde_json::to_vec(&legacy_value).unwrap()
        )
        .is_err());
    }
}
