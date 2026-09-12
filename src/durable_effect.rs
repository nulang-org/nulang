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
//! 5. on recovery, replay a completed result without redispatch, otherwise
//!    follow the declared [`DeliverySemantics`].
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

impl fmt::Display for DurableEffectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
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
            request_digest: *blake3::hash(request).as_bytes(),
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

    /// Commit a result for the already-prepared logical operation.
    pub fn complete(self, result: Vec<u8>) -> Self {
        match self {
            Self::Prepared {
                spec,
                request_digest,
            }
            | Self::Completed {
                spec,
                request_digest,
                ..
            } => Self::Completed {
                spec,
                request_digest,
                result,
            },
        }
    }

    /// Decide what recovery is allowed to do after a crash.
    pub fn recovery_action(&self) -> DurableEffectRecoveryAction<'_> {
        match self {
            Self::Completed { result, .. } => {
                DurableEffectRecoveryAction::ReplayRecordedResult(result.as_slice())
            }
            Self::Prepared { spec, .. } => match (spec.boundary, spec.delivery) {
                (EffectBoundary::BackendOwned, DeliverySemantics::BackendDefined)
                | (_, DeliverySemantics::BackendDefined) => {
                    DurableEffectRecoveryAction::DelegateToBackend
                }
                (_, DeliverySemantics::AtLeastOnce) => {
                    DurableEffectRecoveryAction::RetryAtLeastOnce { operation_id: spec.id }
                }
                (_, DeliverySemantics::EffectivelyOnceWithDeduplication) => {
                    DurableEffectRecoveryAction::RetryWithDeduplication {
                        operation_id: spec.id,
                    }
                }
            },
        }
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
            record.recovery_action(),
            DurableEffectRecoveryAction::RetryAtLeastOnce { operation_id: id }
        );
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
            record.recovery_action(),
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
            record.recovery_action(),
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
    fn backend_defined_recovery_is_not_strengthened_by_runtime() {
        let backend_spec = DurableEffectSpec::new(
            DurableEffectId::derive(9, "publish", 0, "Queue.publish"),
            "Queue.publish",
            EffectBoundary::BackendOwned,
            DeliverySemantics::BackendDefined,
        );
        let record = DurableEffectRecord::prepare(backend_spec, b"message");
        assert_eq!(
            record.recovery_action(),
            DurableEffectRecoveryAction::DelegateToBackend
        );
    }
}
