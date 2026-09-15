//! Versioned wire envelope for protocol-typed actor references.
//!
//! The current NUL0 transport format is versioned and must not be changed
//! implicitly. This module defines an additive compatibility boundary that can
//! accompany distributed messages today and be integrated into a future NUL0
//! wire-version migration without changing protocol identity semantics.

use crate::protocol::{ProtocolActorRef, ProtocolId, ProtocolIdParseError, ProtocolMismatch};
use std::error::Error;
use std::fmt;
use std::str::FromStr;

pub const PROTOCOL_WIRE_MAGIC: [u8; 4] = *b"NUPR";
pub const PROTOCOL_WIRE_VERSION: u16 = 1;
pub const PROTOCOL_WIRE_LEN: usize = 4 + 2 + 8 + 8 + 32;

/// Encode one protocol-typed actor reference into the fixed v1 envelope.
pub fn encode_protocol_actor_ref(reference: ProtocolActorRef) -> [u8; PROTOCOL_WIRE_LEN] {
    let mut out = [0u8; PROTOCOL_WIRE_LEN];
    out[0..4].copy_from_slice(&PROTOCOL_WIRE_MAGIC);
    out[4..6].copy_from_slice(&PROTOCOL_WIRE_VERSION.to_be_bytes());
    out[6..14].copy_from_slice(&reference.node_id.to_be_bytes());
    out[14..22].copy_from_slice(&reference.actor_id.to_be_bytes());
    out[22..54].copy_from_slice(reference.protocol_id.as_bytes());
    out
}

/// Decode one exact v1 protocol actor reference.
///
/// Length, magic, version, and protocol-id encoding are all validated before
/// the reference is returned. Unknown versions fail closed rather than being
/// interpreted using the current layout.
pub fn decode_protocol_actor_ref(bytes: &[u8]) -> Result<ProtocolActorRef, ProtocolWireError> {
    if bytes.len() != PROTOCOL_WIRE_LEN {
        return Err(ProtocolWireError::InvalidLength {
            actual: bytes.len(),
        });
    }
    if bytes[0..4] != PROTOCOL_WIRE_MAGIC {
        return Err(ProtocolWireError::InvalidMagic);
    }

    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if version != PROTOCOL_WIRE_VERSION {
        return Err(ProtocolWireError::UnsupportedVersion { actual: version });
    }

    let node_id = u64::from_be_bytes(bytes[6..14].try_into().expect("validated fixed length"));
    let actor_id = u64::from_be_bytes(bytes[14..22].try_into().expect("validated fixed length"));
    let protocol_id = protocol_id_from_bytes(&bytes[22..54])?;

    Ok(ProtocolActorRef::new(node_id, actor_id, protocol_id))
}

/// Decode a reference and require the exact protocol expected by the caller.
///
/// This is the intended distributed-dispatch boundary: transport corruption
/// and schema mismatch are distinct, observable failures.
pub fn decode_protocol_actor_ref_for(
    bytes: &[u8],
    expected: ProtocolId,
) -> Result<ProtocolActorRef, ProtocolWireError> {
    let reference = decode_protocol_actor_ref(bytes)?;
    reference
        .require_protocol(expected)
        .map_err(ProtocolWireError::ProtocolMismatch)?;
    Ok(reference)
}

fn protocol_id_from_bytes(bytes: &[u8]) -> Result<ProtocolId, ProtocolWireError> {
    let mut encoded = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    ProtocolId::from_str(&encoded).map_err(ProtocolWireError::InvalidProtocolId)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolWireError {
    InvalidLength { actual: usize },
    InvalidMagic,
    UnsupportedVersion { actual: u16 },
    InvalidProtocolId(ProtocolIdParseError),
    ProtocolMismatch(ProtocolMismatch),
}

impl fmt::Display for ProtocolWireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength { actual } => write!(
                f,
                "protocol actor-ref envelope must be {PROTOCOL_WIRE_LEN} bytes, got {actual}"
            ),
            Self::InvalidMagic => f.write_str("invalid protocol actor-ref envelope magic"),
            Self::UnsupportedVersion { actual } => write!(
                f,
                "unsupported protocol actor-ref envelope version {actual}; runtime supports version {PROTOCOL_WIRE_VERSION}"
            ),
            Self::InvalidProtocolId(error) => {
                write!(f, "invalid protocol id in actor-ref envelope: {error}")
            }
            Self::ProtocolMismatch(error) => error.fmt(f),
        }
    }
}

impl Error for ProtocolWireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidProtocolId(error) => Some(error),
            Self::ProtocolMismatch(error) => Some(error),
            Self::InvalidLength { .. } | Self::InvalidMagic | Self::UnsupportedVersion { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocol(byte: u8) -> ProtocolId {
        let mut text = String::with_capacity(64);
        for _ in 0..32 {
            text.push_str(&format!("{byte:02x}"));
        }
        text.parse().unwrap()
    }

    #[test]
    fn actor_ref_wire_round_trip_is_exact() {
        let reference = ProtocolActorRef::new(7, 42, protocol(0x11));
        let encoded = encode_protocol_actor_ref(reference);
        assert_eq!(encoded.len(), PROTOCOL_WIRE_LEN);
        assert_eq!(decode_protocol_actor_ref(&encoded).unwrap(), reference);
    }

    #[test]
    fn wrong_length_fails_closed() {
        assert_eq!(
            decode_protocol_actor_ref(&[0u8; 8]).unwrap_err(),
            ProtocolWireError::InvalidLength { actual: 8 }
        );
    }

    #[test]
    fn wrong_magic_fails_closed() {
        let reference = ProtocolActorRef::new(7, 42, protocol(0x11));
        let mut encoded = encode_protocol_actor_ref(reference);
        encoded[0] = b'X';
        assert_eq!(
            decode_protocol_actor_ref(&encoded).unwrap_err(),
            ProtocolWireError::InvalidMagic
        );
    }

    #[test]
    fn unknown_version_fails_closed() {
        let reference = ProtocolActorRef::new(7, 42, protocol(0x11));
        let mut encoded = encode_protocol_actor_ref(reference);
        encoded[4..6].copy_from_slice(&2u16.to_be_bytes());
        assert_eq!(
            decode_protocol_actor_ref(&encoded).unwrap_err(),
            ProtocolWireError::UnsupportedVersion { actual: 2 }
        );
    }

    #[test]
    fn exact_protocol_is_required_at_dispatch_boundary() {
        let reference = ProtocolActorRef::new(7, 42, protocol(0x11));
        let encoded = encode_protocol_actor_ref(reference);
        assert_eq!(
            decode_protocol_actor_ref_for(&encoded, protocol(0x11)).unwrap(),
            reference
        );
        assert!(matches!(
            decode_protocol_actor_ref_for(&encoded, protocol(0x22)),
            Err(ProtocolWireError::ProtocolMismatch(_))
        ));
    }
}
