//! Rolling-compatible carrier for FAB1 metadata chunks over the existing FAB0
//! gossip envelope.
//!
//! The canonical metadata payload remains the compact binary FAB1 frame. For
//! NUL0-v1 rolling upgrades, that frame is hex-encoded into one reserved FAB0
//! advertisement whose subject is deliberately invalid under normal Fabric
//! subscription validation. New peers recognize the reserved envelope before
//! route validation; old peers reject it fail-closed and preserve their last
//! complete routing generation.

use super::{
    fabric_metadata_wire::decode_fab1_chunk, FabricAdvertisement, FabricAdvertisementSnapshot,
    FABRIC_METADATA_CHUNK_BYTES, FABRIC_METADATA_MAX_CHUNKS,
};
use crate::runtime::{NodeId, Runtime};

pub(crate) const FAB1_TUNNEL_PATTERN: &str = "__nulang_fab1..metadata";
const FAB1_TUNNEL_PREFIX: &str = "FAB1HEX:";

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for &byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_hex(encoded: &str) -> Result<Vec<u8>, String> {
    let bytes = encoded.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err("FAB1 tunnel hex payload has odd length".to_string());
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = decode_nibble(pair[0])
            .ok_or_else(|| "FAB1 tunnel contains non-hex data".to_string())?;
        let low = decode_nibble(pair[1])
            .ok_or_else(|| "FAB1 tunnel contains non-hex data".to_string())?;
        decoded.push((high << 4) | low);
    }
    Ok(decoded)
}

pub(crate) fn is_fab1_tunnel(snapshot: &FabricAdvertisementSnapshot) -> bool {
    snapshot.subscriptions.len() == 1
        && snapshot.subscriptions[0].pattern == FAB1_TUNNEL_PATTERN
        && snapshot.subscriptions[0]
            .behavior
            .starts_with(FAB1_TUNNEL_PREFIX)
}

impl Runtime {
    /// Build the next rolling-compatible FAB1 carrier snapshot for the current
    /// local subscription generation.
    pub(crate) fn fabric_next_advertisement_tunnel(
        &mut self,
    ) -> Result<Option<FabricAdvertisementSnapshot>, String> {
        let Some(bytes) = self.fabric_next_advertisement_chunk_bytes()? else {
            return Ok(None);
        };
        let (chunk, consumed) = decode_fab1_chunk(
            &bytes,
            FABRIC_METADATA_CHUNK_BYTES,
            FABRIC_METADATA_MAX_CHUNKS,
        )
        .map_err(|error| format!("cannot inspect locally encoded FAB1 chunk: {error:?}"))?;
        if consumed != bytes.len() {
            return Err("locally encoded FAB1 chunk contains trailing bytes".to_string());
        }
        let node_id = NodeId(chunk.owner);
        if self.distributed.node_id != Some(node_id) {
            return Err("FAB1 tunnel owner does not match local node identity".to_string());
        }

        Ok(Some(FabricAdvertisementSnapshot {
            node_id,
            generation: chunk.generation,
            subscriptions: vec![FabricAdvertisement {
                node_id,
                pattern: FAB1_TUNNEL_PATTERN.to_string(),
                // Keep this non-zero even though new peers intercept before
                // ordinary validation. Old peers fail earlier on the invalid
                // pattern and therefore cannot install a bogus route.
                actor_id: chunk.chunk_index as u64 + 1,
                behavior: format!("{FAB1_TUNNEL_PREFIX}{}", encode_hex(&bytes)),
                group: None,
            }],
        }))
    }

    /// Recognize and consume a reserved FAB1 tunnel snapshot. `None` means the
    /// snapshot is an ordinary FAB0 subscription generation and should follow
    /// the existing replacement path.
    pub(crate) fn fabric_try_accept_advertisement_tunnel(
        &mut self,
        snapshot: &FabricAdvertisementSnapshot,
    ) -> Option<Result<usize, String>> {
        if !is_fab1_tunnel(snapshot) {
            return None;
        }

        let advertisement = &snapshot.subscriptions[0];
        if advertisement.node_id != snapshot.node_id {
            return Some(Err("FAB1 tunnel advertisement owner mismatch".to_string()));
        }
        let encoded = advertisement
            .behavior
            .strip_prefix(FAB1_TUNNEL_PREFIX)
            .expect("tunnel predicate checked the prefix");
        let bytes = match decode_hex(encoded) {
            Ok(bytes) => bytes,
            Err(error) => return Some(Err(error)),
        };
        let (chunk, consumed) = match decode_fab1_chunk(
            &bytes,
            FABRIC_METADATA_CHUNK_BYTES,
            FABRIC_METADATA_MAX_CHUNKS,
        ) {
            Ok(decoded) => decoded,
            Err(error) => {
                return Some(Err(format!("invalid tunneled FAB1 chunk: {error:?}")));
            }
        };
        if consumed != bytes.len() {
            return Some(Err(
                "tunneled FAB1 chunk contains unexpected trailing bytes".to_string(),
            ));
        }
        if chunk.owner != snapshot.node_id.0 {
            return Some(Err("tunneled FAB1 owner mismatch".to_string()));
        }
        if chunk.generation != snapshot.generation {
            return Some(Err("tunneled FAB1 generation mismatch".to_string()));
        }

        Some(self.fabric_accept_remote_metadata_chunk_bytes(snapshot.node_id, &bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_is_exact_and_case_tolerant() {
        let bytes = b"FAB1\x00\x7f\x80\xff";
        let encoded = encode_hex(bytes);
        assert_eq!(decode_hex(&encoded).unwrap(), bytes);
        assert_eq!(decode_hex(&encoded.to_uppercase()).unwrap(), bytes);
    }

    #[test]
    fn malformed_hex_fails_closed() {
        assert!(decode_hex("abc").is_err());
        assert!(decode_hex("zz").is_err());
    }
}
