//! Activation-fenced reference storage for durable effect intents and receipts.
//!
//! This module composes the receipt state machine with activation authority
//! without defining a second production activation-fence type. Production
//! persistence adapters should implement [`EffectReceiptFence`] for the shared
//! runtime activation-fence primitive and perform the fence check plus receipt
//! mutation in one backend transaction/batch/CAS boundary.

use std::collections::HashMap;
use std::fmt;

use crate::effect_receipt::{
    EffectIntent, EffectReceipt, EffectReceiptError, EffectReplayDecision,
    InMemoryEffectReceiptStore, PersistedEffectState,
};

/// Minimal authority surface required by effect-receipt persistence.
///
/// The runtime's canonical activation-fence type should implement this trait
/// once that type is available on the same stabilization baseline. The trait is
/// deliberately tiny so this module does not duplicate routing/failover logic.
pub trait EffectReceiptFence {
    /// Durable actor namespace owned by this activation.
    fn actor_id(&self) -> u64;

    /// Monotonically increasing activation epoch. Epoch zero is invalid.
    fn epoch(&self) -> u64;
}

/// One authoritative durable mutation to an effect-receipt namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectReceiptMutation {
    /// Create the intent if absent; an identical intent is idempotent.
    CreateIntent(EffectIntent),
    /// Commit a terminal receipt for a pre-existing compatible intent.
    CommitReceipt(EffectReceipt),
}

/// Result of validating activation authority for a receipt mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectReceiptFenceDecision {
    /// No receipt namespace has committed under an activation epoch yet.
    Initialize { actor_id: u64, epoch: u64 },
    /// The writer still owns the currently accepted epoch.
    Current { actor_id: u64, epoch: u64 },
    /// A newly authoritative activation advanced the accepted epoch.
    Advance {
        actor_id: u64,
        previous_epoch: u64,
        epoch: u64,
    },
}

/// Errors raised before an authoritative receipt mutation can commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FencedEffectReceiptError {
    ZeroEpoch {
        actor_id: u64,
    },
    NamespaceMismatch {
        namespace_actor_id: u64,
        presented_actor_id: u64,
    },
    StaleActivation {
        actor_id: u64,
        presented_epoch: u64,
        current_epoch: u64,
    },
    Receipt(EffectReceiptError),
}

impl From<EffectReceiptError> for FencedEffectReceiptError {
    fn from(value: EffectReceiptError) -> Self {
        Self::Receipt(value)
    }
}

impl fmt::Display for FencedEffectReceiptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroEpoch { actor_id } => write!(
                f,
                "effect receipt activation epoch must be >= 1 for actor {actor_id}"
            ),
            Self::NamespaceMismatch {
                namespace_actor_id,
                presented_actor_id,
            } => write!(
                f,
                "effect receipt namespace mismatch: namespace actor {namespace_actor_id}, presented actor {presented_actor_id}"
            ),
            Self::StaleActivation {
                actor_id,
                presented_epoch,
                current_epoch,
            } => write!(
                f,
                "stale effect receipt write rejected for actor {actor_id}: presented epoch {presented_epoch}, current epoch {current_epoch}"
            ),
            Self::Receipt(error) => write!(f, "effect receipt mutation rejected: {error}"),
        }
    }
}

impl std::error::Error for FencedEffectReceiptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Receipt(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct ReceiptNamespace {
    accepted_epoch: u64,
    receipts: InMemoryEffectReceiptStore,
}

/// In-memory reference implementation of activation-fenced receipt storage.
///
/// This type is an executable specification for backend adapters, not a
/// production persistence implementation. It intentionally applies mutations to
/// a cloned receipt state machine and publishes both the mutation and accepted
/// epoch only after all validation succeeds. That models the atomic boundary a
/// real backend must provide:
///
/// 1. read/lock the canonical activation fence;
/// 2. reject a stale or wrong-namespace writer;
/// 3. validate the effect intent/receipt transition;
/// 4. apply the receipt mutation;
/// 5. initialize/advance the activation fence when required;
/// 6. commit those changes atomically.
///
/// A backend that checks the epoch in memory and then performs an unrelated
/// unconditional write does **not** satisfy this contract.
#[derive(Clone, Debug, Default)]
pub struct InMemoryFencedEffectReceiptStore {
    namespaces: HashMap<u64, ReceiptNamespace>,
}

impl InMemoryFencedEffectReceiptStore {
    /// Read the current receipt state without mutating activation authority.
    pub fn load(
        &self,
        namespace_actor_id: u64,
        invocation_id: crate::effect_receipt::EffectInvocationId,
    ) -> Option<&PersistedEffectState> {
        self.namespaces
            .get(&namespace_actor_id)
            .and_then(|namespace| namespace.receipts.load(invocation_id))
    }

    /// Replay decision for a proposed logical invocation.
    ///
    /// Reads do not advance or validate activation authority. Authority is
    /// checked at each authoritative write boundary.
    pub fn decide(
        &self,
        namespace_actor_id: u64,
        proposed: &EffectIntent,
    ) -> Result<EffectReplayDecision, EffectReceiptError> {
        match self.namespaces.get(&namespace_actor_id) {
            Some(namespace) => namespace.receipts.decide(proposed),
            None => {
                proposed.validate()?;
                Ok(EffectReplayDecision::ExecuteNew)
            }
        }
    }

    /// Last activation epoch accepted by this reference receipt namespace.
    pub fn accepted_epoch(&self, namespace_actor_id: u64) -> Option<u64> {
        self.namespaces
            .get(&namespace_actor_id)
            .map(|namespace| namespace.accepted_epoch)
    }

    /// Apply one fenced receipt mutation atomically in the reference model.
    pub fn apply_fenced<F: EffectReceiptFence>(
        &mut self,
        namespace_actor_id: u64,
        fence: &F,
        mutation: EffectReceiptMutation,
    ) -> Result<EffectReceiptFenceDecision, FencedEffectReceiptError> {
        let decision = self.evaluate_fence(namespace_actor_id, fence)?;

        // Work on a detached copy. If receipt validation fails, neither the
        // mutation nor an epoch initialization/advance becomes visible.
        let mut next_receipts = self
            .namespaces
            .get(&namespace_actor_id)
            .map(|namespace| namespace.receipts.clone())
            .unwrap_or_default();

        match mutation {
            EffectReceiptMutation::CreateIntent(intent) => next_receipts.create_intent(intent)?,
            EffectReceiptMutation::CommitReceipt(receipt) => {
                next_receipts.commit_receipt(receipt)?
            }
        }

        self.namespaces.insert(
            namespace_actor_id,
            ReceiptNamespace {
                accepted_epoch: fence.epoch(),
                receipts: next_receipts,
            },
        );

        Ok(decision)
    }

    pub fn create_intent_fenced<F: EffectReceiptFence>(
        &mut self,
        namespace_actor_id: u64,
        fence: &F,
        intent: EffectIntent,
    ) -> Result<EffectReceiptFenceDecision, FencedEffectReceiptError> {
        self.apply_fenced(
            namespace_actor_id,
            fence,
            EffectReceiptMutation::CreateIntent(intent),
        )
    }

    pub fn commit_receipt_fenced<F: EffectReceiptFence>(
        &mut self,
        namespace_actor_id: u64,
        fence: &F,
        receipt: EffectReceipt,
    ) -> Result<EffectReceiptFenceDecision, FencedEffectReceiptError> {
        self.apply_fenced(
            namespace_actor_id,
            fence,
            EffectReceiptMutation::CommitReceipt(receipt),
        )
    }

    fn evaluate_fence<F: EffectReceiptFence>(
        &self,
        namespace_actor_id: u64,
        fence: &F,
    ) -> Result<EffectReceiptFenceDecision, FencedEffectReceiptError> {
        let actor_id = fence.actor_id();
        let epoch = fence.epoch();

        if epoch == 0 {
            return Err(FencedEffectReceiptError::ZeroEpoch { actor_id });
        }

        if actor_id != namespace_actor_id {
            return Err(FencedEffectReceiptError::NamespaceMismatch {
                namespace_actor_id,
                presented_actor_id: actor_id,
            });
        }

        let Some(namespace) = self.namespaces.get(&namespace_actor_id) else {
            return Ok(EffectReceiptFenceDecision::Initialize { actor_id, epoch });
        };

        if epoch < namespace.accepted_epoch {
            return Err(FencedEffectReceiptError::StaleActivation {
                actor_id,
                presented_epoch: epoch,
                current_epoch: namespace.accepted_epoch,
            });
        }

        if epoch == namespace.accepted_epoch {
            return Ok(EffectReceiptFenceDecision::Current { actor_id, epoch });
        }

        Ok(EffectReceiptFenceDecision::Advance {
            actor_id,
            previous_epoch: namespace.accepted_epoch,
            epoch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect_receipt::{
        EffectIdentity, EffectInvocationId, EffectSiteId, RequestFingerprint,
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

    fn fixture(actor_id: u64) -> EffectIntent {
        let site = EffectSiteId::from_semantic_bytes(b"orders.Charge.capture#0");
        let owner = format!("actor:{actor_id}");
        let invocation = EffectInvocationId::derive(owner.as_bytes(), 7, site, 0);
        let identity = EffectIdentity::new("Payments", "charge").unwrap();
        let fingerprint = RequestFingerprint::from_canonical_bytes(b"order=42&amount=1000");
        let provider_key = Some(invocation.provider_idempotency_key("test-payments-v1"));
        EffectIntent::new(invocation, site, identity, fingerprint, provider_key)
    }

    #[test]
    fn first_authoritative_write_initializes_namespace_epoch() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let fence = TestFence { actor_id, epoch: 1 };
        let mut store = InMemoryFencedEffectReceiptStore::default();

        assert_eq!(
            store
                .create_intent_fenced(actor_id, &fence, intent)
                .unwrap(),
            EffectReceiptFenceDecision::Initialize { actor_id, epoch: 1 }
        );
        assert_eq!(store.accepted_epoch(actor_id), Some(1));
    }

    #[test]
    fn same_epoch_can_commit_terminal_receipt() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let receipt = EffectReceipt::success(&intent, 1, b"provider-ok".to_vec());
        let fence = TestFence { actor_id, epoch: 3 };
        let mut store = InMemoryFencedEffectReceiptStore::default();

        store
            .create_intent_fenced(actor_id, &fence, intent.clone())
            .unwrap();
        assert_eq!(
            store
                .commit_receipt_fenced(actor_id, &fence, receipt.clone())
                .unwrap(),
            EffectReceiptFenceDecision::Current { actor_id, epoch: 3 }
        );
        assert_eq!(
            store.decide(actor_id, &intent).unwrap(),
            EffectReplayDecision::ReturnReceipt(receipt)
        );
    }

    #[test]
    fn higher_epoch_advances_only_when_mutation_commits() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let epoch_one = TestFence { actor_id, epoch: 1 };
        let epoch_two = TestFence { actor_id, epoch: 2 };
        let mut store = InMemoryFencedEffectReceiptStore::default();

        store
            .create_intent_fenced(actor_id, &epoch_one, intent.clone())
            .unwrap();

        let receipt = EffectReceipt::success(&intent, 1, b"ok".to_vec());
        assert_eq!(
            store
                .commit_receipt_fenced(actor_id, &epoch_two, receipt)
                .unwrap(),
            EffectReceiptFenceDecision::Advance {
                actor_id,
                previous_epoch: 1,
                epoch: 2,
            }
        );
        assert_eq!(store.accepted_epoch(actor_id), Some(2));
    }

    #[test]
    fn stale_activation_cannot_commit_receipt_after_failover() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let old = TestFence { actor_id, epoch: 1 };
        let current = TestFence { actor_id, epoch: 2 };
        let mut store = InMemoryFencedEffectReceiptStore::default();

        store
            .create_intent_fenced(actor_id, &old, intent.clone())
            .unwrap();

        // The new activation advances authority with an idempotent re-observation
        // of the same intent before the old activation returns from its provider.
        assert_eq!(
            store
                .create_intent_fenced(actor_id, &current, intent.clone())
                .unwrap(),
            EffectReceiptFenceDecision::Advance {
                actor_id,
                previous_epoch: 1,
                epoch: 2,
            }
        );

        let stale_receipt = EffectReceipt::success(&intent, 1, b"old-result".to_vec());
        assert_eq!(
            store.commit_receipt_fenced(actor_id, &old, stale_receipt),
            Err(FencedEffectReceiptError::StaleActivation {
                actor_id,
                presented_epoch: 1,
                current_epoch: 2,
            })
        );

        assert_eq!(
            store.decide(actor_id, &intent).unwrap(),
            EffectReplayDecision::RecoverIndeterminate(intent)
        );
    }

    #[test]
    fn wrong_actor_namespace_fails_before_mutation() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let wrong = TestFence {
            actor_id: 99,
            epoch: 1,
        };
        let mut store = InMemoryFencedEffectReceiptStore::default();

        assert_eq!(
            store.create_intent_fenced(actor_id, &wrong, intent.clone()),
            Err(FencedEffectReceiptError::NamespaceMismatch {
                namespace_actor_id: actor_id,
                presented_actor_id: 99,
            })
        );
        assert_eq!(store.accepted_epoch(actor_id), None);
        assert_eq!(
            store.decide(actor_id, &intent).unwrap(),
            EffectReplayDecision::ExecuteNew
        );
    }

    #[test]
    fn zero_epoch_fails_before_mutation() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let zero = TestFence { actor_id, epoch: 0 };
        let mut store = InMemoryFencedEffectReceiptStore::default();

        assert_eq!(
            store.create_intent_fenced(actor_id, &zero, intent.clone()),
            Err(FencedEffectReceiptError::ZeroEpoch { actor_id })
        );
        assert_eq!(store.accepted_epoch(actor_id), None);
        assert_eq!(
            store.decide(actor_id, &intent).unwrap(),
            EffectReplayDecision::ExecuteNew
        );
    }

    #[test]
    fn failed_receipt_validation_does_not_advance_epoch() {
        let actor_id = 42;
        let intent = fixture(actor_id);
        let first = TestFence { actor_id, epoch: 1 };
        let next = TestFence { actor_id, epoch: 2 };
        let mut store = InMemoryFencedEffectReceiptStore::default();

        store
            .create_intent_fenced(actor_id, &first, intent.clone())
            .unwrap();

        let mut incompatible = EffectReceipt::success(&intent, 1, b"bad".to_vec());
        incompatible.request_fingerprint =
            RequestFingerprint::from_canonical_bytes(b"different-request");

        assert!(matches!(
            store.commit_receipt_fenced(actor_id, &next, incompatible),
            Err(FencedEffectReceiptError::Receipt(
                EffectReceiptError::RequestFingerprintMismatch(_)
            ))
        ));
        assert_eq!(store.accepted_epoch(actor_id), Some(1));
        assert_eq!(
            store.decide(actor_id, &intent).unwrap(),
            EffectReplayDecision::RecoverIndeterminate(intent)
        );
    }

    #[test]
    fn stale_writer_cannot_create_new_intent() {
        let actor_id = 42;
        let first_intent = fixture(actor_id);
        let epoch_one = TestFence { actor_id, epoch: 1 };
        let epoch_two = TestFence { actor_id, epoch: 2 };
        let mut store = InMemoryFencedEffectReceiptStore::default();

        store
            .create_intent_fenced(actor_id, &epoch_one, first_intent.clone())
            .unwrap();
        store
            .create_intent_fenced(actor_id, &epoch_two, first_intent)
            .unwrap();

        let site = EffectSiteId::from_semantic_bytes(b"orders.Notify.send#0");
        let invocation = EffectInvocationId::derive(b"actor:42", 8, site, 0);
        let second_intent = EffectIntent::new(
            invocation,
            site,
            EffectIdentity::new("Mail", "send").unwrap(),
            RequestFingerprint::from_canonical_bytes(b"message=receipt"),
            Some(invocation.provider_idempotency_key("test-mail-v1")),
        );

        assert!(matches!(
            store.create_intent_fenced(actor_id, &epoch_one, second_intent.clone()),
            Err(FencedEffectReceiptError::StaleActivation { .. })
        ));
        assert_eq!(
            store.decide(actor_id, &second_intent).unwrap(),
            EffectReplayDecision::ExecuteNew
        );
    }
}
