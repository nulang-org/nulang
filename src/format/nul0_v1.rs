//! Frozen NUL0 version-1 handshake codec.
//!
//! The transport/runtime owns sockets, timeouts, connection lifecycle, and
//! packet routing. This module owns only the published 16-byte handshake:
//!
//! ```text
//! 0..4   magic      = "NUL0"
//! 4..8   version    = 1 (u32, big-endian)
//! 8..16  node_id    = u64, big-endian
//! ```
//!
//! Keeping wire bytes here prevents host CPU representation or future runtime
//! internals from becoming part of the protocol contract.

use crate::format::constants::{WIRE_HANDSHAKE_LEN, WIRE_MAGIC, WIRE_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandshakeError {
    BadMagic { got: [u8; 4] },
    UnsupportedVersion { found: u32 },
}

/// Encode the frozen NUL0 v1 handshake in canonical network byte order.
pub(crate) fn encode_handshake(node_id: u64) -> [u8; WIRE_HANDSHAKE_LEN] {
    let mut bytes = [0u8; WIRE_HANDSHAKE_LEN];
    bytes[0..4].copy_from_slice(&WIRE_MAGIC);
    bytes[4..8].copy_from_slice(&WIRE_VERSION.to_be_bytes());
    bytes[8..16].copy_from_slice(&node_id.to_be_bytes());
    bytes
}

/// Decode and validate the frozen NUL0 v1 handshake.
pub(crate) fn decode_handshake(bytes: &[u8; WIRE_HANDSHAKE_LEN]) -> Result<u64, HandshakeError> {
    let got_magic: [u8; 4] = bytes[0..4].try_into().expect("fixed 4-byte slice");
    if got_magic != WIRE_MAGIC {
        return Err(HandshakeError::BadMagic { got: got_magic });
    }

    let version = u32::from_be_bytes(bytes[4..8].try_into().expect("fixed 4-byte slice"));
    if version != WIRE_VERSION {
        return Err(HandshakeError::UnsupportedVersion { found: version });
    }

    Ok(u64::from_be_bytes(
        bytes[8..16].try_into().expect("fixed 8-byte slice"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_handshake_uses_canonical_big_endian_bytes() {
        let bytes = encode_handshake(0x0102_0304_0506_0708);
        assert_eq!(
            bytes,
            [
                b'N', b'U', b'L', b'0', 0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
                0x07, 0x08,
            ]
        );
        assert_eq!(decode_handshake(&bytes), Ok(0x0102_0304_0506_0708));
    }
}
