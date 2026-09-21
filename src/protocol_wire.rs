//! Versioned wire envelopes for protocol-typed actor references and schemas.
//!
//! The current NUL0 transport format is versioned and must not be changed
//! implicitly. This module defines additive compatibility boundaries that can
//! accompany distributed messages today and be integrated into a future NUL0
//! wire-version migration without coupling protocol identity to VM layouts.

use crate::protocol::{
    ProtocolActorRef, ProtocolId, ProtocolMember, ProtocolMismatch, ProtocolSchema,
    ProtocolSchemaError, ProtocolTypeId,
};
use std::error::Error;
use std::fmt;

pub const PROTOCOL_WIRE_MAGIC: [u8; 4] = *b"NUPR";
pub const PROTOCOL_WIRE_VERSION: u16 = 1;
pub const PROTOCOL_WIRE_LEN: usize = 4 + 2 + 8 + 8 + 32;

/// Separate, self-describing envelope for a canonical protocol schema.
///
/// This intentionally does not alter the frozen NUL0 packet header. A runtime
/// can exchange/cache NUPS descriptors out of band and keep using the compact
/// protocol id on ordinary messages.
pub const PROTOCOL_SCHEMA_WIRE_MAGIC: [u8; 4] = *b"NUPS";
pub const PROTOCOL_SCHEMA_WIRE_VERSION: u16 = 1;

/// Defensive parser limits. Protocol descriptions are metadata, never an
/// unbounded application payload.
pub const MAX_PROTOCOL_SCHEMA_WIRE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_PROTOCOL_SCHEMA_NAME_BYTES: usize = 4 * 1024;
pub const MAX_PROTOCOL_BEHAVIOR_NAME_BYTES: usize = 1024;
pub const MAX_PROTOCOL_SCHEMA_MEMBERS: usize = 16_384;
pub const MAX_PROTOCOL_MEMBER_PARAMS: usize = 1024;

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
/// Length, magic, and version are validated before the reference is returned.
/// Protocol ids are cryptographic digests, so every 32-byte value is a valid
/// representation; semantic trust comes from schema/admission verification.
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
    let protocol_id = ProtocolId::from_bytes(
        bytes[22..54]
            .try_into()
            .expect("validated fixed protocol-id length"),
    );

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

/// Encode a canonical protocol schema for distributed compatibility discovery.
///
/// The descriptor contains only semantic identities: behavior names plus
/// canonical type/signature digests. Runtime ids, bytecode offsets, memory
/// layouts, source locations, and transport addresses never enter the format.
///
/// The schema's computed ProtocolId is embedded and verified on decode. This
/// makes a received descriptor safe to place in a local ProtocolRegistry after
/// transport/authentication policy accepts its source.
pub fn encode_protocol_schema(schema: &ProtocolSchema) -> Result<Vec<u8>, ProtocolWireError> {
    if schema.name.as_bytes().len() > MAX_PROTOCOL_SCHEMA_NAME_BYTES {
        return Err(ProtocolWireError::SchemaLimitExceeded("schema name"));
    }

    let members: Vec<&ProtocolMember> = schema.members().collect();
    if members.len() > MAX_PROTOCOL_SCHEMA_MEMBERS {
        return Err(ProtocolWireError::SchemaLimitExceeded("member count"));
    }

    let mut out = Vec::with_capacity(64 + members.len() * 128);
    out.extend_from_slice(&PROTOCOL_SCHEMA_WIRE_MAGIC);
    out.extend_from_slice(&PROTOCOL_SCHEMA_WIRE_VERSION.to_be_bytes());
    out.extend_from_slice(schema.id().as_bytes());
    put_string(&mut out, &schema.name)?;

    put_u32(&mut out, members.len())?;
    for member in members {
        if member.behavior.as_bytes().len() > MAX_PROTOCOL_BEHAVIOR_NAME_BYTES {
            return Err(ProtocolWireError::SchemaLimitExceeded("behavior name"));
        }
        if member.params.len() > MAX_PROTOCOL_MEMBER_PARAMS {
            return Err(ProtocolWireError::SchemaLimitExceeded("parameter count"));
        }

        put_string(&mut out, &member.behavior)?;
        put_u32(&mut out, member.params.len())?;
        for param in &member.params {
            out.extend_from_slice(param.as_bytes());
        }
        out.extend_from_slice(member.response.as_bytes());
        out.extend_from_slice(member.signature.as_bytes());

        if out.len() > MAX_PROTOCOL_SCHEMA_WIRE_BYTES {
            return Err(ProtocolWireError::SchemaTooLarge { actual: out.len() });
        }
    }

    Ok(out)
}

/// Decode and verify one canonical protocol schema descriptor.
///
/// The declared ProtocolId is recomputed from the decoded canonical member
/// contracts. A corrupt or forged descriptor therefore fails closed before it
/// can influence rolling-upgrade compatibility decisions.
pub fn decode_protocol_schema(bytes: &[u8]) -> Result<ProtocolSchema, ProtocolWireError> {
    if bytes.len() > MAX_PROTOCOL_SCHEMA_WIRE_BYTES {
        return Err(ProtocolWireError::SchemaTooLarge { actual: bytes.len() });
    }

    let mut cursor = WireCursor::new(bytes);
    if cursor.read_exact::<4>()? != PROTOCOL_SCHEMA_WIRE_MAGIC {
        return Err(ProtocolWireError::InvalidSchemaMagic);
    }

    let version = cursor.read_u16()?;
    if version != PROTOCOL_SCHEMA_WIRE_VERSION {
        return Err(ProtocolWireError::UnsupportedSchemaVersion { actual: version });
    }

    let declared_id = ProtocolId::from_bytes(cursor.read_exact::<32>()?);
    let name = cursor.read_string(MAX_PROTOCOL_SCHEMA_NAME_BYTES, "schema name")?;

    let member_count = cursor.read_u32()? as usize;
    if member_count > MAX_PROTOCOL_SCHEMA_MEMBERS {
        return Err(ProtocolWireError::SchemaLimitExceeded("member count"));
    }

    let mut members = Vec::with_capacity(member_count);
    for _ in 0..member_count {
        let behavior =
            cursor.read_string(MAX_PROTOCOL_BEHAVIOR_NAME_BYTES, "behavior name")?;
        let param_count = cursor.read_u32()? as usize;
        if param_count > MAX_PROTOCOL_MEMBER_PARAMS {
            return Err(ProtocolWireError::SchemaLimitExceeded("parameter count"));
        }

        let mut params = Vec::with_capacity(param_count);
        for _ in 0..param_count {
            params.push(ProtocolTypeId::from_bytes(cursor.read_exact::<32>()?));
        }
        let response = ProtocolTypeId::from_bytes(cursor.read_exact::<32>()?);
        let signature = ProtocolTypeId::from_bytes(cursor.read_exact::<32>()?);
        members.push(ProtocolMember {
            behavior,
            params,
            response,
            signature,
        });
    }

    if !cursor.is_finished() {
        return Err(ProtocolWireError::TrailingSchemaBytes {
            actual: cursor.remaining(),
        });
    }

    let schema = ProtocolSchema::new(name, members).map_err(ProtocolWireError::SchemaDefinition)?;
    let actual_id = schema.id();
    if actual_id != declared_id {
        return Err(ProtocolWireError::SchemaDigestMismatch {
            declared: declared_id,
            actual: actual_id,
        });
    }

    Ok(schema)
}

fn put_u32(out: &mut Vec<u8>, value: usize) -> Result<(), ProtocolWireError> {
    let value = u32::try_from(value)
        .map_err(|_| ProtocolWireError::SchemaLimitExceeded("32-bit length"))?;
    out.extend_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_string(out: &mut Vec<u8>, value: &str) -> Result<(), ProtocolWireError> {
    put_u32(out, value.as_bytes().len())?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

struct WireCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> WireCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], ProtocolWireError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or(ProtocolWireError::TruncatedSchema)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(ProtocolWireError::TruncatedSchema)?;
        self.offset = end;
        Ok(slice.try_into().expect("validated exact slice length"))
    }

    fn read_u16(&mut self) -> Result<u16, ProtocolWireError> {
        Ok(u16::from_be_bytes(self.read_exact::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32, ProtocolWireError> {
        Ok(u32::from_be_bytes(self.read_exact::<4>()?))
    }

    fn read_string(
        &mut self,
        max_len: usize,
        field: &'static str,
    ) -> Result<String, ProtocolWireError> {
        let len = self.read_u32()? as usize;
        if len > max_len {
            return Err(ProtocolWireError::SchemaLimitExceeded(field));
        }
        let end = self
            .offset
            .checked_add(len)
            .ok_or(ProtocolWireError::TruncatedSchema)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(ProtocolWireError::TruncatedSchema)?;
        self.offset = end;
        let text = std::str::from_utf8(slice)
            .map_err(|_| ProtocolWireError::InvalidSchemaUtf8(field))?;
        Ok(text.to_owned())
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn is_finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolWireError {
    InvalidLength { actual: usize },
    InvalidMagic,
    UnsupportedVersion { actual: u16 },
    ProtocolMismatch(ProtocolMismatch),
    SchemaTooLarge { actual: usize },
    InvalidSchemaMagic,
    UnsupportedSchemaVersion { actual: u16 },
    TruncatedSchema,
    InvalidSchemaUtf8(&'static str),
    SchemaLimitExceeded(&'static str),
    TrailingSchemaBytes { actual: usize },
    SchemaDefinition(ProtocolSchemaError),
    SchemaDigestMismatch {
        declared: ProtocolId,
        actual: ProtocolId,
    },
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
            Self::ProtocolMismatch(error) => error.fmt(f),
            Self::SchemaTooLarge { actual } => write!(
                f,
                "protocol schema envelope exceeds {MAX_PROTOCOL_SCHEMA_WIRE_BYTES} bytes: {actual}"
            ),
            Self::InvalidSchemaMagic => f.write_str("invalid protocol schema envelope magic"),
            Self::UnsupportedSchemaVersion { actual } => write!(
                f,
                "unsupported protocol schema envelope version {actual}; runtime supports version {PROTOCOL_SCHEMA_WIRE_VERSION}"
            ),
            Self::TruncatedSchema => f.write_str("truncated protocol schema envelope"),
            Self::InvalidSchemaUtf8(field) => {
                write!(f, "protocol schema {field} is not valid UTF-8")
            }
            Self::SchemaLimitExceeded(field) => {
                write!(f, "protocol schema {field} exceeds the wire-format limit")
            }
            Self::TrailingSchemaBytes { actual } => {
                write!(f, "protocol schema envelope has {actual} trailing bytes")
            }
            Self::SchemaDefinition(error) => {
                write!(f, "invalid protocol schema definition: {error}")
            }
            Self::SchemaDigestMismatch { declared, actual } => write!(
                f,
                "protocol schema digest mismatch: declared {declared}, recomputed {actual}"
            ),
        }
    }
}

impl Error for ProtocolWireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProtocolMismatch(error) => Some(error),
            Self::SchemaDefinition(error) => Some(error),
            Self::InvalidLength { .. }
            | Self::InvalidMagic
            | Self::UnsupportedVersion { .. }
            | Self::SchemaTooLarge { .. }
            | Self::InvalidSchemaMagic
            | Self::UnsupportedSchemaVersion { .. }
            | Self::TruncatedSchema
            | Self::InvalidSchemaUtf8(_)
            | Self::SchemaLimitExceeded(_)
            | Self::TrailingSchemaBytes { .. }
            | Self::SchemaDigestMismatch { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Capability, Effect, EffectRow, Type};

    fn protocol(byte: u8) -> ProtocolId {
        ProtocolId::from_bytes([byte; 32])
    }

    fn schema(order_reversed: bool) -> ProtocolSchema {
        let get = ProtocolMember::behavior(
            "get",
            vec![],
            Type::int(),
            EffectRow::empty(),
            Capability::Tag,
        )
        .unwrap();
        let set = ProtocolMember::behavior(
            "set",
            vec![Type::int()],
            Type::unit(),
            EffectRow::Closed(vec![Effect::Send]),
            Capability::Tag,
        )
        .unwrap();

        let members = if order_reversed {
            vec![set, get]
        } else {
            vec![get, set]
        };
        ProtocolSchema::new("Counter", members).unwrap()
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

    #[test]
    fn schema_wire_round_trip_preserves_canonical_identity() {
        let original = schema(false);
        let encoded = encode_protocol_schema(&original).unwrap();
        let decoded = decode_protocol_schema(&encoded).unwrap();

        assert_eq!(decoded, original);
        assert_eq!(decoded.id(), original.id());
    }

    #[test]
    fn schema_wire_encoding_is_declaration_order_independent() {
        let a = schema(false);
        let b = schema(true);
        assert_eq!(a.id(), b.id());
        assert_eq!(
            encode_protocol_schema(&a).unwrap(),
            encode_protocol_schema(&b).unwrap()
        );
    }

    #[test]
    fn schema_wire_rejects_tampered_member_contract() {
        let original = schema(false);
        let mut encoded = encode_protocol_schema(&original).unwrap();

        // Flip one byte in the final signature digest while leaving the
        // declared ProtocolId intact.
        let last = encoded.len() - 1;
        encoded[last] ^= 0x01;

        assert!(matches!(
            decode_protocol_schema(&encoded),
            Err(ProtocolWireError::SchemaDigestMismatch { .. })
        ));
    }

    #[test]
    fn schema_wire_rejects_trailing_data() {
        let original = schema(false);
        let mut encoded = encode_protocol_schema(&original).unwrap();
        encoded.push(0);

        assert_eq!(
            decode_protocol_schema(&encoded).unwrap_err(),
            ProtocolWireError::TrailingSchemaBytes { actual: 1 }
        );
    }

    #[test]
    fn schema_wire_rejects_unknown_version() {
        let original = schema(false);
        let mut encoded = encode_protocol_schema(&original).unwrap();
        encoded[4..6].copy_from_slice(&2u16.to_be_bytes());

        assert_eq!(
            decode_protocol_schema(&encoded).unwrap_err(),
            ProtocolWireError::UnsupportedSchemaVersion { actual: 2 }
        );
    }
}
