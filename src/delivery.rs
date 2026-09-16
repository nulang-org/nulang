//! Versioned delivery metadata and structured dead-letter primitives.
//!
//! This module is the compatibility boundary between logical message identity
//! and concrete mailbox / transport representations. It deliberately does not
//! change the existing NUL0 `Packet::ActorMessage` layout yet: callers can
//! adopt the envelope and codec first, then bump the wire protocol explicitly
//! when the runtime threads metadata through every send path.

use crate::message::{MessageId, MessageMeta};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt;

/// First stable binary representation of [`MessageMeta`].
pub const MESSAGE_META_WIRE_VERSION: u8 = 1;
/// `NDM1` — Nulang Delivery Metadata, version 1.
pub const MESSAGE_META_MAGIC: [u8; 4] = *b"NDM1";
/// Fixed encoded length for delivery metadata v1.
pub const MESSAGE_META_WIRE_LEN: usize = 66;

const FLAG_CAUSATION: u8 = 1 << 0;
const FLAG_DEADLINE: u8 = 1 << 1;
const KNOWN_FLAGS: u8 = FLAG_CAUSATION | FLAG_DEADLINE;

/// Location-transparent logical delivery target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeliveryTarget {
    LocalActor { actor_id: u64 },
    RemoteActor { node_id: u64, actor_id: u64 },
}

impl DeliveryTarget {
    pub const fn actor_id(self) -> u64 {
        match self {
            DeliveryTarget::LocalActor { actor_id }
            | DeliveryTarget::RemoteActor { actor_id, .. } => actor_id,
        }
    }
}

/// Priority carried by a logical delivery independently of mailbox layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryPriority {
    System,
    Normal,
    Bulk,
}

/// Logical message envelope shared by local, remote, durable, and retry paths.
///
/// `T` is intentionally generic. Runtime mailboxes may use `Arc<Vec<Value>>`,
/// wire codecs may use bytes, and persistence adapters may use a persisted
/// payload representation while sharing the same identity/causality contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryEnvelope<T> {
    pub meta: MessageMeta,
    pub sender_actor: u64,
    pub target: DeliveryTarget,
    pub behavior: String,
    pub priority: DeliveryPriority,
    pub trace_context: Option<String>,
    pub payload: T,
}

impl<T: Clone> DeliveryEnvelope<T> {
    /// Another attempt to deliver the same logical message.
    ///
    /// Identity, correlation, causation, target, behavior, and payload stay
    /// unchanged. Only the delivery-attempt counter changes.
    pub fn retry(&self) -> Self {
        let mut retry = self.clone();
        retry.meta = self.meta.retry();
        retry
    }
}

impl<T> DeliveryEnvelope<T> {
    pub fn is_expired_at(&self, now_unix_ms: u64) -> bool {
        self.meta
            .deadline_unix_ms
            .is_some_and(|deadline| now_unix_ms >= deadline)
    }
}

/// Binary codec failure for versioned logical delivery metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageMetaCodecError {
    WrongLength { expected: usize, actual: usize },
    BadMagic([u8; 4]),
    UnsupportedVersion(u8),
    UnknownFlags(u8),
}

impl fmt::Display for MessageMetaCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MessageMetaCodecError::WrongLength { expected, actual } => {
                write!(f, "delivery metadata length {actual}, expected {expected}")
            }
            MessageMetaCodecError::BadMagic(magic) => {
                write!(f, "invalid delivery metadata magic {magic:?}")
            }
            MessageMetaCodecError::UnsupportedVersion(version) => {
                write!(f, "unsupported delivery metadata version {version}")
            }
            MessageMetaCodecError::UnknownFlags(flags) => {
                write!(f, "delivery metadata contains unknown flags 0x{flags:02x}")
            }
        }
    }
}

impl std::error::Error for MessageMetaCodecError {}

/// Encode logical message metadata into the fixed v1 representation.
///
/// Optional fields occupy fixed slots and are activated by flags. Fixed-size
/// encoding keeps packet framing deterministic and lets a future NUL0 version
/// prepend/append this block without ambiguity.
pub fn encode_message_meta(meta: &MessageMeta) -> [u8; MESSAGE_META_WIRE_LEN] {
    let mut out = [0u8; MESSAGE_META_WIRE_LEN];
    out[..4].copy_from_slice(&MESSAGE_META_MAGIC);
    out[4] = MESSAGE_META_WIRE_VERSION;

    let mut flags = 0u8;
    if meta.causation_id.is_some() {
        flags |= FLAG_CAUSATION;
    }
    if meta.deadline_unix_ms.is_some() {
        flags |= FLAG_DEADLINE;
    }
    out[5] = flags;
    out[6..22].copy_from_slice(&meta.id.to_bytes());
    out[22..38].copy_from_slice(&meta.correlation_id.to_bytes());
    if let Some(causation) = meta.causation_id {
        out[38..54].copy_from_slice(&causation.to_bytes());
    }
    out[54..58].copy_from_slice(&meta.attempt.to_be_bytes());
    if let Some(deadline) = meta.deadline_unix_ms {
        out[58..66].copy_from_slice(&deadline.to_be_bytes());
    }
    out
}

/// Decode delivery metadata, failing closed on unknown versions/flags.
pub fn decode_message_meta(bytes: &[u8]) -> Result<MessageMeta, MessageMetaCodecError> {
    if bytes.len() != MESSAGE_META_WIRE_LEN {
        return Err(MessageMetaCodecError::WrongLength {
            expected: MESSAGE_META_WIRE_LEN,
            actual: bytes.len(),
        });
    }

    let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if magic != MESSAGE_META_MAGIC {
        return Err(MessageMetaCodecError::BadMagic(magic));
    }
    if bytes[4] != MESSAGE_META_WIRE_VERSION {
        return Err(MessageMetaCodecError::UnsupportedVersion(bytes[4]));
    }
    let flags = bytes[5];
    if flags & !KNOWN_FLAGS != 0 {
        return Err(MessageMetaCodecError::UnknownFlags(flags));
    }

    let mut id = [0u8; 16];
    id.copy_from_slice(&bytes[6..22]);
    let mut correlation = [0u8; 16];
    correlation.copy_from_slice(&bytes[22..38]);
    let causation_id = if flags & FLAG_CAUSATION != 0 {
        let mut causation = [0u8; 16];
        causation.copy_from_slice(&bytes[38..54]);
        Some(MessageId::from_bytes(causation))
    } else {
        None
    };
    let attempt = u32::from_be_bytes([bytes[54], bytes[55], bytes[56], bytes[57]]);
    let deadline_unix_ms = if flags & FLAG_DEADLINE != 0 {
        Some(u64::from_be_bytes([
            bytes[58], bytes[59], bytes[60], bytes[61], bytes[62], bytes[63], bytes[64],
            bytes[65],
        ]))
    } else {
        None
    };

    Ok(MessageMeta {
        id: MessageId::from_bytes(id),
        correlation_id: MessageId::from_bytes(correlation),
        causation_id,
        attempt,
        deadline_unix_ms,
    })
}

/// Machine-readable reason an otherwise valid logical delivery was not
/// processed by its intended recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum DeadLetterReason {
    Unresolvable,
    NodeUnavailable,
    TargetActorMissing,
    MailboxFull,
    DeadlineExpired,
    ProtocolIncompatible,
    CapabilityDenied,
    PayloadEncoding,
    BehaviorUnavailable,
    RetryExhausted,
    SpawnRejected,
    Other(String),
}

/// Structured dead-letter record suitable for logs, metrics, persistence, or
/// a future system DLQ actor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadLetter {
    pub meta: MessageMeta,
    pub sender_actor: u64,
    pub target: DeliveryTarget,
    pub behavior: String,
    pub reason: DeadLetterReason,
    pub failed_at_unix_ms: u64,
    pub trace_context: Option<String>,
}

impl<T> DeliveryEnvelope<T> {
    pub fn into_dead_letter(
        self,
        reason: DeadLetterReason,
        failed_at_unix_ms: u64,
    ) -> DeadLetter {
        DeadLetter {
            meta: self.meta,
            sender_actor: self.sender_actor,
            target: self.target,
            behavior: self.behavior,
            reason,
            failed_at_unix_ms,
            trace_context: self.trace_context,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadLetterPush {
    TrackingDisabled,
    Recorded { evicted: Option<DeadLetter> },
}

/// Exact bounded FIFO of structured dead letters.
///
/// This is intentionally separate from the existing `dlq_actor_id`: the
/// runtime can first record a structured diagnostic and optionally fan that
/// record out to a system actor later. Capacity zero disables retention.
#[derive(Debug, Clone)]
pub struct DeadLetterQueue {
    capacity: usize,
    entries: VecDeque<DeadLetter>,
}

impl DeadLetterQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::with_capacity(capacity),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn push(&mut self, letter: DeadLetter) -> DeadLetterPush {
        if self.capacity == 0 {
            return DeadLetterPush::TrackingDisabled;
        }
        let evicted = if self.entries.len() == self.capacity {
            self.entries.pop_front()
        } else {
            None
        };
        self.entries.push_back(letter);
        DeadLetterPush::Recorded { evicted }
    }

    pub fn iter(&self) -> impl Iterator<Item = &DeadLetter> {
        self.entries.iter()
    }

    pub fn drain(&mut self) -> impl Iterator<Item = DeadLetter> + '_ {
        self.entries.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> MessageMeta {
        MessageMeta {
            id: MessageId::new(7, 11),
            correlation_id: MessageId::new(7, 1),
            causation_id: Some(MessageId::new(7, 10)),
            attempt: 3,
            deadline_unix_ms: Some(55_000),
        }
    }

    fn envelope(sequence: u64) -> DeliveryEnvelope<Vec<u8>> {
        DeliveryEnvelope {
            meta: MessageMeta::root(MessageId::new(9, sequence)),
            sender_actor: 5,
            target: DeliveryTarget::RemoteActor {
                node_id: 2,
                actor_id: 99,
            },
            behavior: "Order.submit".into(),
            priority: DeliveryPriority::Normal,
            trace_context: Some("00-trace-parent-01".into()),
            payload: vec![1, 2, 3],
        }
    }

    #[test]
    fn metadata_wire_round_trip_preserves_identity_and_causality() {
        let expected = meta();
        let encoded = encode_message_meta(&expected);
        assert_eq!(encoded.len(), MESSAGE_META_WIRE_LEN);
        assert_eq!(decode_message_meta(&encoded).unwrap(), expected);
    }

    #[test]
    fn metadata_codec_supports_absent_optional_fields() {
        let expected = MessageMeta::root(MessageId::new(1, 2));
        let encoded = encode_message_meta(&expected);
        assert_eq!(decode_message_meta(&encoded).unwrap(), expected);
    }

    #[test]
    fn metadata_codec_fails_closed_on_future_version_and_unknown_flags() {
        let mut encoded = encode_message_meta(&meta());
        encoded[4] = MESSAGE_META_WIRE_VERSION + 1;
        assert_eq!(
            decode_message_meta(&encoded),
            Err(MessageMetaCodecError::UnsupportedVersion(2))
        );

        let mut encoded = encode_message_meta(&meta());
        encoded[5] |= 0x80;
        assert_eq!(
            decode_message_meta(&encoded),
            Err(MessageMetaCodecError::UnknownFlags(encoded[5]))
        );
    }

    #[test]
    fn metadata_codec_rejects_truncated_frame() {
        let encoded = encode_message_meta(&meta());
        assert_eq!(
            decode_message_meta(&encoded[..MESSAGE_META_WIRE_LEN - 1]),
            Err(MessageMetaCodecError::WrongLength {
                expected: MESSAGE_META_WIRE_LEN,
                actual: MESSAGE_META_WIRE_LEN - 1,
            })
        );
    }

    #[test]
    fn retry_keeps_logical_identity_and_increments_attempt() {
        let original = envelope(1);
        let retry = original.retry();
        assert_eq!(retry.meta.id, original.meta.id);
        assert_eq!(retry.meta.correlation_id, original.meta.correlation_id);
        assert_eq!(retry.meta.attempt, original.meta.attempt + 1);
        assert_eq!(retry.target, original.target);
        assert_eq!(retry.payload, original.payload);
    }

    #[test]
    fn deadline_boundary_is_expired() {
        let mut delivery = envelope(1);
        delivery.meta.deadline_unix_ms = Some(100);
        assert!(!delivery.is_expired_at(99));
        assert!(delivery.is_expired_at(100));
        assert!(delivery.is_expired_at(101));
    }

    #[test]
    fn structured_dead_letter_keeps_original_message_identity() {
        let delivery = envelope(44);
        let id = delivery.meta.id;
        let letter = delivery.into_dead_letter(DeadLetterReason::MailboxFull, 123);
        assert_eq!(letter.meta.id, id);
        assert_eq!(letter.reason, DeadLetterReason::MailboxFull);
        assert_eq!(letter.failed_at_unix_ms, 123);
    }

    #[test]
    fn dead_letter_queue_is_bounded_fifo() {
        let mut queue = DeadLetterQueue::new(2);
        let a = envelope(1).into_dead_letter(DeadLetterReason::Unresolvable, 1);
        let b = envelope(2).into_dead_letter(DeadLetterReason::NodeUnavailable, 2);
        let c = envelope(3).into_dead_letter(DeadLetterReason::MailboxFull, 3);

        assert_eq!(
            queue.push(a.clone()),
            DeadLetterPush::Recorded { evicted: None }
        );
        assert_eq!(
            queue.push(b.clone()),
            DeadLetterPush::Recorded { evicted: None }
        );
        assert_eq!(
            queue.push(c.clone()),
            DeadLetterPush::Recorded {
                evicted: Some(a.clone())
            }
        );
        assert_eq!(queue.iter().cloned().collect::<Vec<_>>(), vec![b, c]);
    }

    #[test]
    fn zero_capacity_disables_dead_letter_retention() {
        let mut queue = DeadLetterQueue::new(0);
        let letter = envelope(1).into_dead_letter(DeadLetterReason::Unresolvable, 1);
        assert_eq!(queue.push(letter), DeadLetterPush::TrackingDisabled);
        assert!(queue.is_empty());
    }
}
