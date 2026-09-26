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

pub const DURABLE_TRANSITION_PROTOCOL_VERSION: &str = "nulang-durable-transition/v0alpha1";
const DURABLE_TRANSITION_DIGEST_DOMAIN: &[u8] = b"nulang.durable-transition-protocol.v0alpha1\0";

mod u64_string {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse::<u64>().map_err(serde::de::Error::custom)
    }
}

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
    #[serde(with = "u64_string")]
    pub activation_epoch: u64,
    #[serde(with = "u64_string")]
    pub sequence: u64,
    #[serde(with = "u64_string")]
    pub expected_previous_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<DurableCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<DurableStateCheckpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_events: Vec<DurableWorkflowEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domain_events: Vec<DurableDomainEvent>,
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

        if let Some(command) = &self.command {
            command.validate()?;
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
        for event in &self.domain_events {
            event.validate()?;
        }
        for timer in &self.timers {
            timer.validate(self.activation_epoch, self.sequence)?;
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
        let value = serde_json::to_value(self)
            .map_err(|error| DurableProtocolError::Serialization(error.to_string()))?;
        let mut bytes = Vec::new();
        write_canonical_json(&value, &mut bytes)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(DURABLE_TRANSITION_DIGEST_DOMAIN);
        hasher.update(&bytes);
        Ok(format!("blake3:{}", hasher.finalize().to_hex()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableCommand {
    pub command_id: String,
    pub command_type: String,
    #[serde(default)]
    pub payload: Value,
}

impl DurableCommand {
    fn validate(&self) -> Result<(), DurableProtocolError> {
        if self.command_id.trim().is_empty() || self.command_type.trim().is_empty() {
            Err(DurableProtocolError::InvalidCommand)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableStateCheckpoint {
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableWorkflowActivation {
    #[serde(with = "u64_string")]
    pub actor_id: u64,
    #[serde(with = "u64_string")]
    pub command_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum DurableWorkflowEvent {
    WorkflowStarted {
        workflow_name: String,
    },
    StepCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation: Option<DurableWorkflowActivation>,
        step_name: String,
    },
    StepFailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation: Option<DurableWorkflowActivation>,
        step_name: String,
        error: String,
    },
    SignalAccepted {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
    },
    SagaCompensated {
        step_name: String,
    },
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
            Self::StepCompleted { step_name, .. } | Self::SagaCompensated { step_name } => {
                !step_name.trim().is_empty()
            }
            Self::StepFailed {
                step_name, error, ..
            } => {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableDomainEvent {
    pub event_type: String,
    #[serde(default)]
    pub payload: Value,
}

impl DurableDomainEvent {
    fn validate(&self) -> Result<(), DurableProtocolError> {
        if self.event_type.trim().is_empty() {
            Err(DurableProtocolError::InvalidDomainEvent)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum DurableTimerMutation {
    Set {
        timer_id: String,
        #[serde(with = "u64_string")]
        set_activation_epoch: u64,
        #[serde(with = "u64_string")]
        set_sequence: u64,
        #[serde(with = "u64_string")]
        due_at_unix_ms: u64,
    },
    Cancel {
        timer_id: String,
        #[serde(with = "u64_string")]
        set_activation_epoch: u64,
        #[serde(with = "u64_string")]
        set_sequence: u64,
    },
    Fired {
        timer_id: String,
        #[serde(with = "u64_string")]
        set_activation_epoch: u64,
        #[serde(with = "u64_string")]
        set_sequence: u64,
    },
}

impl DurableTimerMutation {
    fn validate(
        &self,
        transition_activation_epoch: u64,
        transition_sequence: u64,
    ) -> Result<(), DurableProtocolError> {
        let (timer_id, set_activation_epoch, set_sequence, is_set) = match self {
            Self::Set {
                timer_id,
                set_activation_epoch,
                set_sequence,
                ..
            } => (timer_id, *set_activation_epoch, *set_sequence, true),
            Self::Cancel {
                timer_id,
                set_activation_epoch,
                set_sequence,
            }
            | Self::Fired {
                timer_id,
                set_activation_epoch,
                set_sequence,
            } => (timer_id, *set_activation_epoch, *set_sequence, false),
        };

        if timer_id.trim().is_empty() {
            return Err(DurableProtocolError::InvalidTimer);
        }
        if set_activation_epoch == 0 || set_sequence == 0 {
            return Err(DurableProtocolError::InvalidTimerGeneration);
        }
        if set_activation_epoch > transition_activation_epoch || set_sequence > transition_sequence
        {
            return Err(DurableProtocolError::InvalidTimerGeneration);
        }
        if is_set
            && (set_activation_epoch != transition_activation_epoch
                || set_sequence != transition_sequence)
        {
            return Err(DurableProtocolError::InvalidTimerGeneration);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurableEffectIdValue([u8; 32]);

impl DurableEffectIdValue {
    pub fn parse(value: &str) -> Result<Self, DurableProtocolError> {
        if value.len() != 64 {
            return Err(DurableProtocolError::InvalidDurableEffect);
        }
        let bytes = value.as_bytes();
        let mut out = [0u8; 32];
        for (index, slot) in out.iter_mut().enumerate() {
            let hi = decode_hex_lower(bytes[index * 2])
                .ok_or(DurableProtocolError::InvalidDurableEffect)?;
            let lo = decode_hex_lower(bytes[index * 2 + 1])
                .ok_or(DurableProtocolError::InvalidDurableEffect)?;
            *slot = (hi << 4) | lo;
        }
        Ok(Self(out))
    }

    pub fn derive_compensation(self, compensation_ordinal: u32, operation: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(DURABLE_COMPENSATION_ID_DOMAIN);
        hasher.update(&self.0);
        hasher.update(&compensation_ordinal.to_le_bytes());
        hash_len_prefixed(&mut hasher, operation.as_bytes());
        Self(*hasher.finalize().as_bytes())
    }
}

impl fmt::Display for DurableEffectIdValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for DurableEffectIdValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for DurableEffectIdValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

fn decode_hex_lower(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn hash_len_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableEffectBoundary {
    RuntimeOwned,
    BackendOwned,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableDeliverySemantics {
    AtLeastOnce,
    EffectivelyOnceWithDeduplication,
    BackendDefined,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub enum DurableEffectMutation {
    Prepared {
        effect_id: DurableEffectIdValue,
        operation: String,
        boundary: DurableEffectBoundary,
        delivery: DurableDeliverySemantics,
        request_digest: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    Completed {
        effect_id: DurableEffectIdValue,
        operation: String,
        boundary: DurableEffectBoundary,
        delivery: DurableDeliverySemantics,
        request_digest: String,
        result_digest: String,
        #[serde(default)]
        result: Value,
    },
    CompensationPrepared {
        original_effect_id: DurableEffectIdValue,
        compensation_ordinal: u32,
        effect_id: DurableEffectIdValue,
        operation: String,
        boundary: DurableEffectBoundary,
        delivery: DurableDeliverySemantics,
        request_digest: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    CompensationCompleted {
        original_effect_id: DurableEffectIdValue,
        compensation_ordinal: u32,
        effect_id: DurableEffectIdValue,
        operation: String,
        boundary: DurableEffectBoundary,
        delivery: DurableDeliverySemantics,
        request_digest: String,
        result_digest: String,
        #[serde(default)]
        result: Value,
    },
}

impl DurableEffectMutation {
    fn validate(&self) -> Result<(), DurableProtocolError> {
        match self {
            Self::Prepared {
                operation,
                request_digest,
                idempotency_key,
                ..
            } => validate_prepared_effect(operation, request_digest, idempotency_key.as_deref()),
            Self::Completed {
                operation,
                request_digest,
                result_digest,
                ..
            } => validate_completed_effect(operation, request_digest, result_digest),
            Self::CompensationPrepared {
                original_effect_id,
                compensation_ordinal,
                effect_id,
                operation,
                request_digest,
                idempotency_key,
                ..
            } => {
                if original_effect_id.derive_compensation(*compensation_ordinal, operation)
                    != *effect_id
                {
                    return Err(DurableProtocolError::InvalidCompensation);
                }
                validate_prepared_effect(operation, request_digest, idempotency_key.as_deref())
            }
            Self::CompensationCompleted {
                original_effect_id,
                compensation_ordinal,
                effect_id,
                operation,
                request_digest,
                result_digest,
                ..
            } => {
                if original_effect_id.derive_compensation(*compensation_ordinal, operation)
                    != *effect_id
                {
                    return Err(DurableProtocolError::InvalidCompensation);
                }
                validate_completed_effect(operation, request_digest, result_digest)
            }
        }
    }
}

fn validate_prepared_effect(
    operation: &str,
    request_digest: &str,
    idempotency_key: Option<&str>,
) -> Result<(), DurableProtocolError> {
    if operation.trim().is_empty()
        || !valid_blake3_digest(request_digest)
        || idempotency_key.is_some_and(|key| key.trim().is_empty())
    {
        Err(DurableProtocolError::InvalidDurableEffect)
    } else {
        Ok(())
    }
}

fn validate_completed_effect(
    operation: &str,
    request_digest: &str,
    result_digest: &str,
) -> Result<(), DurableProtocolError> {
    if operation.trim().is_empty()
        || !valid_blake3_digest(request_digest)
        || !valid_blake3_digest(result_digest)
    {
        Err(DurableProtocolError::InvalidDurableEffect)
    } else {
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
    #[serde(with = "u64_string")]
    pub activation_epoch: u64,
    #[serde(with = "u64_string")]
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
    InvalidCommand,
    InvalidStateField,
    InvalidWorkflowEvent,
    InvalidDomainEvent,
    InvalidTimer,
    InvalidTimerGeneration,
    InvalidDurableEffect,
    InvalidCompensation,
    InvalidOutboxMessage,
    DuplicateOutboxOrdinal(u32),
    Serialization(String),
    DigestMismatch {
        expected: String,
        actual: String,
    },
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
            Self::InvalidCommand => write!(f, "invalid durable command record"),
            Self::InvalidStateField => write!(f, "durable state field name must not be empty"),
            Self::InvalidWorkflowEvent => write!(f, "invalid durable workflow event"),
            Self::InvalidDomainEvent => write!(f, "invalid durable domain event"),
            Self::InvalidTimer => write!(f, "invalid durable timer mutation"),
            Self::InvalidTimerGeneration => {
                write!(f, "invalid durable timer generation identity")
            }
            Self::InvalidDurableEffect => write!(f, "invalid durable effect mutation"),
            Self::InvalidCompensation => {
                write!(f, "invalid durable compensation linkage")
            }
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

fn write_canonical_json(value: &Value, out: &mut Vec<u8>) -> Result<(), DurableProtocolError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(value) => {
            out.extend_from_slice(if *value { b"true" } else { b"false" });
        }
        Value::Number(value) => out.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => {
            let encoded = serde_json::to_string(value)
                .map_err(|error| DurableProtocolError::Serialization(error.to_string()))?;
            out.extend_from_slice(encoded.as_bytes());
        }
        Value::Array(values) => {
            out.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical_json(value, out)?;
            }
            out.push(b']');
        }
        Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_unstable();
            out.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                let encoded = serde_json::to_string(key)
                    .map_err(|error| DurableProtocolError::Serialization(error.to_string()))?;
                out.extend_from_slice(encoded.as_bytes());
                out.push(b':');
                write_canonical_json(&values[key], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
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
            command: Some(DurableCommand {
                command_id: "msg-11".into(),
                command_type: "ChargeOrder".into(),
                payload: json!({"order_id":"42"}),
            }),
            state: Some(DurableStateCheckpoint {
                fields: BTreeMap::from([
                    ("step_index".into(), json!(2)),
                    ("order_id".into(), json!("42")),
                ]),
            }),
            workflow_events: vec![DurableWorkflowEvent::StepCompleted {
                activation: None,
                step_name: "charge".into(),
            }],
            domain_events: vec![DurableDomainEvent {
                event_type: "OrderCharged".into(),
                payload: json!({"order_id":"42"}),
            }],
            timers: vec![DurableTimerMutation::Set {
                timer_id: "shipping-timeout".into(),
                set_activation_epoch: 7,
                set_sequence: 12,
                due_at_unix_ms: 1_800_000_000_000,
            }],
            durable_effects: vec![DurableEffectMutation::Prepared {
                effect_id: DurableEffectIdValue::parse(
                    "0101010101010101010101010101010101010101010101010101010101010101",
                )
                .unwrap(),
                operation: "Payment.charge".into(),
                boundary: DurableEffectBoundary::External,
                delivery: DurableDeliverySemantics::EffectivelyOnceWithDeduplication,
                request_digest:
                    "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
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
        assert_eq!(value["activation_epoch"], "7");
        assert_eq!(value["sequence"], "12");
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
    fn fencing_counters_roundtrip_above_javascript_safe_integer_range() {
        let mut large = transition();
        large.activation_epoch = 9_007_199_254_740_993;
        large.expected_previous_sequence = 9_007_199_254_740_993;
        large.sequence = 9_007_199_254_740_994;

        let encoded = serde_json::to_value(&large).unwrap();
        assert_eq!(encoded["activation_epoch"], "9007199254740993");
        assert_eq!(encoded["sequence"], "9007199254740994");

        let decoded: DurableTransition = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.activation_epoch, large.activation_epoch);
        assert_eq!(decoded.sequence, large.sequence);
    }

    #[test]
    fn deterministic_digest_ignores_nested_json_object_insertion_order() {
        let mut first = transition();
        let mut second = transition();

        let mut left = serde_json::Map::new();
        left.insert("a".into(), json!(1));
        left.insert("b".into(), json!({"x":1,"y":2}));
        let mut right_nested = serde_json::Map::new();
        right_nested.insert("y".into(), json!(2));
        right_nested.insert("x".into(), json!(1));
        let mut right = serde_json::Map::new();
        right.insert("b".into(), Value::Object(right_nested));
        right.insert("a".into(), json!(1));

        first.state = Some(DurableStateCheckpoint {
            fields: BTreeMap::from([("payload".into(), Value::Object(left))]),
        });
        second.state = Some(DurableStateCheckpoint {
            fields: BTreeMap::from([("payload".into(), Value::Object(right))]),
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
            activation: None,
            step_name: "charge".into(),
            error: "declined".into(),
        }];
        let second = DurableCommitRequest::new(changed).unwrap();

        assert_eq!(first.transition.sequence, second.transition.sequence);
        assert_ne!(first.digest, second.digest);
    }

    #[test]
    fn command_domain_and_effect_recovery_semantics_are_in_atomic_record() {
        let value = serde_json::to_value(transition()).unwrap();

        assert_eq!(value["command"]["command_id"], "msg-11");
        assert_eq!(value["domain_events"][0]["event_type"], "OrderCharged");
        assert_eq!(value["durable_effects"][0]["boundary"], "external");
        assert_eq!(
            value["durable_effects"][0]["delivery"],
            "effectively_once_with_deduplication"
        );
    }

    #[test]
    fn timer_generation_is_stable_across_rearm_and_fire() {
        let set = DurableTimerMutation::Set {
            timer_id: "shipping-timeout".into(),
            set_activation_epoch: 7,
            set_sequence: 12,
            due_at_unix_ms: 1_800_000_000_000,
        };
        let fired = DurableTimerMutation::Fired {
            timer_id: "shipping-timeout".into(),
            set_activation_epoch: 7,
            set_sequence: 12,
        };
        let rearmed = DurableTimerMutation::Set {
            timer_id: "shipping-timeout".into(),
            set_activation_epoch: 8,
            set_sequence: 19,
            due_at_unix_ms: 1_900_000_000_000,
        };

        let set_value = serde_json::to_value(&set).unwrap();
        let fired_value = serde_json::to_value(&fired).unwrap();
        let rearmed_value = serde_json::to_value(&rearmed).unwrap();

        assert_eq!(set_value["set_activation_epoch"], "7");
        assert_eq!(set_value["set_sequence"], "12");
        assert_eq!(fired_value["set_activation_epoch"], "7");
        assert_eq!(fired_value["set_sequence"], "12");
        assert_ne!(
            (
                set_value["set_activation_epoch"].clone(),
                set_value["set_sequence"].clone()
            ),
            (
                rearmed_value["set_activation_epoch"].clone(),
                rearmed_value["set_sequence"].clone()
            )
        );
    }

    #[test]
    fn timer_set_generation_must_match_committing_transition() {
        let mut invalid = transition();
        invalid.timers = vec![DurableTimerMutation::Set {
            timer_id: "shipping-timeout".into(),
            set_activation_epoch: invalid.activation_epoch,
            set_sequence: invalid.sequence - 1,
            due_at_unix_ms: 1_800_000_000_000,
        }];

        assert_eq!(
            invalid.validate().unwrap_err(),
            DurableProtocolError::InvalidTimerGeneration
        );
    }

    #[test]
    fn compensation_effect_records_preserve_original_linkage() {
        let mutation = DurableEffectMutation::CompensationPrepared {
            original_effect_id: DurableEffectIdValue::parse(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap(),
            compensation_ordinal: 2,
            effect_id: DurableEffectIdValue::parse(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap()
            .derive_compensation(2, "Payment.refund"),
            operation: "Payment.refund".into(),
            boundary: DurableEffectBoundary::External,
            delivery: DurableDeliverySemantics::EffectivelyOnceWithDeduplication,
            request_digest:
                "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            idempotency_key: Some("eff-compensation".into()),
        };

        let value = serde_json::to_value(&mutation).unwrap();
        assert_eq!(value["kind"], "compensation_prepared");
        assert_eq!(
            value["original_effect_id"],
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert_eq!(value["compensation_ordinal"], 2);
        assert_eq!(
            value["effect_id"],
            DurableEffectIdValue::parse(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap()
            .derive_compensation(2, "Payment.refund")
            .to_string()
        );

        let decoded: DurableEffectMutation = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, mutation);
    }

    #[test]
    fn completed_compensation_preserves_linkage_and_result() {
        let mutation = DurableEffectMutation::CompensationCompleted {
            original_effect_id: DurableEffectIdValue::parse(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap(),
            compensation_ordinal: 2,
            effect_id: DurableEffectIdValue::parse(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap()
            .derive_compensation(2, "Payment.refund"),
            operation: "Payment.refund".into(),
            boundary: DurableEffectBoundary::External,
            delivery: DurableDeliverySemantics::EffectivelyOnceWithDeduplication,
            request_digest:
                "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            result_digest:
                "blake3:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            result: json!({"refunded": true}),
        };

        let value = serde_json::to_value(&mutation).unwrap();
        assert_eq!(value["kind"], "compensation_completed");
        assert_eq!(
            value["original_effect_id"],
            "1111111111111111111111111111111111111111111111111111111111111111"
        );
        assert_eq!(value["compensation_ordinal"], 2);
        assert_eq!(value["result"]["refunded"], true);

        let decoded: DurableEffectMutation = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, mutation);
    }

    #[test]
    fn compensation_effect_requires_original_identity() {
        let value = json!({
            "kind": "compensation_prepared",
            "original_effect_id": " ",
            "compensation_ordinal": 0,
            "effect_id":
                "2222222222222222222222222222222222222222222222222222222222222222",
            "operation": "Payment.refund",
            "boundary": "external",
            "delivery": "at_least_once",
            "request_digest":
                "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });

        assert!(serde_json::from_value::<DurableEffectMutation>(value).is_err());
    }

    #[test]
    fn durable_effect_ids_must_match_runtime_identity_shape() {
        let value = json!({
            "kind": "prepared",
            "effect_id": "eff-01",
            "operation": "Payment.charge",
            "boundary": "external",
            "delivery": "at_least_once",
            "request_digest":
                "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        });

        assert!(serde_json::from_value::<DurableEffectMutation>(value).is_err());
    }

    #[test]
    fn compensation_effect_id_must_match_runtime_derivation() {
        let mut invalid = transition();
        invalid.durable_effects = vec![DurableEffectMutation::CompensationPrepared {
            original_effect_id: DurableEffectIdValue::parse(
                "1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap(),
            compensation_ordinal: 2,
            effect_id: DurableEffectIdValue::parse(
                "2222222222222222222222222222222222222222222222222222222222222222",
            )
            .unwrap(),
            operation: "Payment.refund".into(),
            boundary: DurableEffectBoundary::External,
            delivery: DurableDeliverySemantics::AtLeastOnce,
            request_digest:
                "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            idempotency_key: None,
        }];

        assert_eq!(
            invalid.validate().unwrap_err(),
            DurableProtocolError::InvalidCompensation
        );
    }

    #[test]
    fn future_timer_generation_is_rejected() {
        let mut invalid = transition();
        invalid.timers = vec![DurableTimerMutation::Fired {
            timer_id: "shipping-timeout".into(),
            set_activation_epoch: invalid.activation_epoch + 1,
            set_sequence: invalid.sequence + 1,
        }];

        assert_eq!(
            invalid.validate().unwrap_err(),
            DurableProtocolError::InvalidTimerGeneration
        );
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


    #[test]
    fn terminal_workflow_activation_identity_roundtrips_exact_u64_values() {
        let activation = DurableWorkflowActivation {
            actor_id: 9_007_199_254_740_993,
            command_sequence: 9_007_199_254_740_994,
        };
        let event = DurableWorkflowEvent::StepCompleted {
            activation: Some(activation.clone()),
            step_name: "charge".into(),
        };

        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["activation"]["actor_id"], "9007199254740993");
        assert_eq!(
            value["activation"]["command_sequence"],
            "9007199254740994"
        );

        let decoded: DurableWorkflowEvent = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn legacy_terminal_workflow_event_without_activation_remains_digest_compatible() {
        let legacy = json!({
            "kind": "step_completed",
            "step_name": "charge"
        });
        let event: DurableWorkflowEvent = serde_json::from_value(legacy.clone()).unwrap();

        assert_eq!(
            event,
            DurableWorkflowEvent::StepCompleted {
                activation: None,
                step_name: "charge".into(),
            }
        );
        assert_eq!(serde_json::to_value(event).unwrap(), legacy);
    }


    #[test]
    fn unknown_semantic_record_fields_fail_closed() {
        let value = json!({
            "kind": "step_completed",
            "step_name": "charge",
            "typo_field": true
        });
        assert!(serde_json::from_value::<DurableWorkflowEvent>(value).is_err());

        let value = json!({
            "kind": "set",
            "timer_id": "t1",
            "set_activation_epoch": "1",
            "set_sequence": "1",
            "due_at_unix_ms": "10",
            "typo_field": true
        });
        assert!(serde_json::from_value::<DurableTimerMutation>(value).is_err());
    }
}
