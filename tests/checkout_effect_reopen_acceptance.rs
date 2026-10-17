//! Checkout acceptance slice for the durable-effect substrate.
//!
//! Exercises the same payment/reservation identity and receipt contract that a
//! combined job queue + workflow engine should provide. This is a persistence
//! integration test, NOT a hosted Nulang Cloud / WASM / process-kill proof.

#![cfg(feature = "sqlite")]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
use nulang::durable_effect_runtime::{
    DurableEffectCoordinator, DurableEffectDispatchDecision, DurableEffectRuntimeError,
};
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{LibsqlStore, PersistenceStore};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};

const CHECKOUT_ACTOR_ID: u64 = 4200;
const EPOCH: u64 = 1;

fn effect(order: &str, operation: &str) -> DurableEffectSpec {
    let site = effect_site_id(
        "checkout-consolidation-acceptance",
        EffectSiteOwnerKind::Behavior,
        "Checkout.process",
        operation,
        0,
    );
    DurableEffectSpec::new(
        DurableEffectId::derive_from_site(CHECKOUT_ACTOR_ID, order, site, 0),
        operation,
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    )
}

#[derive(Default)]
struct IdempotentProvider {
    committed: BTreeMap<String, Vec<u8>>,
    calls: usize,
    mutations: usize,
}

impl IdempotentProvider {
    fn call(&mut self, id: DurableEffectId, request: &[u8]) -> Vec<u8> {
        self.calls += 1;
        let key = id.idempotency_key();
        if let Some(recorded) = self.committed.get(&key) {
            return recorded.clone();
        }
        self.mutations += 1;
        let response = format!(
            "provider-ack:{}",
            String::from_utf8_lossy(request)
        ).into_bytes();
        self.committed.insert(key, response.clone());
        response
    }
}

fn isolated_db() -> (PathBuf, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-checkout-proof-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("effects.db");
    (dir, db)
}

#[test]
fn checkout_recovers_two_effects_across_reopen_without_duplicate_external_mutations() {
    let (dir, db) = isolated_db();
    let reserve = effect("order-42", "Inventory.reserve");
    let charge = effect("order-42", "Payment.charge");
    assert_ne!(reserve.id, charge.id);

    let inventory_request = b"order=42&sku=billboard-7";
    let payment_request = b"order=42&amount_cents=1000";
    let mut provider = IdempotentProvider::default();

    // Incarnation 1: inventory commits its receipt. Payment commits at the
    // provider, but the runtime terminates before its terminal receipt.
    let first_payment_result = {
        let mut store = LibsqlStore::new(&db).unwrap();
        let mut coordinator =
            DurableEffectCoordinator::new(&mut store, CHECKOUT_ACTOR_ID, EPOCH);

        assert_eq!(
            coordinator.begin(reserve.clone(), inventory_request).unwrap(),
            DurableEffectDispatchDecision::DispatchWithDeduplication {
                operation_id: reserve.id
            }
        );
        let inventory_result = provider.call(reserve.id, inventory_request);
        assert_eq!(
            coordinator
                .complete(reserve.id, inventory_request, inventory_result.clone())
                .unwrap(),
            inventory_result
        );

        assert_eq!(
            coordinator.begin(charge.clone(), payment_request).unwrap(),
            DurableEffectDispatchDecision::DispatchWithDeduplication {
                operation_id: charge.id
            }
        );
        let payment_result = provider.call(charge.id, payment_request);
        assert_eq!(store.latest_sequence(CHECKOUT_ACTOR_ID), 3);
        payment_result
    };

    // Incarnation 2: committed inventory must replay; the prepared payment
    // retries with the SAME idempotency identity, and the provider dedups it.
    {
        let mut store = LibsqlStore::new(&db).unwrap();
        let mut coordinator =
            DurableEffectCoordinator::new(&mut store, CHECKOUT_ACTOR_ID, EPOCH);
        assert!(matches!(
            coordinator.begin(reserve.clone(), inventory_request).unwrap(),
            DurableEffectDispatchDecision::ReplayRecordedResult(_)
        ));
        assert_eq!(
            coordinator.begin(charge.clone(), payment_request).unwrap(),
            DurableEffectDispatchDecision::DispatchWithDeduplication {
                operation_id: charge.id
            }
        );

        let retry_result = provider.call(charge.id, payment_request);
        assert_eq!(retry_result, first_payment_result);
        assert_eq!(
            coordinator
                .complete(charge.id, payment_request, retry_result)
                .unwrap(),
            first_payment_result
        );
        assert_eq!(store.latest_sequence(CHECKOUT_ACTOR_ID), 4);
    }

    // Incarnation 3: both receipts replay, with zero further provider calls.
    {
        let mut store = LibsqlStore::new(&db).unwrap();
        let mut coordinator =
            DurableEffectCoordinator::new(&mut store, CHECKOUT_ACTOR_ID, EPOCH);
        assert!(matches!(
            coordinator.begin(reserve, inventory_request).unwrap(),
            DurableEffectDispatchDecision::ReplayRecordedResult(_)
        ));
        assert_eq!(
            coordinator.begin(charge.clone(), payment_request).unwrap(),
            DurableEffectDispatchDecision::ReplayRecordedResult(first_payment_result.clone())
        );
        // Changing the logical request with an unchanged effect identity must
        // fail closed before the external provider can be invoked.
        assert!(matches!(
            coordinator.begin(charge, b"order=42&amount_cents=9999"),
            Err(DurableEffectRuntimeError::RequestMismatch(_))
        ));
        assert_eq!(store.latest_sequence(CHECKOUT_ACTOR_ID), 4);
    }

    assert_eq!(provider.calls, 3, "one deduplicated payment retry is expected");
    assert_eq!(
        provider.mutations, 2,
        "inventory and payment must each commit at the provider exactly once"
    );
    fs::remove_dir_all(dir).unwrap();
}
