//! Versioned persistence envelope for durable external-effect records.
//!
//! The semantic types in [`crate::durable_effect`] intentionally do not derive
//! `Serialize`/`Deserialize`: their in-memory representation may evolve without
//! silently changing the durable wire format. This module is the explicit
//! compatibility boundary for journal/storage integrations.

use crate::durable_effect::{
    DurableCompensationRecord, DurableEffectId, DurableEffectRecord, DurableEffectSpec,
};
use crate::primitives::{DeliverySemantics, EffectBoundary};
use std::fmt;
use std::str::FromStr;

pub const DURABLE_EFFECT_PERSISTENCE_VERSION: u16 = 1;

/// A durable effect restored from the versioned persistence envelope.
///
/// Compensation linkage is represented explicitly instead of being inferred
/// from operation names or journal position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableEffectPersistenceRecord {
    Effect(DurableEffectRecord),
    Compensation {
        original_effect_id: DurableEffectId,
        compensation_ordinal: u32,
        effect: DurableEffectRecord,
    },
}

impl DurableEffectPersistenceRecord {
    pub fn from_effect(record: DurableEffectRecord) -> Self {
        Self::Effect(record)
    }

    pub fn from_compensation(record: &DurableCompensationRecord) -> Self {
        Self::Compensation {
            original_effect_id: record.original_effect_id(),
            compensation_ordinal: record.compensation_ordinal(),
            effect: record.effect().clone(),
        }
    }

    pub fn effect(&self) -> &DurableEffectRecord {
        match self {
            Self::Effect(effect) | Self::Compensation { effect, .. } => effect,
        }
    }

    pub fn original_effect_id(&self) -> Option<DurableEffectId> {
        match self {
            Self::Effect(_) => None,
            Self::Compensation {
                original_effect_id, ..
            } => Some(*original_effect_id),
        }
    }

    pub fn compensation_ordinal(&self) -> Option<u32> {
        match self {
            Self::Effect(_) => None,
            Self::Compensation {
                compensation_ordinal,
                ..
            } => Some(*compensation_ordinal),
        }
    }

    /// Encode the stable versioned JSON representation.
    pub fn to_json(&self) -> Result<Vec<u8>, DurableEffectPersistenceError> {
        let envelope = PersistedEnvelopeV1 {
            version: DURABLE_EFFECT_PERSISTENCE_VERSION,
            record: PersistedRecordKindV1::from_runtime(self),
        };
        serde_json::to_vec(&envelope).map_err(DurableEffectPersistenceError::from)
    }

    /// Decode a versioned durable-effect record.
    ///
    /// Unknown versions fail closed. Callers must perform an explicit format
    /// migration rather than asking an older runtime to guess at newer durable
    /// semantics. Compensation identity is re-derived during decode so a
    /// corrupted or inconsistent original-id/ordinal/operation tuple cannot be
    /// accepted as a different logical compensation.
    pub fn from_json(bytes: &[u8]) -> Result<Self, DurableEffectPersistenceError> {
        let envelope: PersistedEnvelopeV1 =
            serde_json::from_slice(bytes).map_err(DurableEffectPersistenceError::from)?;
        if envelope.version != DURABLE_EFFECT_PERSISTENCE_VERSION {
            return Err(DurableEffectPersistenceError::UnsupportedVersion {
                actual: envelope.version,
            });
        }
        envelope.record.into_runtime()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableEffectPersistenceError {
    Json(String),
    UnsupportedVersion {
        actual: u16,
    },
    InvalidEffectId(String),
    CompensationIdentityMismatch {
        expected: DurableEffectId,
        actual: DurableEffectId,
    },
}

impl fmt::Display for DurableEffectPersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "invalid durable effect persistence data: {message}"),
            Self::UnsupportedVersion { actual } => write!(
                f,
                "unsupported durable effect persistence version {actual}; runtime supports version {DURABLE_EFFECT_PERSISTENCE_VERSION}"
            ),
            Self::InvalidEffectId(value) => {
                write!(f, "invalid durable effect id in persistence data: {value}")
            }
            Self::CompensationIdentityMismatch { expected, actual } => write!(
                f,
                "persisted compensation identity mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for DurableEffectPersistenceError {}

impl From<serde_json::Error> for DurableEffectPersistenceError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PersistedEnvelopeV1 {
    version: u16,
    record: PersistedRecordKindV1,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value")]
enum PersistedRecordKindV1 {
    Effect(PersistedEffectRecordV1),
    Compensation {
        original_effect_id: String,
        compensation_ordinal: u32,
        effect: PersistedEffectRecordV1,
    },
}

impl PersistedRecordKindV1 {
    fn from_runtime(record: &DurableEffectPersistenceRecord) -> Self {
        match record {
            DurableEffectPersistenceRecord::Effect(effect) => {
                Self::Effect(PersistedEffectRecordV1::from_runtime(effect))
            }
            DurableEffectPersistenceRecord::Compensation {
                original_effect_id,
                compensation_ordinal,
                effect,
            } => Self::Compensation {
                original_effect_id: original_effect_id.to_string(),
                compensation_ordinal: *compensation_ordinal,
                effect: PersistedEffectRecordV1::from_runtime(effect),
            },
        }
    }

    fn into_runtime(self) -> Result<DurableEffectPersistenceRecord, DurableEffectPersistenceError> {
        match self {
            Self::Effect(effect) => Ok(DurableEffectPersistenceRecord::Effect(
                effect.into_runtime()?,
            )),
            Self::Compensation {
                original_effect_id,
                compensation_ordinal,
                effect,
            } => {
                let original_effect_id =
                    DurableEffectId::from_str(&original_effect_id).map_err(|_| {
                        DurableEffectPersistenceError::InvalidEffectId(original_effect_id.clone())
                    })?;
                let effect = effect.into_runtime()?;
                let actual = effect.spec().id;
                let expected = original_effect_id
                    .derive_compensation(compensation_ordinal, &effect.spec().effect_operation);
                if actual != expected {
                    return Err(
                        DurableEffectPersistenceError::CompensationIdentityMismatch {
                            expected,
                            actual,
                        },
                    );
                }
                Ok(DurableEffectPersistenceRecord::Compensation {
                    original_effect_id,
                    compensation_ordinal,
                    effect,
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", content = "value")]
enum PersistedEffectRecordV1 {
    Prepared {
        spec: PersistedEffectSpecV1,
        request_digest: [u8; 32],
    },
    Completed {
        spec: PersistedEffectSpecV1,
        request_digest: [u8; 32],
        result: Vec<u8>,
    },
}

impl PersistedEffectRecordV1 {
    fn from_runtime(record: &DurableEffectRecord) -> Self {
        match record {
            DurableEffectRecord::Prepared {
                spec,
                request_digest,
            } => Self::Prepared {
                spec: PersistedEffectSpecV1::from_runtime(spec),
                request_digest: *request_digest,
            },
            DurableEffectRecord::Completed {
                spec,
                request_digest,
                result,
            } => Self::Completed {
                spec: PersistedEffectSpecV1::from_runtime(spec),
                request_digest: *request_digest,
                result: result.clone(),
            },
        }
    }

    fn into_runtime(self) -> Result<DurableEffectRecord, DurableEffectPersistenceError> {
        match self {
            Self::Prepared {
                spec,
                request_digest,
            } => Ok(DurableEffectRecord::Prepared {
                spec: spec.into_runtime()?,
                request_digest,
            }),
            Self::Completed {
                spec,
                request_digest,
                result,
            } => Ok(DurableEffectRecord::Completed {
                spec: spec.into_runtime()?,
                request_digest,
                result,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PersistedEffectSpecV1 {
    id: String,
    effect_operation: String,
    boundary: PersistedEffectBoundaryV1,
    delivery: PersistedDeliverySemanticsV1,
}

impl PersistedEffectSpecV1 {
    fn from_runtime(spec: &DurableEffectSpec) -> Self {
        Self {
            id: spec.id.to_string(),
            effect_operation: spec.effect_operation.clone(),
            boundary: PersistedEffectBoundaryV1::from_runtime(spec.boundary),
            delivery: PersistedDeliverySemanticsV1::from_runtime(spec.delivery),
        }
    }

    fn into_runtime(self) -> Result<DurableEffectSpec, DurableEffectPersistenceError> {
        let id = DurableEffectId::from_str(&self.id)
            .map_err(|_| DurableEffectPersistenceError::InvalidEffectId(self.id.clone()))?;
        Ok(DurableEffectSpec::new(
            id,
            self.effect_operation,
            self.boundary.into_runtime(),
            self.delivery.into_runtime(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum PersistedEffectBoundaryV1 {
    RuntimeOwned,
    BackendOwned,
    External,
}

impl PersistedEffectBoundaryV1 {
    fn from_runtime(boundary: EffectBoundary) -> Self {
        match boundary {
            EffectBoundary::RuntimeOwned => Self::RuntimeOwned,
            EffectBoundary::BackendOwned => Self::BackendOwned,
            EffectBoundary::External => Self::External,
        }
    }

    fn into_runtime(self) -> EffectBoundary {
        match self {
            Self::RuntimeOwned => EffectBoundary::RuntimeOwned,
            Self::BackendOwned => EffectBoundary::BackendOwned,
            Self::External => EffectBoundary::External,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum PersistedDeliverySemanticsV1 {
    AtLeastOnce,
    EffectivelyOnceWithDeduplication,
    BackendDefined,
}

impl PersistedDeliverySemanticsV1 {
    fn from_runtime(delivery: DeliverySemantics) -> Self {
        match delivery {
            DeliverySemantics::AtLeastOnce => Self::AtLeastOnce,
            DeliverySemantics::EffectivelyOnceWithDeduplication => {
                Self::EffectivelyOnceWithDeduplication
            }
            DeliverySemantics::BackendDefined => Self::BackendDefined,
        }
    }

    fn into_runtime(self) -> DeliverySemantics {
        match self {
            Self::AtLeastOnce => DeliverySemantics::AtLeastOnce,
            Self::EffectivelyOnceWithDeduplication => {
                DeliverySemantics::EffectivelyOnceWithDeduplication
            }
            Self::BackendDefined => DeliverySemantics::BackendDefined,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_effect::DurableEffectRecoveryAction;

    fn completed_effect() -> DurableEffectRecord {
        let spec = DurableEffectSpec::new(
            DurableEffectId::derive(42, "charge-order-7", 0, "Payment.charge"),
            "Payment.charge",
            EffectBoundary::External,
            DeliverySemantics::EffectivelyOnceWithDeduplication,
        );
        DurableEffectRecord::prepare(spec, b"order=7&amount=10").complete(b"charged".to_vec())
    }

    #[test]
    fn completed_effect_round_trips_without_losing_replay_contract() {
        let persisted = DurableEffectPersistenceRecord::from_effect(completed_effect());
        let bytes = persisted.to_json().unwrap();
        let restored = DurableEffectPersistenceRecord::from_json(&bytes).unwrap();

        assert_eq!(restored.original_effect_id(), None);
        assert_eq!(restored.compensation_ordinal(), None);
        assert_eq!(
            restored
                .effect()
                .recovery_action_for_request(b"order=7&amount=10")
                .unwrap(),
            DurableEffectRecoveryAction::ReplayRecordedResult(b"charged")
        );
        assert!(restored
            .effect()
            .recovery_action_for_request(b"order=7&amount=11")
            .is_err());
    }

    #[test]
    fn compensation_round_trip_preserves_original_linkage() {
        let original = completed_effect();
        let original_id = original.spec().id;
        let compensation = original
            .prepare_compensation(
                0,
                "Payment.refund",
                EffectBoundary::External,
                DeliverySemantics::EffectivelyOnceWithDeduplication,
                b"order=7&refund=10",
            )
            .unwrap();
        let compensation_id = compensation.compensation_id();
        let persisted = DurableEffectPersistenceRecord::from_compensation(&compensation);
        let bytes = persisted.to_json().unwrap();
        let restored = DurableEffectPersistenceRecord::from_json(&bytes).unwrap();

        assert_eq!(restored.original_effect_id(), Some(original_id));
        assert_eq!(restored.compensation_ordinal(), Some(0));
        assert_eq!(restored.effect().spec().id, compensation_id);
        assert_eq!(
            restored
                .effect()
                .recovery_action_for_request(b"order=7&refund=10")
                .unwrap(),
            DurableEffectRecoveryAction::RetryWithDeduplication {
                operation_id: compensation_id,
            }
        );
    }

    #[test]
    fn corrupted_compensation_ordinal_fails_identity_validation() {
        let original = completed_effect();
        let compensation = original
            .prepare_compensation(
                0,
                "Payment.refund",
                EffectBoundary::External,
                DeliverySemantics::EffectivelyOnceWithDeduplication,
                b"order=7&refund=10",
            )
            .unwrap();
        let persisted = DurableEffectPersistenceRecord::from_compensation(&compensation);
        let bytes = persisted.to_json().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["record"]["value"]["compensation_ordinal"] = serde_json::Value::from(1);
        let bytes = serde_json::to_vec(&value).unwrap();

        assert!(matches!(
            DurableEffectPersistenceRecord::from_json(&bytes),
            Err(DurableEffectPersistenceError::CompensationIdentityMismatch { .. })
        ));
    }

    #[test]
    fn unknown_persistence_version_fails_closed() {
        let persisted = DurableEffectPersistenceRecord::from_effect(completed_effect());
        let bytes = persisted.to_json().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["version"] = serde_json::Value::from(999);
        let bytes = serde_json::to_vec(&value).unwrap();

        assert_eq!(
            DurableEffectPersistenceRecord::from_json(&bytes).unwrap_err(),
            DurableEffectPersistenceError::UnsupportedVersion { actual: 999 }
        );
    }

    #[test]
    fn persistence_format_carries_an_explicit_version() {
        let persisted = DurableEffectPersistenceRecord::from_effect(completed_effect());
        let bytes = persisted.to_json().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["version"].as_u64(),
            Some(DURABLE_EFFECT_PERSISTENCE_VERSION as u64)
        );
    }
}
