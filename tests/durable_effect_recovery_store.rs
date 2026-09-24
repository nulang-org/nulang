use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    DurableTransition, JsonFileStore, MemoryStore, PersistenceStore, DURABLE_TRANSITION_VERSION,
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
fn json_file_store_loads_newest_record_for_logical_effect() {
    let base = std::env::temp_dir().join(format!(
        "nulang_durable_effect_recovery_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let mut store = JsonFileStore::new(&base).unwrap();
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
