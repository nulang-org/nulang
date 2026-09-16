//! Compatibility-safe NUL0 ActorMessage v2 framing helpers.
//!
//! The eventual NUL0 v2 ActorMessage payload is deliberately defined as:
//!
//! ```text
//! [0..66]   fixed MessageMeta v1 block (NDM1)
//! [66..]    existing ActorMessage v1 payload bytes, unchanged
//! ```
//!
//! Keeping the existing payload byte-for-byte after the metadata prefix makes
//! the transport migration mechanical: bump `WIRE_VERSION`, prepend metadata
//! on write, strip/decode it on read, then run the existing payload codec.
//! Until `network.rs` is switched atomically, `WIRE_VERSION` must remain 1.

use crate::delivery::{
    decode_message_meta, encode_message_meta, MessageMetaCodecError, MESSAGE_META_WIRE_LEN,
};
use crate::message::MessageMeta;
use std::fmt;

pub const ACTOR_MESSAGE_V2_META_PREFIX_LEN: usize = MESSAGE_META_WIRE_LEN;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorMessageV2CodecError {
    TooShort { minimum: usize, actual: usize },
    Metadata(MessageMetaCodecError),
}

impl fmt::Display for ActorMessageV2CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { minimum, actual } => {
                write!(f, "actor-message v2 payload is {actual} bytes, minimum is {minimum}")
            }
            Self::Metadata(error) => write!(f, "actor-message v2 metadata: {error}"),
        }
    }
}

impl std::error::Error for ActorMessageV2CodecError {}

impl From<MessageMetaCodecError> for ActorMessageV2CodecError {
    fn from(value: MessageMetaCodecError) -> Self {
        Self::Metadata(value)
    }
}

/// Prefix an already-encoded v1 ActorMessage payload with stable logical
/// delivery metadata. The legacy bytes are not inspected or rewritten.
pub fn encode_actor_message_v2(meta: &MessageMeta, legacy_payload: &[u8]) -> Vec<u8> {
    let encoded_meta = encode_message_meta(meta);
    let mut out = Vec::with_capacity(ACTOR_MESSAGE_V2_META_PREFIX_LEN + legacy_payload.len());
    out.extend_from_slice(&encoded_meta);
    out.extend_from_slice(legacy_payload);
    out
}

/// Decode the fixed metadata prefix and return the untouched legacy payload.
///
/// This function borrows the payload tail so the transport can pass it into
/// the existing ActorMessage decoder without copying it.
pub fn decode_actor_message_v2(
    bytes: &[u8],
) -> Result<(MessageMeta, &[u8]), ActorMessageV2CodecError> {
    if bytes.len() < ACTOR_MESSAGE_V2_META_PREFIX_LEN {
        return Err(ActorMessageV2CodecError::TooShort {
            minimum: ACTOR_MESSAGE_V2_META_PREFIX_LEN,
            actual: bytes.len(),
        });
    }
    let meta = decode_message_meta(&bytes[..ACTOR_MESSAGE_V2_META_PREFIX_LEN])?;
    Ok((meta, &bytes[ACTOR_MESSAGE_V2_META_PREFIX_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{MessageId, MessageMeta};

    fn meta() -> MessageMeta {
        MessageMeta {
            id: MessageId::new(9, 44),
            correlation_id: MessageId::new(9, 1),
            causation_id: Some(MessageId::new(9, 43)),
            attempt: 2,
            deadline_unix_ms: Some(123_456),
        }
    }

    #[test]
    fn v2_prefix_round_trip_leaves_legacy_payload_unchanged() {
        let legacy = b"legacy-actor-message-payload\x00\x01\xff";
        let expected_meta = meta();
        let encoded = encode_actor_message_v2(&expected_meta, legacy);
        let (decoded_meta, decoded_legacy) = decode_actor_message_v2(&encoded).unwrap();
        assert_eq!(decoded_meta, expected_meta);
        assert_eq!(decoded_legacy, legacy);
    }

    #[test]
    fn retry_metadata_round_trip_keeps_same_logical_id() {
        let original = meta();
        let retry = original.retry();
        let encoded = encode_actor_message_v2(&retry, b"payload");
        let (decoded, _) = decode_actor_message_v2(&encoded).unwrap();
        assert_eq!(decoded.id, original.id);
        assert_eq!(decoded.correlation_id, original.correlation_id);
        assert_eq!(decoded.attempt, original.attempt + 1);
    }

    #[test]
    fn truncated_prefix_fails_closed() {
        let bytes = vec![0u8; ACTOR_MESSAGE_V2_META_PREFIX_LEN - 1];
        assert_eq!(
            decode_actor_message_v2(&bytes),
            Err(ActorMessageV2CodecError::TooShort {
                minimum: ACTOR_MESSAGE_V2_META_PREFIX_LEN,
                actual: ACTOR_MESSAGE_V2_META_PREFIX_LEN - 1,
            })
        );
    }

    #[test]
    fn corrupt_metadata_is_not_treated_as_legacy_payload() {
        let mut encoded = encode_actor_message_v2(&meta(), b"payload");
        encoded[0] = b'X';
        assert!(matches!(
            decode_actor_message_v2(&encoded),
            Err(ActorMessageV2CodecError::Metadata(
                MessageMetaCodecError::BadMagic(_)
            ))
        ));
    }
}
