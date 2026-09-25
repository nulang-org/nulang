//! Versioned transport-neutral atomic durable transition protocol.
//!
//! This crate is the host boundary for RFC 0022. It describes one logical
//! durable transition without choosing a database, journal implementation,
//! timer service, transport, or Cloud runtime. Nulang owns the meaning of these
//! records; a host owns how they are committed atomically.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const DURABLE_TRANSITION_PROTOCOL_VERSION: &str =
    "nulang-durable-transition/v0alpha1";
const DURABLE_TRANSITION_DIGEST_DOMAIN: &[u8] =
    b"nulang.durable-transition-protocol.v0alpha1\0";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DurableOwnerId(String);

impl DurableOwnerId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DurableOwnerId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for DurableOwnerId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Display for DurableOwnerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableTransition {
    pub protocol: String,
    pub owner_id: DurableOwnerId,
    pub activation_epoch: u64,
    pub sequence: u64,
    pub expected_previous_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<DurableStateCheckpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_events: Vec<DurableWorkflowEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timers: Vec<DurableTimerMutation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub durable_effects: Vec<DurableEffectMutation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outbox: Vec<DurableOutboxMessage>,
}

impl DurableTransition {
    pub fn validate(&self) -> Result<(), DurableProtocolError> {
        if self.protocol != DURABLE_TRANSITION_PROTOCOL_VERSION {
            return Err(DurableProtocolError::UnsupportedVersion(
                self.protocol.clone(),
            ));
        }
        if self.owner_id.as_str().trim().is_empty() {
            return Err(DurableProtocolError::InvalidOwnerId);
        }
        if self.activation_epoch == 0 {
            return Err(DurableProtocolError::InvalidActivationEpoch);
        }
        if self.sequence == 0 {
            return Err(DurableProtocolError::InvalidSequence);
        }
        if self
            .expected_previous_sequence
            .checked_add(1)
            .filter(|expected| *expected == self.sequence)
            .is_none()
        {
            return Err(DurableProtocolError::NonContiguousSequence {
                expected_previous_sequence: self.expected_previous_sequence,
                sequence: self.sequence,
            });
        }

        if let Some(state) = &self.state {
            for key in state.fields.keys() {
                if key.trim().is_empty() {
                    return Err(DurableProtocolError::InvalidStateField);
                }
            }
        }

        for event in &self.workflow_events {
            event.validate()?;
        }
        for timer in &self.timers {
            timer.validate()?;
        }
        for effect in &self.durable_effects {
            effect.validate()?;
        }

        let mut ordinals = BTreeSet::new();
        for message in &self.outbox {
            if message.destination.as_str().trim().is_empty()
                || message.message_type.trim().is_empty()
            {
                return Err(DurableProtocolError::InvalidOutboxMessage);
            }
            if !ordinals.insert(message.ordinal) {
                return Err(DurableProtocolError::DuplicateOutboxOrdinal(
                    message.ordinal,
                ));
            }
        }

        Ok(())
    }

    pub fn digest(&self) -> Result<String, DurableProtocolError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)
            .map_err(|error| DurableProtocolError::Serialization(error.to_string()))?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(DURABLE_TRANSITION_DIGEST_DOMAIN);
        hasher.update(&bytes);
        Ok(format!("blake3:{}", hasher.finalize().to_hex()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableStateCheckpoint {
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableWorkflowEvent {
    WorkflowStarted { workflow_name: String },
    StepCompleted { step_name: String },
    StepFailed { step_name: String, error: String },
    SignalAccepted {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
    },
    SagaCompensated { step_name: String },
    ParallelBranchCompleted {
        step_name: String,
        branch_name: String,
    },
    Custom {
        name: String,
        #[serde(default)]
        payload: Value,
    },
}

impl DurableWorkflowEvent {
    fn validate(&self) -> Result<(), DurableProtocolError> {
        let valid = match self {
            Self::WorkflowStarted { workflow_name } => !workflow_name.trim().is_empty(),
            Self::StepCompleted { step_name }
            | Self::SagaCompensated { step_name } => !step_name.trim().is_empty(),
            Self::StepFailed { step_name, error } => {
                !step_name.trim().is_empty() && !error.trim().is_empty()
            }
            Self::SignalAccepted { name, .. } | Self::Custom { name, .. } => {
                !name.trim().is_empty()
            }
            Self::ParallelBranchCompleted {
                step_name,
                branch_name,
            } => !step_name.trim().is_empty() && !branch_name.trim().is_empty(),
        };
        if valid {
            Ok(())
        } else {
            Err(DurableProtocolError::InvalidWorkflowEvent)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableTimerMutation {
    Set {
        timer_id: String,
        due_at_unix_ms: u64,
    },
    Cancel {
        timer_id: String,
    },
    Fired {
        timer_id: String,
    },
}

impl DurableTimerMutation {
    fn validate(&self) -> Result<(), DurableProtocolError> {
        let timer_id = match self {
            Self::Set { timer_id, .. } | Self::Cancel { timer_id } | Self::Fired { timer_id } => {
                timer_id
            }
        };
        if timer_id.trim().is_empty() {
            Err(DurableProtocolError::InvalidTimer)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableEffectMutation {
    Prepared {
        effect_id: String,
        operation: String,
        request_digest: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    Completed {
        effect_id: String,
        result_digest: String,
        #[serde(default)]
        result: Value,
    },
}

impl DurableEffectMutation {
    fn validate(&self) -> Result<(), DurableProtocolError> {
        match self {
            Self::Prepared {
                effect_id,
                operation,
                request_digest,
                idempotency_key,
            } => {
                if effect_id.trim().is_empty()
                    || operation.trim().is_empty()
                    || !valid_blake3_digest(request_digest)
                    || idempotency_key
                        .as_ref()
                        .is_some_and(|key| key.trim().is_empty())
                {
                    return Err(DurableProtocolError::InvalidDurableEffect);
                }
            }
            Self::Completed {
                effect_id,
                result_digest,
                ..
            } => {
                if effect_id.trim().is_empty() || !valid_blake3_digest(result_digest) {
                    return Err(DurableProtocolError::InvalidDurableEffect);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableOutboxMessage {
    pub destination: DurableOwnerId,
    pub ordinal: u32,
    pub message_type: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableCommitRequest {
    pub transition: DurableTransition,
    pub digest: String,
}

impl DurableCommitRequest {
    pub fn new(transition: DurableTransition) -> Result<Self, DurableProtocolError> {
        let digest = transition.digest()?;
        Ok(Self { transition, digest })
    }

    pub fn validate(&self) -> Result<(), DurableProtocolError> {
        let expected = self.transition.digest()?;
        if expected != self.digest {
            return Err(DurableProtocolError::DigestMismatch {
                expected,
                actual: self.digest.clone(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableCommit {
    pub owner_id: DurableOwnerId,
    pub activation_epoch: u64,
    pub sequence: u64,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableProtocolError {
    UnsupportedVersion(String),
    InvalidOwnerId,
    InvalidActivationEpoch,
    InvalidSequence,
    NonContiguousSequence {
        expected_previous_sequence: u64,
        sequence: u64,
    },
    InvalidStateField,
    InvalidWorkflowEvent,
    InvalidTimer,
    InvalidDurableEffect,
    InvalidOutboxMessage,
    DuplicateOutboxOrdinal(u32),
    Serialization(String),
    DigestMismatch { expected: String, actual: String },
}

impl fmt::Display for DurableProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported durable transition protocol: {version}")
            }
            Self::InvalidOwnerId => write!(f, "durable owner id must not be empty"),
            Self::InvalidActivationEpoch => {
                write!(f, "durable activation epoch must be non-zero")
            }
            Self::InvalidSequence => write!(f, "durable transition sequence must be non-zero"),
            Self::NonContiguousSequence {
                expected_previous_sequence,
                sequence,
            } => write!(
                f,
                "durable transition sequence {sequence} does not follow predecessor {expected_previous_sequence}"
            ),
            Self::InvalidStateField => write!(f, "durable state field name must not be empty"),
            Self::InvalidWorkflowEvent => write!(f, "invalid durable workflow event"),
            Self::InvalidTimer => write!(f, "invalid durable timer mutation"),
            Self::InvalidDurableEffect => write!(f, "invalid durable effect mutation"),
            Self::InvalidOutboxMessage => write!(f, "invalid durable outbox message"),
            Self::DuplicateOutboxOrdinal(ordinal) => {
                write!(f, "duplicate durable outbox ordinal {ordinal}")
            }
            Self::Serialization(message) => {
                write!(f, "durable transition serialization failed: {message}")
            }
            Self::DigestMismatch { expected, actual } => write!(
                f,
                "durable transition digest mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for DurableProtocolError {}

fn valid_blake3_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("blake3:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn transition() -> DurableTransition {
        DurableTransition {
            protocol: DURABLE_TRANSITION_PROTOCOL_VERSION.into(),
            owner_id: DurableOwnerId::new("tenant/orders/order-42"),
            activation_epoch: 7,
            sequence: 12,
            expected_previous_sequence: 11,
            state: Some(DurableStateCheckpoint {
                fields: BTreeMap::from([
                    ("step_index".into(), json!(2)),
                    ("order_id".into(), json!("42")),
                ]),
            }),
            workflow_events: vec![DurableWorkflowEvent::StepCompleted {
                step_name: "charge".into(),
            }],
            timers: vec![DurableTimerMutation::Set {
                timer_id: "shipping-timeout".into(),
                due_at_unix_ms: 1_800_000_000_000,
            }],
            durable_effects: vec![DurableEffectMutation::Prepared {
                effect_id: "eff-01".into(),
                operation: "Payment.charge".into(),
                request_digest: "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                idempotency_key: Some("eff-01".into()),
            }],
            outbox: vec![DurableOutboxMessage {
                destination: DurableOwnerId::new("tenant/orders/notifications"),
                ordinal: 0,
                message_type: "OrderCharged".into(),
                payload: json!({"order_id":"42"}),
            }],
        }
    }

    #[test]
    fn transition_roundtrips_with_stable_tagged_records() {
        let value = serde_json::to_value(transition()).unwrap();

        assert_eq!(value["protocol"], DURABLE_TRANSITION_PROTOCOL_VERSION);
        assert_eq!(value["workflow_events"][0]["kind"], "step_completed");
        assert_eq!(value["timers"][0]["kind"], "set");
        assert_eq!(value["durable_effects"][0]["kind"], "prepared");

        let decoded: DurableTransition = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, transition());
    }

    #[test]
    fn transition_validation_enforces_epoch_and_sequence_fencing() {
        let mut invalid = transition();
        invalid.activation_epoch = 0;
        assert_eq!(
            invalid.validate().unwrap_err(),
            DurableProtocolError::InvalidActivationEpoch
        );

        let mut invalid = transition();
        invalid.sequence = 13;
        assert_eq!(
            invalid.validate().unwrap_err(),
            DurableProtocolError::NonContiguousSequence {
                expected_previous_sequence: 11,
                sequence: 13,
            }
        );
    }

    #[test]
    fn deterministic_digest_ignores_state_insertion_order() {
        let mut first = transition();
        let mut second = transition();

        first.state = Some(DurableStateCheckpoint {
            fields: BTreeMap::from([
                ("a".into(), json!(1)),
                ("b".into(), json!(2)),
            ]),
        });
        second.state = Some(DurableStateCheckpoint {
            fields: BTreeMap::from([
                ("b".into(), json!(2)),
                ("a".into(), json!(1)),
            ]),
        });

        assert_eq!(first.digest().unwrap(), second.digest().unwrap());
    }

    #[test]
    fn exact_duplicate_commit_can_be_identified_by_sequence_and_digest() {
        let first = DurableCommitRequest::new(transition()).unwrap();
        let second = DurableCommitRequest::new(transition()).unwrap();

        assert_eq!(first.transition.sequence, second.transition.sequence);
        assert_eq!(first.digest, second.digest);
    }

    #[test]
    fn same_sequence_with_different_content_has_different_digest() {
        let first = DurableCommitRequest::new(transition()).unwrap();
        let mut changed = transition();
        changed.workflow_events = vec![DurableWorkflowEvent::StepFailed {
            step_name: "charge".into(),
            error: "declined".into(),
        }];
        let second = DurableCommitRequest::new(changed).unwrap();

        assert_eq!(first.transition.sequence, second.transition.sequence);
        assert_ne!(first.digest, second.digest);
    }

    #[test]
    fn timer_and_signal_records_are_semantic_not_host_specific() {
        let records = vec![
            DurableWorkflowEvent::SignalAccepted {
                name: "approved".into(),
                payload: Some(json!({"by":"manager"})),
            },
            DurableWorkflowEvent::SagaCompensated {
                step_name: "reserve".into(),
            },
            DurableWorkflowEvent::ParallelBranchCompleted {
                step_name: "notify".into(),
                branch_name: "email".into(),
            },
        ];
        let encoded = serde_json::to_value(records).unwrap();

        assert_eq!(encoded[0]["kind"], "signal_accepted");
        assert_eq!(encoded[1]["kind"], "saga_compensated");
        assert_eq!(encoded[2]["kind"], "parallel_branch_completed");
    }
}
