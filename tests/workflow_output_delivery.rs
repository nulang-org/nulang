//! Durable delivery consumes validated workflow receipts, not ambient emits.
//! A provider MUST honor the operation ID to prevent duplicate external work.
use nulang::runtime::{
    MemoryStore, PersistedValue, PersistenceStore, WorkflowActivationId, WorkflowEvent,
    WorkflowReplayEventId,
};
use nulang::workflow_output::{read_workflow_output_receipts, WorkflowOutputReceipt};
use nulang::workflow_output_delivery::{
    deliver_output_with_deduplication, WorkflowOutputDeliveryError,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::rc::Rc;

const ACTOR: u64 = 52;
fn fixture() -> (MemoryStore, WorkflowOutputReceipt) {
    let mut store = MemoryStore::new();
    store
        .append_workflow_event(
            ACTOR,
            WorkflowEvent::Custom {
                sequence: 1,
                replay_id: Some(WorkflowReplayEventId::new(
                    WorkflowActivationId::new(ACTOR, 17),
                    0,
                )),
                name: "WorkflowOutput".into(),
                args: vec![PersistedValue::String("invoice-ready".into())],
            },
        )
        .unwrap();
    let receipt = read_workflow_output_receipts(&store, ACTOR, 0, 1)
        .unwrap()
        .remove(0);
    (store, receipt)
}

#[test]
fn prepared_is_committed_before_external_dispatch_and_completed_receipt_replays() {
    let (mut store, receipt) = fixture();
    let calls = Cell::new(0);
    let original = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |key, destination, bytes| {
            calls.set(calls.get() + 1);
            assert_eq!(key, receipt.idempotency_key());
            assert_eq!(destination, "webhook:billing");
            assert!(std::str::from_utf8(bytes)
                .unwrap()
                .contains("invoice-ready"));
            Ok(b"provider-accepted".to_vec())
        },
    )
    .unwrap();
    assert_eq!(original, b"provider-accepted");
    assert_eq!(calls.get(), 1);
    let after_restart = store.clone();
    let mut recovered = after_restart;
    let replay = deliver_output_with_deduplication(
        &mut recovered,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |_, _, _| panic!("completed receipt must not dispatch"),
    )
    .unwrap();
    assert_eq!(replay, original);
    assert_eq!(calls.get(), 1);
    assert_eq!(
        recovered.latest_sequence(ACTOR),
        store.latest_sequence(ACTOR)
    );
}

#[test]
fn accepted_then_lost_reply_retries_same_key_without_duplicate_side_effect() {
    let (mut store, receipt) = fixture();
    let accepted = Rc::new(RefCell::new(HashMap::<String, Vec<u8>>::new()));
    let actual_operations = Cell::new(0);
    let sink = |key: &str| {
        let mut map = accepted.borrow_mut();
        if let Some(existing) = map.get(key) {
            return existing.clone();
        }
        actual_operations.set(actual_operations.get() + 1);
        map.insert(key.to_string(), b"accepted".to_vec());
        b"accepted".to_vec()
    };
    let first = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |key, _, _| {
            sink(key);
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "response lost after provider accepted",
            ))
        },
    );
    assert!(matches!(
        first,
        Err(WorkflowOutputDeliveryError::Provider(_))
    ));
    assert_eq!(actual_operations.get(), 1);
    let entry = store
        .load_durable_effect(ACTOR, receipt.durable_effect_id())
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            entry.effect(),
            nulang::durable_effect::DurableEffectRecord::Prepared { .. }
        ),
        "provider failure must leave prepared intent for replay"
    );
    let prepared = store.clone(); // simulated restart after provider accept, before receipt completion
    let mut recovered = prepared;
    let result = deliver_output_with_deduplication(
        &mut recovered,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |key, _, _| Ok(sink(key)),
    )
    .unwrap();
    assert_eq!(result, b"accepted");
    assert_eq!(
        actual_operations.get(),
        1,
        "provider honoring the key must not duplicate its side effect"
    );
    assert_eq!(accepted.borrow().len(), 1);
    let entry = recovered
        .load_durable_effect(ACTOR, receipt.durable_effect_id())
        .unwrap()
        .unwrap();
    assert!(matches!(
        entry.effect(),
        nulang::durable_effect::DurableEffectRecord::Completed { .. }
    ));
}

#[test]
fn provider_cannot_run_if_journal_receipt_is_missing_or_forged() {
    let (mut store, mut receipt) = fixture();
    receipt.value = PersistedValue::String("forged".into());
    let called = Cell::new(false);
    let err = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |_, _, _| {
            called.set(true);
            Ok(vec![])
        },
    )
    .unwrap_err();
    assert!(matches!(err, WorkflowOutputDeliveryError::ReceiptMismatch));
    assert!(!called.get());
    assert_eq!(store.latest_sequence(ACTOR), 1);
}

#[test]
fn destination_drift_is_rejected_before_redispatch() {
    let (mut store, receipt) = fixture();
    let _ = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |_, _, _| Err(io::Error::other("network down")),
    );
    let called = Cell::new(false);
    let err = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:other",
        |_, _, _| {
            called.set(true);
            Ok(vec![])
        },
    )
    .unwrap_err();
    assert!(matches!(err, WorkflowOutputDeliveryError::Recovery(_)));
    assert!(!called.get());
}

#[test]
fn empty_destination_is_rejected_without_preparation_or_dispatch() {
    let (mut store, receipt) = fixture();
    let called = Cell::new(false);
    let err = deliver_output_with_deduplication(&mut store, ACTOR, 1, &receipt, "", |_, _, _| {
        called.set(true);
        Ok(vec![])
    })
    .unwrap_err();
    assert!(matches!(
        err,
        WorkflowOutputDeliveryError::InvalidDestination
    ));
    assert!(!called.get());
    assert_eq!(store.latest_sequence(ACTOR), 1);
}

#[test]
fn completed_receipt_does_not_authorize_changed_destination_or_payload() {
    let (mut store, receipt) = fixture();
    deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |_, _, _| Ok(b"ok".to_vec()),
    )
    .unwrap();
    let drifted = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:other",
        |_, _, _| panic!("must not send"),
    )
    .unwrap_err();
    assert!(matches!(drifted, WorkflowOutputDeliveryError::Recovery(_)));
}

#[test]
fn stale_activation_cannot_redispatch_prepared_output() {
    use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
    use nulang::durable_effect_runtime::DurableEffectCoordinator;
    use nulang::primitives::{DeliverySemantics, EffectBoundary};
    let (mut store, receipt) = fixture();
    let first = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |_, _, _| Err(io::Error::other("network timeout")),
    );
    assert!(matches!(
        first,
        Err(WorkflowOutputDeliveryError::Provider(_))
    ));
    let spec = DurableEffectSpec::new(
        DurableEffectId::derive(ACTOR, "new-epoch", 0, "Other.test"),
        "Other.test",
        EffectBoundary::External,
        DeliverySemantics::AtLeastOnce,
    );
    DurableEffectCoordinator::new(&mut store, ACTOR, 2)
        .begin(spec, b"claim")
        .unwrap();
    let called = Cell::new(false);
    let error = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |_, _, _| {
            called.set(true);
            Ok(vec![])
        },
    )
    .unwrap_err();
    assert!(matches!(error, WorkflowOutputDeliveryError::Recovery(_)));
    assert!(!called.get());
}

#[test]
fn unsupported_atomic_store_rejects_prepare_before_provider_call() {
    use nulang::runtime::JsonFileStore;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};
    let uniq = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "nulang-output-unsupported-{uniq}-{}",
        std::process::id()
    ));
    let mut store = JsonFileStore::new(&root).unwrap();
    let (_memory, receipt) = fixture();
    store
        .append_workflow_event(
            ACTOR,
            WorkflowEvent::Custom {
                sequence: receipt.sequence,
                replay_id: Some(receipt.replay_id),
                name: "WorkflowOutput".into(),
                args: vec![receipt.value.clone()],
            },
        )
        .unwrap();
    let called = Cell::new(false);
    let error = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |_, _, _| {
            called.set(true);
            Ok(vec![])
        },
    )
    .unwrap_err();
    assert!(matches!(error, WorkflowOutputDeliveryError::Recovery(_)));
    assert!(
        !called.get(),
        "no atomic Prepared support means no provider dispatch"
    );
    fs::remove_dir_all(root).unwrap();
}

/// A fault-injection store whose Prepared writes succeed but Completed writes
/// fail: models a crash after provider acceptance before local acknowledgement.
struct FailCompletion {
    inner: MemoryStore,
    reject_completion: bool,
}

impl PersistenceStore for FailCompletion {
    fn commit_transition(
        &mut self,
        transition: nulang::runtime::DurableTransition,
    ) -> io::Result<nulang::runtime::DurableCommit> {
        if self.reject_completion
            && transition.durable_effects.iter().any(|record| {
                matches!(
                    record.effect(),
                    nulang::durable_effect::DurableEffectRecord::Completed { .. }
                )
            })
        {
            return Err(io::Error::other("injected crash before Completed commit"));
        }
        self.inner.commit_transition(transition)
    }
    fn load_durable_effect(
        &self,
        actor_id: u64,
        effect_id: nulang::durable_effect::DurableEffectId,
    ) -> io::Result<Option<nulang::durable_effect_persistence::DurableEffectPersistenceRecord>>
    {
        self.inner.load_durable_effect(actor_id, effect_id)
    }
    fn load_durable_tail_position(
        &self,
        actor_id: u64,
    ) -> io::Result<Option<nulang::runtime::DurableTailPosition>> {
        self.inner.load_durable_tail_position(actor_id)
    }
    fn save_snapshot(&mut self, snapshot: nulang::runtime::ActorSnapshot) -> io::Result<()> {
        self.inner.save_snapshot(snapshot)
    }
    fn load_snapshot(&self, actor_id: u64) -> Option<nulang::runtime::ActorSnapshot> {
        self.inner.load_snapshot(actor_id)
    }
    fn append_journal(
        &mut self,
        actor_id: u64,
        entry: nulang::runtime::JournalEntry,
    ) -> io::Result<()> {
        self.inner.append_journal(actor_id, entry)
    }
    fn read_journal(&self, actor_id: u64) -> Vec<nulang::runtime::JournalEntry> {
        self.inner.read_journal(actor_id)
    }
    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()> {
        self.inner.append_workflow_event(actor_id, event)
    }
    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        self.inner.read_workflow_events(actor_id)
    }
    fn try_read_workflow_events(&self, actor_id: u64) -> io::Result<Vec<WorkflowEvent>> {
        self.inner.try_read_workflow_events(actor_id)
    }
    fn append_event(
        &mut self,
        actor_id: u64,
        entry: nulang::runtime::EventEntry,
    ) -> io::Result<()> {
        self.inner.append_event(actor_id, entry)
    }
    fn read_events(&self, actor_id: u64) -> Vec<nulang::runtime::EventEntry> {
        self.inner.read_events(actor_id)
    }
    fn latest_sequence(&self, actor_id: u64) -> u64 {
        self.inner.latest_sequence(actor_id)
    }
    fn clear(&mut self, actor_id: u64) -> io::Result<()> {
        self.inner.clear(actor_id)
    }
}

#[test]
fn provider_success_but_completed_commit_failure_remains_prepared_and_retries() {
    let (inner, receipt) = fixture();
    let mut store = FailCompletion {
        inner,
        reject_completion: true,
    };
    let calls = Cell::new(0);
    let first = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |key, _, _| {
            calls.set(calls.get() + 1);
            assert_eq!(key, receipt.idempotency_key());
            Ok(b"remote-receipt".to_vec())
        },
    );
    assert!(matches!(
        first,
        Err(WorkflowOutputDeliveryError::Recovery(_))
    ));
    assert_eq!(calls.get(), 1);
    let stored = store
        .load_durable_effect(ACTOR, receipt.durable_effect_id())
        .unwrap()
        .unwrap();
    assert!(matches!(
        stored.effect(),
        nulang::durable_effect::DurableEffectRecord::Prepared { .. }
    ));
    // Provider promises it will deduplicate the same idempotency key. The
    // already accepted external operation has not been rolled back.
    let mut after_crash = store.inner.clone();
    let recovered = deliver_output_with_deduplication(
        &mut after_crash,
        ACTOR,
        1,
        &receipt,
        "webhook:billing",
        |key, _, _| {
            assert_eq!(key, receipt.idempotency_key());
            Ok(b"remote-receipt".to_vec())
        },
    )
    .unwrap();
    assert_eq!(recovered, b"remote-receipt");
    let stored = after_crash
        .load_durable_effect(ACTOR, receipt.durable_effect_id())
        .unwrap()
        .unwrap();
    assert!(matches!(
        stored.effect(),
        nulang::durable_effect::DurableEffectRecord::Completed { .. }
    ));
}
