use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
use nulang::durable_effect_runtime::{
    DurableEffectCoordinator, DurableEffectDispatchDecision, DurableEffectRuntimeError,
};
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{MemoryStore, PersistenceStore};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};
use std::collections::BTreeMap;

const ACTOR_ID: u64 = 7;

fn effect_spec(
    execution_key: &str,
    operation: &str,
    boundary: EffectBoundary,
    delivery: DeliverySemantics,
) -> DurableEffectSpec {
    let site = effect_site_id(
        "durable-effect-failure-matrix",
        EffectSiteOwnerKind::Behavior,
        "Checkout.run",
        operation,
        0,
    );
    DurableEffectSpec::new(
        DurableEffectId::derive_from_site(ACTOR_ID, execution_key, site, 0),
        operation,
        boundary,
        delivery,
    )
}

#[derive(Default)]
struct DeduplicatingProvider {
    committed: BTreeMap<String, Vec<u8>>,
    mutation_count: usize,
    call_count: usize,
}

impl DeduplicatingProvider {
    fn execute(&mut self, operation_id: DurableEffectId, request: &[u8]) -> Vec<u8> {
        self.call_count += 1;
        let key = operation_id.idempotency_key();
        if let Some(result) = self.committed.get(&key) {
            return result.clone();
        }

        self.mutation_count += 1;
        let result =
            format!("provider-result:{}", String::from_utf8_lossy(request)).into_bytes();
        self.committed.insert(key, result.clone());
        result
    }
}

#[derive(Default)]
struct AtLeastOnceProvider {
    mutation_count: usize,
}

impl AtLeastOnceProvider {
    fn execute(&mut self, request: &[u8]) -> Vec<u8> {
        self.mutation_count += 1;
        format!(
            "provider-result-{}:{}",
            self.mutation_count,
            String::from_utf8_lossy(request)
        )
        .into_bytes()
    }
}

#[test]
fn crash_after_intent_before_provider_retries_same_semantic_operation() {
    let request = b"order=42&amount=1000";
    let spec = effect_spec(
        "checkout/order-42",
        "Payment.charge",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    let operation_id = spec.id;
    let mut store = MemoryStore::new();

    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        assert_eq!(
            coordinator.begin(spec.clone(), request).unwrap(),
            DurableEffectDispatchDecision::DispatchWithDeduplication { operation_id }
        );
    }
    assert_eq!(store.latest_sequence(ACTOR_ID), 1);

    let mut provider = DeduplicatingProvider::default();
    {
        let mut recovered = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        assert_eq!(
            recovered.begin(spec.clone(), request).unwrap(),
            DurableEffectDispatchDecision::DispatchWithDeduplication { operation_id }
        );
        let result = provider.execute(operation_id, request);
        assert_eq!(
            recovered
                .complete(operation_id, request, result.clone())
                .unwrap(),
            result
        );
    }

    assert_eq!(provider.call_count, 1);
    assert_eq!(provider.mutation_count, 1);
    assert_eq!(store.latest_sequence(ACTOR_ID), 2);

    let mut restarted = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert!(matches!(
        restarted.begin(spec, request).unwrap(),
        DurableEffectDispatchDecision::ReplayRecordedResult(_)
    ));
    assert_eq!(provider.call_count, 1);
}

#[test]
fn crash_after_provider_commit_before_receipt_is_effectively_once_with_provider_dedup() {
    let request = b"order=42&amount=1000";
    let spec = effect_spec(
        "checkout/order-42",
        "Payment.charge",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    let operation_id = spec.id;
    let mut store = MemoryStore::new();
    let mut provider = DeduplicatingProvider::default();

    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        assert_eq!(
            coordinator.begin(spec.clone(), request).unwrap(),
            DurableEffectDispatchDecision::DispatchWithDeduplication { operation_id }
        );
    }

    // Provider commits, but the process dies before Nulang persists Completed.
    let first_result = provider.execute(operation_id, request);
    assert_eq!(provider.call_count, 1);
    assert_eq!(provider.mutation_count, 1);
    assert_eq!(store.latest_sequence(ACTOR_ID), 1);

    {
        let mut recovered = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        assert_eq!(
            recovered.begin(spec.clone(), request).unwrap(),
            DurableEffectDispatchDecision::DispatchWithDeduplication { operation_id }
        );

        let recovered_result = provider.execute(operation_id, request);
        assert_eq!(recovered_result, first_result);
        assert_eq!(provider.call_count, 2);
        assert_eq!(provider.mutation_count, 1);

        recovered
            .complete(operation_id, request, recovered_result)
            .unwrap();
    }

    let mut restarted = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert_eq!(
        restarted.begin(spec, request).unwrap(),
        DurableEffectDispatchDecision::ReplayRecordedResult(first_result)
    );
    assert_eq!(store.latest_sequence(ACTOR_ID), 2);
}

#[test]
fn crash_after_receipt_before_actor_resume_never_redispatches_provider() {
    let request = b"order=42&amount=1000";
    let spec = effect_spec(
        "checkout/order-42",
        "Payment.charge",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    let operation_id = spec.id;
    let mut store = MemoryStore::new();
    let mut provider = DeduplicatingProvider::default();

    let result = {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        coordinator.begin(spec.clone(), request).unwrap();
        let result = provider.execute(operation_id, request);
        coordinator
            .complete(operation_id, request, result.clone())
            .unwrap();
        result
    };

    let mut restarted = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert_eq!(
        restarted.begin(spec, request).unwrap(),
        DurableEffectDispatchDecision::ReplayRecordedResult(result)
    );
    assert_eq!(provider.call_count, 1);
    assert_eq!(provider.mutation_count, 1);
}

#[test]
fn request_drift_on_replay_fails_closed_before_provider_dispatch() {
    let spec = effect_spec(
        "checkout/order-42",
        "Payment.charge",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    let mut store = MemoryStore::new();

    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        coordinator
            .begin(spec.clone(), b"order=42&amount=1000")
            .unwrap();
    }

    let provider = DeduplicatingProvider::default();
    let mut restarted = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert!(matches!(
        restarted.begin(spec, b"order=42&amount=9999"),
        Err(DurableEffectRuntimeError::RequestMismatch(_))
    ));
    assert_eq!(provider.call_count, 0);
    assert_eq!(provider.mutation_count, 0);
    assert_eq!(store.latest_sequence(ACTOR_ID), 1);
}

#[test]
fn specification_drift_on_replay_fails_closed_before_provider_dispatch() {
    let original = effect_spec(
        "checkout/order-42",
        "Payment.charge",
        EffectBoundary::External,
        DeliverySemantics::AtLeastOnce,
    );
    let operation_id = original.id;
    let mut store = MemoryStore::new();

    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        coordinator.begin(original, b"order=42&amount=1000").unwrap();
    }

    let drifted = DurableEffectSpec::new(
        operation_id,
        "Payment.charge",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    let mut restarted = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert!(matches!(
        restarted.begin(drifted, b"order=42&amount=1000"),
        Err(DurableEffectRuntimeError::SpecMismatch { effect_id, .. })
            if effect_id == operation_id
    ));
    assert_eq!(store.latest_sequence(ACTOR_ID), 1);
}

#[test]
fn at_least_once_contract_exposes_duplicate_external_mutation() {
    let request = b"email=customer@example.test";
    let spec = effect_spec(
        "notification/order-42",
        "Email.send",
        EffectBoundary::External,
        DeliverySemantics::AtLeastOnce,
    );
    let operation_id = spec.id;
    let mut store = MemoryStore::new();
    let mut provider = AtLeastOnceProvider::default();

    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        assert_eq!(
            coordinator.begin(spec.clone(), request).unwrap(),
            DurableEffectDispatchDecision::DispatchAtLeastOnce { operation_id }
        );
    }
    let _first_result = provider.execute(request);

    {
        let mut restarted = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        assert_eq!(
            restarted.begin(spec, request).unwrap(),
            DurableEffectDispatchDecision::DispatchAtLeastOnce { operation_id }
        );
    }
    let _second_result = provider.execute(request);

    assert_eq!(provider.mutation_count, 2);
    assert_eq!(store.latest_sequence(ACTOR_ID), 1);
}

#[test]
fn backend_defined_semantics_remain_delegated_after_restart() {
    let request = b"message";
    let spec = effect_spec(
        "publish/topic-a",
        "Queue.publish",
        EffectBoundary::BackendOwned,
        DeliverySemantics::BackendDefined,
    );
    let operation_id = spec.id;
    let mut store = MemoryStore::new();

    {
        let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
        assert_eq!(
            coordinator.begin(spec.clone(), request).unwrap(),
            DurableEffectDispatchDecision::DelegateToBackend { operation_id }
        );
    }

    let mut restarted = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert_eq!(
        restarted.begin(spec, request).unwrap(),
        DurableEffectDispatchDecision::DelegateToBackend { operation_id }
    );
    assert_eq!(store.latest_sequence(ACTOR_ID), 1);
}
