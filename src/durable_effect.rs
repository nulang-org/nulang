//! Durable side-effect identity and crash-recovery semantics.
//!
//! This module deliberately does **not** claim arbitrary exactly-once delivery.
//! A Nulang journal can make its own state transitions durable, but a crash can
//! always happen after an external system accepts a request and before Nulang
//! records the result. The semantic contract is therefore:
//!
//! 1. derive one stable [`DurableEffectId`] before dispatch;
//! 2. journal a [`DurableEffectRecord::Prepared`] with that ID;
//! 3. execute the operation, passing the same ID as the idempotency/dedup key
//!    whenever the dependency supports one;
//! 4. journal [`DurableEffectRecord::Completed`] with the result;
//! 5. on recovery, validate that the replayed request matches the journaled
//!    request before replaying a result or redispatching;
//! 6. if a completed operation must be undone, prepare an explicit
//!    [`DurableCompensationRecord`] rather than pretending the original effect
//!    was rollbackable.
//!
//! The runtime/workflow persistence layer can adopt this state machine without
//! changing its storage backend. Until that integration lands, these types pin
//! the contract and prevent future implementations from quietly upgrading an
//! at-least-once external call into an unsound "exactly once" promise.

use crate::primitives::{DeliverySemantics, EffectBoundary};
use blake3::Hasher;
use std::fmt;
use std::str::FromStr;

const EFFECT_ID_DOMAIN: &[u8] = b"nulang.durable-effect.v1\0";
const COMPENSATION_ID_DOMAIN: &[u8] = b"nulang.durable-compensation.v1\0";
const REQUEST_DIGEST_DOMAIN: &[u8] = b"nulang.durable-effect-request.v1\0";

/// Stable identity for one logical durable side effect.
///
/// Identity is derived from replay-stable inputs: the durable actor/workflow
/// identity, a caller-supplied execution key (for example a workflow step or
/// journal command key), the effect's ordinal within that execution, and the
/// qualified effect operation. Retries MUST reuse the same ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurableEffectId([u8; 32]);

impl DurableEffectId {
    pub fn derive(
        actor_id: u64,
        execution_key: &str,
        effect_ordinal: u32,
        effect_operation: &str,
    ) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(EFFECT_ID_DOMAIN);
        hasher.update(&actor_id.to_le_bytes());
        hash_len_prefixed(&mut hasher, execution_key.as_bytes());
        hasher.update(&effect_ordinal.to_le_bytes());
        hash_len_prefixed(&mut hasher, effect_operation.as_bytes());
        Self(*hasher.finalize().as_bytes())
    }

    /// Derive a stable logical operation ID for a compensation of this effect.
    ///
    /// Compensation identity is domain-separated from ordinary effect identity
    /// and includes the original effect ID, so compensating two otherwise
    /// identical operations cannot collide.
    pub fn derive_compensation(
        self,
        compensation_ordinal: u32,
        effect_operation: &str,
    ) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(COMPENSATION_ID_DOMAIN);
        hasher.update(&self.0);
        hasher.update(&compensation_ordinal.to_le_bytes());
        hash_len_prefixed(&mut hasher, effect_operation.as_bytes());
        Self(*hasher.finalize().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Canonical lowercase idempotency key suitable for an external API.
    pub fn idempotency_key(self) -> String {
        self.to_string()
    }
}

fn hash_len_prefixed(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn request_digest(request: &[u8]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(REQUEST_DIGEST_DOMAIN);
    hash_len_prefixed(&mut hasher, request);
    *hasher.finalize().as_bytes()
}

fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8; 32]) -> fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

impl fmt::Display for DurableEffectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableEffectIdParseError {
    InvalidLength { actual: usize },
    InvalidHex { index: usize },
}

impl fmt::Display for DurableEffectIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength { actual } => write!(
                f,
                "durable effect id must contain exactly 64 hex characters, got {actual}"
            ),
            Self::InvalidHex { index } => {
                write!(f, "invalid durable effect id hex at byte {index}")
            }
        }
    }
}

impl std::error::Error for DurableEffectIdParseError {}

impl FromStr for DurableEffectId {
    type Err = DurableEffectIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(DurableEffectIdParseError::InvalidLength {
                actual: value.len(),
            });
        }

        let bytes = value.as_bytes();
        let mut out = [0u8; 32];
        for (index, slot) in out.iter_mut().enumerate() {
            let hi = decode_hex(bytes[index * 2])
                .ok_or(DurableEffectIdParseError::InvalidHex { index: index * 2 })?;
            let lo = decode_hex(bytes[index * 2 + 1]).ok_or(
                DurableEffectIdParseError::InvalidHex {
                    index: index * 2 + 1,
                },
            )?;
            *slot = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

fn decode_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Replay-stable description of one durable effect operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableEffectSpec {
    pub id: DurableEffectId,
    pub effect_operation: String,
    pub boundary: EffectBoundary,
    pub delivery: DeliverySemantics,
}

impl DurableEffectSpec {
    pub fn new(
        id: DurableEffectId,
        effect_operation: impl Into<String>,
        boundary: EffectBoundary,
        delivery: DeliverySemantics,
    ) -> Self {
        Self {
            id,
            effect_operation: effect_operation.into(),
            boundary,
            delivery,
        }
    }
}

/// A journal lookup used the correct logical operation ID but supplied a
/// different request body. Recovery must fail closed rather than replaying a
/// recorded result or redispatching the old operation for the new request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableEffectRequestMismatch {
    pub expected_digest: [u8; 32],
    pub actual_digest: [u8; 32],
}

impl fmt::Display for DurableEffectRequestMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "durable effect request digest mismatch: expected ")?;
        write_hex(f, &self.expected_digest)?;
        write!(f, ", got ")?;
        write_hex(f, &self.actual_digest)
    }
}

impl std::error::Error for DurableEffectRequestMismatch {}

/// Durable journal state for one logical effect.
///
/// There is intentionally no durable `Dispatched` state. Recording such a
/// marker either before or after an external request merely moves the crash
/// window; it cannot prove whether the remote system committed the operation.
/// `Prepared` therefore means "completion is not durably known".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableEffectRecord {
    Prepared {
        spec: DurableEffectSpec,
        request_digest: [u8; 32],
    },
    Completed {
        spec: DurableEffectSpec,
        request_digest: [u8; 32],
        result: Vec<u8>,
    },
}

impl DurableEffectRecord {
    pub fn prepare(spec: DurableEffectSpec, request: &[u8]) -> Self {
        Self::Prepared {
            spec,
            request_digest: request_digest(request),
        }
    }

    pub fn spec(&self) -> &DurableEffectSpec {
        match self {
            Self::Prepared { spec, .. } | Self::Completed { spec, .. } => spec,
        }
    }

    pub fn request_digest(&self) -> &[u8; 32] {
        match self {
            Self::Prepared { request_digest, .. }
            | Self::Completed { request_digest, .. } => request_digest,
        }
    }

    /// Verify that recovery is replaying the exact request originally bound to
    /// this logical effect ID.
    pub fn validate_request(
        &self,
        request: &[u8],
    ) -> Result<(), DurableEffectRequestMismatch> {
        let actual_digest = request_digest(request);
        let expected_digest = *self.request_digest();
        if actual_digest == expected_digest {
            Ok(())
        } else {
            Err(DurableEffectRequestMismatch {
                expected_digest,
                actual_digest,
            })
        }
    }

    /// Commit the first durable result for this logical operation.
    ///
    /// Completion is monotonic. If recovery or a duplicate acknowledgement
    /// tries to complete an already-completed record again, the original
    /// durable result wins instead of being silently overwritten.
    pub fn complete(self, result: Vec<u8>) -> Self {
        match self {
            Self::Prepared {
                spec,
                request_digest,
            } => Self::Completed {
                spec,
                request_digest,
                result,
            },
            completed @ Self::Completed { .. } => completed,
        }
    }

    /// Decide what recovery is allowed to do after a crash.
    pub fn recovery_action(&self) -> DurableEffectRecoveryAction<'_> {
        match self {
            Self::Completed { result, .. } => {
                DurableEffectRecoveryAction::ReplayRecordedResult(result.as_slice())
            }
            Self::Prepared { spec, .. } => match spec.delivery {
                DeliverySemantics::BackendDefined => DurableEffectRecoveryAction::DelegateToBackend,
                DeliverySemantics::AtLeastOnce => {
                    DurableEffectRecoveryAction::RetryAtLeastOnce { operation_id: spec.id }
                }
                DeliverySemantics::EffectivelyOnceWithDeduplication => {
                    DurableEffectRecoveryAction::RetryWithDeduplication {
                        operation_id: spec.id,
                    }
                }
            },
        }
    }

    /// Validate replay request identity and then choose the recovery action.
    ///
    /// Persistence integrations SHOULD use this method instead of calling
    /// `recovery_action` directly when they still have the replayed request.
    pub fn recovery_action_for_request(
        &self,
        request: &[u8],
    ) -> Result<DurableEffectRecoveryAction<'_>, DurableEffectRequestMismatch> {
        self.validate_request(request)?;
        Ok(self.recovery_action())
    }

    /// Prepare an explicit compensation for a completed effect.
    ///
    /// A compensation is another durable effect with its own stable operation
    /// ID and recovery semantics. It can only be prepared after the original
    /// effect has a durable completion record; Nulang never models an external
    /// side effect as if it could be rolled back before its commit is known.
    pub fn prepare_compensation(
        &self,
        compensation_ordinal: u32,
        effect_operation: impl Into<String>,
        boundary: EffectBoundary,
        delivery: DeliverySemantics,
        request: &[u8],
    ) -> Result<DurableCompensationRecord, DurableCompensationError> {
        let original_effect_id = match self {
            Self::Completed { spec, .. } => spec.id,
            Self::Prepared { .. } => return Err(DurableCompensationError::OriginalNotCompleted),
        };
        let effect_operation = effect_operation.into();
        let compensation_id = original_effect_id
            .derive_compensation(compensation_ordinal, &effect_operation);
        let spec = DurableEffectSpec::new(
            compensation_id,
            effect_operation,
            boundary,
            delivery,
        );
        Ok(DurableCompensationRecord {
            original_effect_id,
            compensation_ordinal,
            effect: DurableEffectRecord::prepare(spec, request),
        })
    }
}

/// Recovery decision for a durable effect journal record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableEffectRecoveryAction<'a> {
    /// Completion is already journaled: return the recorded bytes and do not
    /// execute the side effect again.
    ReplayRecordedResult(&'a [u8]),
    /// Completion is unknown and duplicates are permitted by the declared
    /// contract. Retry the same logical operation ID.
    RetryAtLeastOnce { operation_id: DurableEffectId },
    /// Completion is unknown but the dependency supports deduplication. Retry
    /// with this exact stable operation ID as the idempotency key.
    RetryWithDeduplication { operation_id: DurableEffectId },
    /// The configured backend owns the recovery guarantee and must decide.
    DelegateToBackend,
}

/// Invalid durable compensation transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableCompensationError {
    /// The original effect has not durably completed, so compensation would
    /// pretend to undo work whose commit state is still unknown.
    OriginalNotCompleted,
}

impl fmt::Display for DurableCompensationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OriginalNotCompleted => write!(
                f,
                "cannot prepare compensation before the original durable effect completes"
            ),
        }
    }
}

impl std::error::Error for DurableCompensationError {}

/// Explicit durable compensation linked to one completed original effect.
///
/// The embedded effect record intentionally reuses the ordinary durable-effect
/// state machine. Compensation can itself crash after external commit, so it
/// requires the same stable identity, retry, deduplication, and replay rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableCompensationRecord {
    original_effect_id: DurableEffectId,
    compensation_ordinal: u32,
    effect: DurableEffectRecord,
}

impl DurableCompensationRecord {
    pub fn original_effect_id(&self) -> DurableEffectId {
        self.original_effect_id
    }

    pub fn compensation_ordinal(&self) -> u32 {
        self.compensation_ordinal
    }

    pub fn effect(&self) -> &DurableEffectRecord {
        &self.effect
    }

    pub fn compensation_id(&self) -> DurableEffectId {
        self.effect.spec().id
    }

    pub fn validate_request(
        &self,
        request: &[u8],
    ) -> Result<(), DurableEffectRequestMismatch> {
        self.effect.validate_request(request)
    }

    pub fn recovery_action(&self) -> DurableEffectRecoveryAction<'_> {
        self.effect.recovery_action()
    }

    pub fn recovery_action_for_request(
        &self,
        request: &[u8],
    ) -> Result<DurableEffectRecoveryAction<'_>, DurableEffectRequestMismatch> {
        self.effect.recovery_action_for_request(request)
    }

    /// Commit the first durable compensation result.
    pub fn complete(mut self, result: Vec<u8>) -> Self {
        self.effect = self.effect.complete(result);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(delivery: DeliverySemantics) -> DurableEffectSpec {
        DurableEffectSpec::new(
            DurableEffectId::derive(42, "charge-order-7", 0, "Payment.charge"),
            "Payment.charge",
            EffectBoundary::External,
            delivery,
        )
    }

    #[test]
    fn operation_id_is_stable_for_replay() {
        let first = DurableEffectId::derive(42, "charge-order-7", 0, "Payment.charge");
        let replay = DurableEffectId::derive(42, "charge-order-7", 0, "Payment.charge");
        assert_eq!(first, replay);
        assert_eq!(first.to_string().len(), 64);
        assert_eq!(first.to_string().parse::<DurableEffectId>().unwrap(), first);
    }

    #[test]
    fn operation_identity_changes_when_logical_effect_changes() {
        let base = DurableEffectId::derive(42, "step", 0, "Payment.charge");
        assert_ne!(
            base,
            DurableEffectId::derive(42, "step", 1, "Payment.charge")
        );
        assert_ne!(
            base,
            DurableEffectId::derive(42, "step", 0, "Email.send")
        );
        assert_ne!(
            base,
            DurableEffectId::derive(43, "step", 0, "Payment.charge")
        );
    }

    #[test]
    fn crash_after_prepare_retries_at_least_once_with_same_operation_id() {
        let record = DurableEffectRecord::prepare(spec(DeliverySemantics::AtLeastOnce), b"$10");
        let id = record.spec().id;
        assert_eq!(
            record.recovery_action_for_request(b"$10").unwrap(),
            DurableEffectRecoveryAction::RetryAtLeastOnce { operation_id: id }
        );
    }

    #[test]
    fn replay_with_different_request_fails_closed() {
        let record = DurableEffectRecord::prepare(
            spec(DeliverySemantics::EffectivelyOnceWithDeduplication),
            b"amount=10&currency=USD",
        );
        let err = record
            .recovery_action_for_request(b"amount=100&currency=USD")
            .unwrap_err();
        assert_ne!(err.expected_digest, err.actual_digest);
    }

    #[test]
    fn crash_after_external_dispatch_uses_same_deduplication_key() {
        // There is deliberately no `Dispatched` journal state. A crash after
        // the remote system commits but before Nulang records completion still
        // recovers from Prepared and MUST retry the same logical operation ID.
        let record = DurableEffectRecord::prepare(
            spec(DeliverySemantics::EffectivelyOnceWithDeduplication),
            b"$10",
        );
        let id = record.spec().id;
        assert_eq!(
            record.recovery_action_for_request(b"$10").unwrap(),
            DurableEffectRecoveryAction::RetryWithDeduplication { operation_id: id }
        );
        assert_eq!(id.idempotency_key(), record.spec().id.to_string());
    }

    #[test]
    fn completed_result_is_replayed_without_redispatch() {
        let record = DurableEffectRecord::prepare(
            spec(DeliverySemantics::EffectivelyOnceWithDeduplication),
            b"$10",
        )
        .complete(b"charged".to_vec());
        assert_eq!(
            record.recovery_action_for_request(b"$10").unwrap(),
            DurableEffectRecoveryAction::ReplayRecordedResult(b"charged")
        );
    }

    #[test]
    fn completed_result_is_not_replayed_for_a_different_request() {
        let record = DurableEffectRecord::prepare(
            spec(DeliverySemantics::EffectivelyOnceWithDeduplication),
            b"order=7&amount=10",
        )
        .complete(b"charged".to_vec());
        assert!(record
            .recovery_action_for_request(b"order=7&amount=11")
            .is_err());
    }

    #[test]
    fn completion_is_monotonic_and_cannot_replace_recorded_result() {
        let completed = DurableEffectRecord::prepare(
            spec(DeliverySemantics::EffectivelyOnceWithDeduplication),
            b"$10",
        )
        .complete(b"charged".to_vec())
        .complete(b"different-result".to_vec());
        assert_eq!(
            completed.recovery_action(),
            DurableEffectRecoveryAction::ReplayRecordedResult(b"charged")
        );
    }

    #[test]
    fn request_digest_is_preserved_when_result_commits() {
        let prepared = DurableEffectRecord::prepare(spec(DeliverySemantics::AtLeastOnce), b"body");
        let digest = *prepared.request_digest();
        let completed = prepared.complete(b"result".to_vec());
        assert_eq!(*completed.request_digest(), digest);
    }

    #[test]
    fn compensation_requires_a_completed_original_effect() {
        let prepared = DurableEffectRecord::prepare(
            spec(DeliverySemantics::EffectivelyOnceWithDeduplication),
            b"charge",
        );
        assert_eq!(
            prepared
                .prepare_compensation(
                    0,
                    "Payment.refund",
                    EffectBoundary::External,
                    DeliverySemantics::EffectivelyOnceWithDeduplication,
                    b"refund",
                )
                .unwrap_err(),
            DurableCompensationError::OriginalNotCompleted
        );
    }

    #[test]
    fn compensation_has_stable_distinct_identity_and_recovery_rules() {
        let completed = DurableEffectRecord::prepare(
            spec(DeliverySemantics::EffectivelyOnceWithDeduplication),
            b"charge",
        )
        .complete(b"charged".to_vec());
        let original_id = completed.spec().id;
        let compensation = completed
            .prepare_compensation(
                0,
                "Payment.refund",
                EffectBoundary::External,
                DeliverySemantics::EffectivelyOnceWithDeduplication,
                b"refund",
            )
            .unwrap();
        let compensation_id = compensation.compensation_id();
        assert_eq!(compensation.original_effect_id(), original_id);
        assert_eq!(compensation.compensation_ordinal(), 0);
        assert_ne!(compensation_id, original_id);
        assert_eq!(
            compensation
                .recovery_action_for_request(b"refund")
                .unwrap(),
            DurableEffectRecoveryAction::RetryWithDeduplication {
                operation_id: compensation_id,
            }
        );
    }

    #[test]
    fn completed_compensation_replays_without_redispatch() {
        let original = DurableEffectRecord::prepare(
            spec(DeliverySemantics::EffectivelyOnceWithDeduplication),
            b"charge",
        )
        .complete(b"charged".to_vec());
        let compensation = original
            .prepare_compensation(
                0,
                "Payment.refund",
                EffectBoundary::External,
                DeliverySemantics::EffectivelyOnceWithDeduplication,
                b"refund",
            )
            .unwrap()
            .complete(b"refunded".to_vec());
        assert_eq!(
            compensation
                .recovery_action_for_request(b"refund")
                .unwrap(),
            DurableEffectRecoveryAction::ReplayRecordedResult(b"refunded")
        );
    }

    #[test]
    fn backend_defined_recovery_is_not_strengthened_by_runtime() {
        let backend_spec = DurableEffectSpec::new(
            DurableEffectId::derive(9, "publish", 0, "Queue.publish"),
            "Queue.publish",
            EffectBoundary::BackendOwned,
            DeliverySemantics::BackendDefined,
        );
        let record = DurableEffectRecord::prepare(backend_spec, b"message");
        assert_eq!(
            record.recovery_action_for_request(b"message").unwrap(),
            DurableEffectRecoveryAction::DelegateToBackend
        );
    }
}
