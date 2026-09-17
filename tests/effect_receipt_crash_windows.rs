use std::collections::HashMap;

use nulang::effect_receipt::{
    EffectIdentity, EffectIntent, EffectInvocationId, EffectReceipt, EffectReplayDecision,
    EffectSiteId, RequestFingerprint,
};
use nulang::effect_receipt_fence::{
    EffectReceiptFence, FencedEffectReceiptError, InMemoryFencedEffectReceiptStore,
};

#[derive(Clone, Copy, Debug)]
struct TestFence {
    actor_id: u64,
    epoch: u64,
}

impl EffectReceiptFence for TestFence {
    fn actor_id(&self) -> u64 {
        self.actor_id
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProviderResult(Vec<u8>);

#[derive(Default)]
struct FakeIdempotentProvider {
    completed: HashMap<String, ProviderResult>,
    side_effect_count: usize,
}

impl FakeIdempotentProvider {
    fn execute(&mut self, key: &str, request: &[u8]) -> ProviderResult {
        if let Some(existing) = self.completed.get(key) {
            return existing.clone();
        }

        self.side_effect_count += 1;
        let result = ProviderResult([b"provider:".as_slice(), request].concat());
        self.completed.insert(key.to_string(), result.clone());
        result
    }
}

fn fixture(actor_id: u64) -> EffectIntent {
    let site = EffectSiteId::from_semantic_bytes(b"orders.Charge.capture#0");
    let owner = format!("actor:{actor_id}");
    let invocation = EffectInvocationId::derive(owner.as_bytes(), 7, site, 0);
    let identity = EffectIdentity::new("Payments", "charge").unwrap();
    let fingerprint = RequestFingerprint::from_canonical_bytes(b"order=42&amount=1000");
    let provider_key = Some(invocation.provider_idempotency_key("fake-payments-v1"));

    EffectIntent::new(invocation, site, identity, fingerprint, provider_key)
}

fn provider_key(intent: &EffectIntent) -> &str {
    intent
        .provider_idempotency_key
        .as_deref()
        .expect("fixture has provider key")
}

#[test]
fn crash_before_intent_commit_does_not_start_provider() {
    let actor_id = 42;
    let intent = fixture(actor_id);
    let fence = TestFence { actor_id, epoch: 1 };
    let mut receipts = InMemoryFencedEffectReceiptStore::default();
    let mut provider = FakeIdempotentProvider::default();

    assert_eq!(
        receipts.decide(actor_id, &intent).unwrap(),
        EffectReplayDecision::ExecuteNew
    );
    assert_eq!(provider.side_effect_count, 0);

    // Recovery starts from no durable intent. It must commit intent first.
    receipts
        .create_intent_fenced(actor_id, &fence, intent.clone())
        .unwrap();
    let result = provider.execute(provider_key(&intent), b"charge-order-42");
    let receipt = EffectReceipt::success(&intent, 1, result.0);
    receipts
        .commit_receipt_fenced(actor_id, &fence, receipt.clone())
        .unwrap();

    assert_eq!(provider.side_effect_count, 1);
    assert_eq!(
        receipts.decide(actor_id, &intent).unwrap(),
        EffectReplayDecision::ReturnReceipt(receipt)
    );
}

#[test]
fn crash_after_intent_before_provider_is_explicitly_indeterminate() {
    let actor_id = 42;
    let intent = fixture(actor_id);
    let fence = TestFence { actor_id, epoch: 1 };
    let mut receipts = InMemoryFencedEffectReceiptStore::default();
    let mut provider = FakeIdempotentProvider::default();

    receipts
        .create_intent_fenced(actor_id, &fence, intent.clone())
        .unwrap();

    assert_eq!(
        receipts.decide(actor_id, &intent).unwrap(),
        EffectReplayDecision::RecoverIndeterminate(intent.clone())
    );
    assert_eq!(provider.side_effect_count, 0);

    let result = provider.execute(provider_key(&intent), b"charge-order-42");
    receipts
        .commit_receipt_fenced(
            actor_id,
            &fence,
            EffectReceipt::success(&intent, 1, result.0),
        )
        .unwrap();

    assert_eq!(provider.side_effect_count, 1);
}

#[test]
fn crash_after_provider_success_before_receipt_reuses_provider_key() {
    let actor_id = 42;
    let intent = fixture(actor_id);
    let fence = TestFence { actor_id, epoch: 1 };
    let mut receipts = InMemoryFencedEffectReceiptStore::default();
    let mut provider = FakeIdempotentProvider::default();

    receipts
        .create_intent_fenced(actor_id, &fence, intent.clone())
        .unwrap();

    // Provider performs the external side effect, then the process crashes
    // before Nulang commits a receipt.
    let first = provider.execute(provider_key(&intent), b"charge-order-42");
    assert_eq!(provider.side_effect_count, 1);
    assert_eq!(
        receipts.decide(actor_id, &intent).unwrap(),
        EffectReplayDecision::RecoverIndeterminate(intent.clone())
    );

    // Recovery retries the *same logical invocation* using the persisted key.
    // The fake provider returns the existing result without a second side effect.
    let recovered = provider.execute(provider_key(&intent), b"charge-order-42");
    assert_eq!(recovered, first);
    assert_eq!(provider.side_effect_count, 1);

    let receipt = EffectReceipt::success(&intent, 2, recovered.0);
    receipts
        .commit_receipt_fenced(actor_id, &fence, receipt.clone())
        .unwrap();

    assert_eq!(
        receipts.decide(actor_id, &intent).unwrap(),
        EffectReplayDecision::ReturnReceipt(receipt)
    );
}

#[test]
fn crash_after_receipt_before_actor_resume_never_reinvokes_provider() {
    let actor_id = 42;
    let intent = fixture(actor_id);
    let fence = TestFence { actor_id, epoch: 1 };
    let mut receipts = InMemoryFencedEffectReceiptStore::default();
    let mut provider = FakeIdempotentProvider::default();

    receipts
        .create_intent_fenced(actor_id, &fence, intent.clone())
        .unwrap();
    let result = provider.execute(provider_key(&intent), b"charge-order-42");
    let receipt = EffectReceipt::success(&intent, 1, result.0);
    receipts
        .commit_receipt_fenced(actor_id, &fence, receipt.clone())
        .unwrap();

    assert_eq!(provider.side_effect_count, 1);

    // Recovery happens after the durable receipt but before user code observed
    // the return value. The runtime must replay the receipt, not the provider.
    match receipts.decide(actor_id, &intent).unwrap() {
        EffectReplayDecision::ReturnReceipt(replayed) => assert_eq!(replayed, receipt),
        other => panic!("expected terminal receipt replay, got {other:?}"),
    }
    assert_eq!(provider.side_effect_count, 1);
}

#[test]
fn failover_rejects_late_old_activation_receipt_without_duplicate_provider_effect() {
    let actor_id = 42;
    let intent = fixture(actor_id);
    let old = TestFence { actor_id, epoch: 1 };
    let current = TestFence { actor_id, epoch: 2 };
    let mut receipts = InMemoryFencedEffectReceiptStore::default();
    let mut provider = FakeIdempotentProvider::default();

    receipts
        .create_intent_fenced(actor_id, &old, intent.clone())
        .unwrap();

    // Old activation reaches the provider successfully.
    let old_result = provider.execute(provider_key(&intent), b"charge-order-42");
    assert_eq!(provider.side_effect_count, 1);

    // Failover occurs before the old activation can commit its receipt. The new
    // activation re-observes the same intent and advances durable authority.
    receipts
        .create_intent_fenced(actor_id, &current, intent.clone())
        .unwrap();

    let late_old_receipt = EffectReceipt::success(&intent, 1, old_result.0.clone());
    assert_eq!(
        receipts.commit_receipt_fenced(actor_id, &old, late_old_receipt),
        Err(FencedEffectReceiptError::StaleActivation {
            actor_id,
            presented_epoch: 1,
            current_epoch: 2,
        })
    );

    // The current activation resolves the indeterminate call through the same
    // provider key. The provider observes one logical effect, not two.
    let recovered = provider.execute(provider_key(&intent), b"charge-order-42");
    assert_eq!(recovered, old_result);
    assert_eq!(provider.side_effect_count, 1);

    let current_receipt = EffectReceipt::success(&intent, 2, recovered.0);
    receipts
        .commit_receipt_fenced(actor_id, &current, current_receipt.clone())
        .unwrap();

    assert_eq!(
        receipts.decide(actor_id, &intent).unwrap(),
        EffectReplayDecision::ReturnReceipt(current_receipt)
    );
    assert_eq!(provider.side_effect_count, 1);
}
