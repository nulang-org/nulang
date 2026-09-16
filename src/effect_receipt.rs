//! Storage-neutral durable external-effect identity and receipt primitives.
//!
//! This module deliberately does not execute providers or persist data. It
//! defines the deterministic identity, intent/receipt records, and replay state
//! machine that persistence/provider integrations can build on.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Current serialized format version for effect intent/receipt records.
pub const EFFECT_RECEIPT_FORMAT_VERSION: u16 = 1;

const SITE_ID_DOMAIN: &[u8] = b"nulang.effect-site.v1";
const INVOCATION_ID_DOMAIN: &[u8] = b"nulang.effect-invocation.v1";
const REQUEST_FINGERPRINT_DOMAIN: &[u8] = b"nulang.effect-request.v1";
const PROVIDER_KEY_DOMAIN: &[u8] = b"nulang.effect-provider-key.v1";

/// Canonical compiler-owned identity for one semantic effect operation site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EffectSiteId([u8; 32]);

impl EffectSiteId {
    /// Derive an opaque site id from canonical semantic bytes supplied by the
    /// compiler. Source line/column should not be used as the canonical bytes.
    pub fn from_semantic_bytes(bytes: &[u8]) -> Self {
        Self(hash_parts(SITE_ID_DOMAIN, &[bytes]))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for EffectSiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// Stable logical identity for one durable external effect occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EffectInvocationId([u8; 32]);

impl EffectInvocationId {
    /// Derive an invocation id from durable semantic identity.
    ///
    /// Retries must pass the same inputs. Attempt number is intentionally not
    /// part of the identity.
    pub fn derive(
        durable_owner_identity: &[u8],
        durable_sequence: u64,
        site_id: EffectSiteId,
        occurrence_index: u32,
    ) -> Self {
        let sequence = durable_sequence.to_le_bytes();
        let occurrence = occurrence_index.to_le_bytes();
        Self(hash_parts(
            INVOCATION_ID_DOMAIN,
            &[
                durable_owner_identity,
                &sequence,
                site_id.as_bytes(),
                &occurrence,
            ],
        ))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Derive a stable opaque provider idempotency key for an adapter
    /// namespace. Concrete adapters may further encode/truncate this value if
    /// their provider has stricter key constraints, but must preserve stable
    /// mapping for an already-created intent.
    pub fn provider_idempotency_key(&self, adapter_namespace: &str) -> String {
        let digest = hash_parts(
            PROVIDER_KEY_DOMAIN,
            &[adapter_namespace.as_bytes(), self.as_bytes()],
        );
        format!("nula_eff_{}", hex::encode(digest))
    }
}

impl fmt::Display for EffectInvocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// Deterministic fingerprint of the canonical provider request semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestFingerprint([u8; 32]);

impl RequestFingerprint {
    pub fn from_canonical_bytes(bytes: &[u8]) -> Self {
        Self(hash_parts(REQUEST_FINGERPRINT_DOMAIN, &[bytes]))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for RequestFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// Canonical semantic effect + operation identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EffectIdentity {
    pub effect: String,
    pub operation: String,
}

impl EffectIdentity {
    pub fn new(
        effect: impl Into<String>,
        operation: impl Into<String>,
    ) -> Result<Self, EffectReceiptError> {
        let identity = Self {
            effect: effect.into(),
            operation: operation.into(),
        };
        identity.validate()?;
        Ok(identity)
    }

    fn validate(&self) -> Result<(), EffectReceiptError> {
        if self.effect.trim().is_empty() || self.operation.trim().is_empty() {
            return Err(EffectReceiptError::InvalidEffectIdentity);
        }
        Ok(())
    }
}

/// Persisted proof that one logical invocation was allocated before provider
/// execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectIntent {
    pub format_version: u16,
    pub invocation_id: EffectInvocationId,
    pub site_id: EffectSiteId,
    pub effect_identity: EffectIdentity,
    pub request_fingerprint: RequestFingerprint,
    pub provider_idempotency_key: Option<String>,
    pub first_attempt: u32,
}

impl EffectIntent {
    pub fn new(
        invocation_id: EffectInvocationId,
        site_id: EffectSiteId,
        effect_identity: EffectIdentity,
        request_fingerprint: RequestFingerprint,
        provider_idempotency_key: Option<String>,
    ) -> Self {
        Self {
            format_version: EFFECT_RECEIPT_FORMAT_VERSION,
            invocation_id,
            site_id,
            effect_identity,
            request_fingerprint,
            provider_idempotency_key,
            first_attempt: 1,
        }
    }

    pub fn validate(&self) -> Result<(), EffectReceiptError> {
        validate_format(self.format_version)?;
        self.effect_identity.validate()?;
        if self.first_attempt == 0 {
            return Err(EffectReceiptError::InvalidAttempt(0));
        }
        Ok(())
    }

    fn ensure_same_logical_request(&self, proposed: &Self) -> Result<(), EffectReceiptError> {
        self.validate()?;
        proposed.validate()?;

        if self.invocation_id != proposed.invocation_id {
            return Err(EffectReceiptError::InvocationMismatch);
        }
        if self.site_id != proposed.site_id {
            return Err(EffectReceiptError::SiteMismatch(self.invocation_id));
        }
        if self.effect_identity != proposed.effect_identity {
            return Err(EffectReceiptError::EffectIdentityMismatch(
                self.invocation_id,
            ));
        }
        if self.request_fingerprint != proposed.request_fingerprint {
            return Err(EffectReceiptError::RequestFingerprintMismatch(
                self.invocation_id,
            ));
        }
        if self.provider_idempotency_key != proposed.provider_idempotency_key {
            return Err(EffectReceiptError::ProviderKeyMismatch(self.invocation_id));
        }
        Ok(())
    }
}

/// Persistable terminal failure without assuming a language-level `Value`
/// representation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedEffectFailure {
    pub code: String,
    pub message: String,
}

/// Terminal provider outcome recorded for replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectOutcome {
    /// Provider result encoded by the concrete adapter's versioned codec.
    Succeeded(Vec<u8>),
    Failed(RecordedEffectFailure),
}

/// Durable terminal observation of one logical external effect invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectReceipt {
    pub format_version: u16,
    pub invocation_id: EffectInvocationId,
    pub effect_identity: EffectIdentity,
    pub request_fingerprint: RequestFingerprint,
    pub completed_attempt: u32,
    pub outcome: EffectOutcome,
}

impl EffectReceipt {
    pub fn success(intent: &EffectIntent, completed_attempt: u32, payload: Vec<u8>) -> Self {
        Self::from_outcome(intent, completed_attempt, EffectOutcome::Succeeded(payload))
    }

    pub fn failure(
        intent: &EffectIntent,
        completed_attempt: u32,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::from_outcome(
            intent,
            completed_attempt,
            EffectOutcome::Failed(RecordedEffectFailure {
                code: code.into(),
                message: message.into(),
            }),
        )
    }

    fn from_outcome(intent: &EffectIntent, completed_attempt: u32, outcome: EffectOutcome) -> Self {
        Self {
            format_version: EFFECT_RECEIPT_FORMAT_VERSION,
            invocation_id: intent.invocation_id,
            effect_identity: intent.effect_identity.clone(),
            request_fingerprint: intent.request_fingerprint,
            completed_attempt,
            outcome,
        }
    }

    pub fn validate_against(&self, intent: &EffectIntent) -> Result<(), EffectReceiptError> {
        validate_format(self.format_version)?;
        intent.validate()?;
        self.effect_identity.validate()?;

        if self.completed_attempt == 0 {
            return Err(EffectReceiptError::InvalidAttempt(0));
        }
        if self.invocation_id != intent.invocation_id {
            return Err(EffectReceiptError::InvocationMismatch);
        }
        if self.effect_identity != intent.effect_identity {
            return Err(EffectReceiptError::EffectIdentityMismatch(
                intent.invocation_id,
            ));
        }
        if self.request_fingerprint != intent.request_fingerprint {
            return Err(EffectReceiptError::RequestFingerprintMismatch(
                intent.invocation_id,
            ));
        }
        Ok(())
    }
}

/// Persisted state for one logical invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PersistedEffectState {
    Intent(EffectIntent),
    Completed {
        intent: EffectIntent,
        receipt: EffectReceipt,
    },
}

/// Deterministic decision made when durable execution reaches an effect site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectReplayDecision {
    /// No durable record exists. Persist an intent before provider execution.
    ExecuteNew,
    /// An intent exists without a terminal receipt. Recovery policy must decide
    /// whether to query/retry/compensate/fail closed.
    RecoverIndeterminate(EffectIntent),
    /// A compatible terminal receipt exists. Return it without provider
    /// execution.
    ReturnReceipt(EffectReceipt),
}

/// Validation/storage errors for the receipt state machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectReceiptError {
    UnsupportedFormat(u16),
    InvalidEffectIdentity,
    InvalidAttempt(u32),
    InvocationMismatch,
    SiteMismatch(EffectInvocationId),
    EffectIdentityMismatch(EffectInvocationId),
    RequestFingerprintMismatch(EffectInvocationId),
    ProviderKeyMismatch(EffectInvocationId),
    MissingIntent(EffectInvocationId),
    ConflictingReceipt(EffectInvocationId),
}

impl fmt::Display for EffectReceiptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat(version) => {
                write!(f, "unsupported effect receipt format version {version}")
            }
            Self::InvalidEffectIdentity => {
                f.write_str("effect and operation names must be non-empty")
            }
            Self::InvalidAttempt(attempt) => {
                write!(f, "effect attempt must be positive, got {attempt}")
            }
            Self::InvocationMismatch => f.write_str("effect invocation ids do not match"),
            Self::SiteMismatch(id) => write!(f, "effect site mismatch for invocation {id}"),
            Self::EffectIdentityMismatch(id) => {
                write!(f, "effect identity mismatch for invocation {id}")
            }
            Self::RequestFingerprintMismatch(id) => {
                write!(f, "request fingerprint mismatch for invocation {id}")
            }
            Self::ProviderKeyMismatch(id) => {
                write!(f, "provider idempotency-key mismatch for invocation {id}")
            }
            Self::MissingIntent(id) => write!(f, "missing effect intent for invocation {id}"),
            Self::ConflictingReceipt(id) => {
                write!(f, "conflicting terminal receipt for invocation {id}")
            }
        }
    }
}

impl std::error::Error for EffectReceiptError {}

/// In-memory reference implementation of the durable receipt state machine.
///
/// This is intentionally simple and single-process. Persistence backends should
/// reproduce its validation semantics while making intent/receipt writes atomic
/// with activation fencing.
#[derive(Clone, Debug, Default)]
pub struct InMemoryEffectReceiptStore {
    states: HashMap<EffectInvocationId, PersistedEffectState>,
}

impl InMemoryEffectReceiptStore {
    pub fn load(&self, invocation_id: EffectInvocationId) -> Option<&PersistedEffectState> {
        self.states.get(&invocation_id)
    }

    /// Decide what durable execution should do when it reaches `proposed`.
    pub fn decide(
        &self,
        proposed: &EffectIntent,
    ) -> Result<EffectReplayDecision, EffectReceiptError> {
        proposed.validate()?;

        match self.states.get(&proposed.invocation_id) {
            None => Ok(EffectReplayDecision::ExecuteNew),
            Some(PersistedEffectState::Intent(existing)) => {
                existing.ensure_same_logical_request(proposed)?;
                Ok(EffectReplayDecision::RecoverIndeterminate(existing.clone()))
            }
            Some(PersistedEffectState::Completed { intent, receipt }) => {
                intent.ensure_same_logical_request(proposed)?;
                receipt.validate_against(intent)?;
                Ok(EffectReplayDecision::ReturnReceipt(receipt.clone()))
            }
        }
    }

    /// Create an intent if absent. Repeating the identical write is idempotent;
    /// conflicting identity/request data fails closed.
    pub fn create_intent(&mut self, intent: EffectIntent) -> Result<(), EffectReceiptError> {
        intent.validate()?;

        match self.states.get(&intent.invocation_id) {
            None => {
                self.states
                    .insert(intent.invocation_id, PersistedEffectState::Intent(intent));
                Ok(())
            }
            Some(PersistedEffectState::Intent(existing)) => {
                existing.ensure_same_logical_request(&intent)
            }
            Some(PersistedEffectState::Completed {
                intent: existing, ..
            }) => existing.ensure_same_logical_request(&intent),
        }
    }

    /// Commit a terminal receipt for an existing intent. Repeating the exact
    /// same receipt is idempotent; replacing terminal history is forbidden.
    pub fn commit_receipt(&mut self, receipt: EffectReceipt) -> Result<(), EffectReceiptError> {
        validate_format(receipt.format_version)?;
        let invocation_id = receipt.invocation_id;

        match self.states.get(&invocation_id).cloned() {
            None => Err(EffectReceiptError::MissingIntent(invocation_id)),
            Some(PersistedEffectState::Intent(intent)) => {
                receipt.validate_against(&intent)?;
                self.states.insert(
                    invocation_id,
                    PersistedEffectState::Completed { intent, receipt },
                );
                Ok(())
            }
            Some(PersistedEffectState::Completed {
                intent,
                receipt: existing,
            }) => {
                existing.validate_against(&intent)?;
                receipt.validate_against(&intent)?;
                if existing == receipt {
                    Ok(())
                } else {
                    Err(EffectReceiptError::ConflictingReceipt(invocation_id))
                }
            }
        }
    }
}

fn validate_format(version: u16) -> Result<(), EffectReceiptError> {
    if version != EFFECT_RECEIPT_FORMAT_VERSION {
        return Err(EffectReceiptError::UnsupportedFormat(version));
    }
    Ok(())
}

fn hash_parts(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (EffectSiteId, EffectIntent) {
        let site = EffectSiteId::from_semantic_bytes(b"orders.Charge.capture#0");
        let invocation = EffectInvocationId::derive(b"order:42", 7, site, 0);
        let identity = EffectIdentity::new("Payments", "charge").unwrap();
        let fingerprint = RequestFingerprint::from_canonical_bytes(b"order=42&amount=1000");
        let provider_key = Some(invocation.provider_idempotency_key("test-payments-v1"));
        let intent = EffectIntent::new(invocation, site, identity, fingerprint, provider_key);
        (site, intent)
    }

    #[test]
    fn invocation_identity_is_deterministic_and_occurrence_sensitive() {
        let site = EffectSiteId::from_semantic_bytes(b"orders.Charge.capture#0");
        let first = EffectInvocationId::derive(b"order:42", 7, site, 0);
        let retry = EffectInvocationId::derive(b"order:42", 7, site, 0);
        let next_occurrence = EffectInvocationId::derive(b"order:42", 7, site, 1);

        assert_eq!(first, retry);
        assert_ne!(first, next_occurrence);
    }

    #[test]
    fn provider_idempotency_key_is_stable_and_namespaced() {
        let (_, intent) = fixture();
        let one = intent.invocation_id.provider_idempotency_key("payments-v1");
        let two = intent.invocation_id.provider_idempotency_key("payments-v1");
        let other_adapter = intent.invocation_id.provider_idempotency_key("mail-v1");

        assert_eq!(one, two);
        assert_ne!(one, other_adapter);
        assert!(one.starts_with("nula_eff_"));
    }

    #[test]
    fn absent_invocation_executes_new() {
        let (_, intent) = fixture();
        let store = InMemoryEffectReceiptStore::default();

        assert_eq!(
            store.decide(&intent).unwrap(),
            EffectReplayDecision::ExecuteNew
        );
    }

    #[test]
    fn intent_without_receipt_is_indeterminate_on_replay() {
        let (_, intent) = fixture();
        let mut store = InMemoryEffectReceiptStore::default();
        store.create_intent(intent.clone()).unwrap();

        assert_eq!(
            store.decide(&intent).unwrap(),
            EffectReplayDecision::RecoverIndeterminate(intent)
        );
    }

    #[test]
    fn identical_intent_write_is_idempotent() {
        let (_, intent) = fixture();
        let mut store = InMemoryEffectReceiptStore::default();

        store.create_intent(intent.clone()).unwrap();
        store.create_intent(intent).unwrap();
    }

    #[test]
    fn terminal_receipt_replays_without_new_execution() {
        let (_, intent) = fixture();
        let receipt = EffectReceipt::success(&intent, 1, b"provider-ok".to_vec());
        let mut store = InMemoryEffectReceiptStore::default();

        store.create_intent(intent.clone()).unwrap();
        store.commit_receipt(receipt.clone()).unwrap();

        assert_eq!(
            store.decide(&intent).unwrap(),
            EffectReplayDecision::ReturnReceipt(receipt)
        );
    }

    #[test]
    fn identical_terminal_receipt_write_is_idempotent() {
        let (_, intent) = fixture();
        let receipt = EffectReceipt::success(&intent, 2, b"provider-ok".to_vec());
        let mut store = InMemoryEffectReceiptStore::default();

        store.create_intent(intent).unwrap();
        store.commit_receipt(receipt.clone()).unwrap();
        store.commit_receipt(receipt).unwrap();
    }

    #[test]
    fn conflicting_terminal_receipt_fails_closed() {
        let (_, intent) = fixture();
        let first = EffectReceipt::success(&intent, 1, b"first".to_vec());
        let second = EffectReceipt::success(&intent, 2, b"second".to_vec());
        let mut store = InMemoryEffectReceiptStore::default();

        store.create_intent(intent).unwrap();
        store.commit_receipt(first).unwrap();

        assert!(matches!(
            store.commit_receipt(second),
            Err(EffectReceiptError::ConflictingReceipt(_))
        ));
    }

    #[test]
    fn request_fingerprint_mismatch_fails_closed() {
        let (_, intent) = fixture();
        let mut store = InMemoryEffectReceiptStore::default();
        store.create_intent(intent.clone()).unwrap();

        let mut conflicting = intent;
        conflicting.request_fingerprint =
            RequestFingerprint::from_canonical_bytes(b"order=42&amount=9999");

        assert!(matches!(
            store.decide(&conflicting),
            Err(EffectReceiptError::RequestFingerprintMismatch(_))
        ));
    }

    #[test]
    fn receipt_without_intent_is_rejected() {
        let (_, intent) = fixture();
        let receipt = EffectReceipt::failure(&intent, 1, "provider", "declined");
        let mut store = InMemoryEffectReceiptStore::default();

        assert!(matches!(
            store.commit_receipt(receipt),
            Err(EffectReceiptError::MissingIntent(_))
        ));
    }

    #[test]
    fn unsupported_record_format_is_rejected() {
        let (_, mut intent) = fixture();
        intent.format_version = EFFECT_RECEIPT_FORMAT_VERSION + 1;
        let store = InMemoryEffectReceiptStore::default();

        assert_eq!(
            store.decide(&intent),
            Err(EffectReceiptError::UnsupportedFormat(
                EFFECT_RECEIPT_FORMAT_VERSION + 1
            ))
        );
    }
}
