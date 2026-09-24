use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableTransition, EventEntry, JournalEntry, MemoryStore, PersistenceStore,
    WorkflowEvent, DURABLE_TRANSITION_VERSION,
};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};

#[derive(Default)]
struct UnsupportedEffectStore {
    inner: MemoryStore,
}

impl PersistenceStore for UnsupportedEffectStore {
    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> std::io::Result<()> {
        self.inner.save_snapshot(snapshot)
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        self.inner.load_snapshot(actor_id)
    }

    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> std::io::Result<()> {
        self.inner.append_journal(actor_id, entry)
    }

    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
        self.inner.read_journal(actor_id)
    }

    fn append_workflow_event(
        &mut self,
        actor_id: u64,
        event: WorkflowEvent,
    ) -> std::io::Result<()> {
        self.inner.append_workflow_event(actor_id, event)
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        self.inner.read_workflow_events(actor_id)
    }

    fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> std::io::Result<()> {
        self.inner.append_event(actor_id, entry)
    }

    fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
        self.inner.read_events(actor_id)
    }

    fn latest_sequence(&self, actor_id: u64) -> u64 {
        self.inner.latest_sequence(actor_id)
    }

    fn clear(&mut self, actor_id: u64) -> std::io::Result<()> {
        self.inner.clear(actor_id)
    }
}

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
        inbox: vec![],
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
    let store = UnsupportedEffectStore::default();
    let error = store
        .load_durable_effect(42, spec().id)
        .expect_err("backend without durable-effect recovery must fail closed");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
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
