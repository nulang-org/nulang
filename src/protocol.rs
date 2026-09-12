//! Stable identities for typed actor protocols.
//!
//! Distributed actor references need a schema identity that is independent of
//! source declaration order, formatting, and compiler-internal `Debug` output.
//! This module provides that boundary without changing the existing actor-ref
//! representation yet.
//!
//! `ProtocolTypeId` is derived from a canonical type encoding (for example the
//! typed/Core IR encoding). `ProtocolId` is then derived from the set of
//! behavior signatures. Human-readable protocol names are deliberately not
//! hashed, so a source-level rename does not break wire compatibility when the
//! protocol shape is unchanged.

use blake3::Hasher;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

const TYPE_DOMAIN: &[u8] = b"nulang.protocol.type.v1\0";
const PROTOCOL_DOMAIN: &[u8] = b"nulang.protocol.v1\0";

/// Stable identity of one canonical parameter/response type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolTypeId([u8; 32]);

impl ProtocolTypeId {
    /// Hash a canonical type representation.
    ///
    /// Callers must pass a stable semantic encoding, never source pretty text
    /// or Rust `Debug` output. Keeping this API byte-oriented makes that
    /// requirement explicit and lets the compiler later feed its canonical
    /// typed/Core IR encoding directly.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(TYPE_DOMAIN);
        put_bytes(&mut hasher, bytes);
        Self(*hasher.finalize().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One behavior in an actor protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolMember {
    pub behavior: String,
    pub params: Vec<ProtocolTypeId>,
    /// `None` means fire-and-forget. `Some` means request/reply and hashes the
    /// response type into the protocol identity.
    pub response: Option<ProtocolTypeId>,
}

impl ProtocolMember {
    pub fn message(behavior: impl Into<String>, params: Vec<ProtocolTypeId>) -> Self {
        Self {
            behavior: behavior.into(),
            params,
            response: None,
        }
    }

    pub fn request_reply(
        behavior: impl Into<String>,
        params: Vec<ProtocolTypeId>,
        response: ProtocolTypeId,
    ) -> Self {
        Self {
            behavior: behavior.into(),
            params,
            response: Some(response),
        }
    }
}

/// Validated structural actor protocol schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolSchema {
    /// Human-readable/source name only. It is intentionally excluded from the
    /// structural identity so compatible renames do not invalidate references.
    pub name: String,
    members: BTreeMap<String, ProtocolMember>,
}

impl ProtocolSchema {
    pub fn new(
        name: impl Into<String>,
        members: impl IntoIterator<Item = ProtocolMember>,
    ) -> Result<Self, ProtocolSchemaError> {
        let name = name.into();
        let mut canonical = BTreeMap::new();
        for member in members {
            if member.behavior.is_empty() {
                return Err(ProtocolSchemaError::EmptyBehaviorName);
            }
            let behavior = member.behavior.clone();
            if canonical.insert(behavior.clone(), member).is_some() {
                return Err(ProtocolSchemaError::DuplicateBehavior(behavior));
            }
        }
        Ok(Self {
            name,
            members: canonical,
        })
    }

    pub fn members(&self) -> impl Iterator<Item = &ProtocolMember> {
        self.members.values()
    }

    pub fn id(&self) -> ProtocolId {
        ProtocolId::from_schema(self)
    }
}

/// BLAKE3 identity of a canonical actor protocol schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolId([u8; 32]);

impl ProtocolId {
    pub fn from_schema(schema: &ProtocolSchema) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(PROTOCOL_DOMAIN);
        put_u32(&mut hasher, schema.members.len() as u32);

        // BTreeMap iteration gives order-independent canonicalization of source
        // declaration order. Behavior names remain semantic and are hashed.
        for member in schema.members.values() {
            put_bytes(&mut hasher, member.behavior.as_bytes());
            put_u32(&mut hasher, member.params.len() as u32);
            for param in &member.params {
                hasher.update(param.as_bytes());
            }
            match member.response {
                Some(response) => {
                    hasher.update(&[1]);
                    hasher.update(response.as_bytes());
                }
                None => hasher.update(&[0]),
            }
        }

        Self(*hasher.finalize().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        encode_hex(&self.0)
    }
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&encode_hex(&self.0))
    }
}

impl FromStr for ProtocolId {
    type Err = ProtocolIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(ProtocolIdParseError::InvalidLength(value.len()));
        }
        let mut bytes = [0u8; 32];
        let raw = value.as_bytes();
        for (idx, out) in bytes.iter_mut().enumerate() {
            let hi = decode_hex_nibble(raw[idx * 2])?;
            let lo = decode_hex_nibble(raw[idx * 2 + 1])?;
            *out = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolSchemaError {
    EmptyBehaviorName,
    DuplicateBehavior(String),
}

impl fmt::Display for ProtocolSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolSchemaError::EmptyBehaviorName => f.write_str("protocol behavior name is empty"),
            ProtocolSchemaError::DuplicateBehavior(name) => {
                write!(f, "duplicate protocol behavior '{name}'")
            }
        }
    }
}

impl Error for ProtocolSchemaError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolIdParseError {
    InvalidLength(usize),
    InvalidHex(char),
}

impl fmt::Display for ProtocolIdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolIdParseError::InvalidLength(length) => {
                write!(f, "protocol id must contain 64 hex characters, got {length}")
            }
            ProtocolIdParseError::InvalidHex(ch) => {
                write!(f, "protocol id contains invalid hex character '{ch}'")
            }
        }
    }
}

impl Error for ProtocolIdParseError {}

fn put_u32(hasher: &mut Hasher, value: u32) {
    hasher.update(&value.to_le_bytes());
}

fn put_bytes(hasher: &mut Hasher, value: &[u8]) {
    put_u32(hasher, value.len() as u32);
    hasher.update(value);
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex_nibble(byte: u8) -> Result<u8, ProtocolIdParseError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ProtocolIdParseError::InvalidHex(byte as char)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int() -> ProtocolTypeId {
        ProtocolTypeId::from_canonical_bytes(b"core-type:int-v1")
    }

    fn money() -> ProtocolTypeId {
        ProtocolTypeId::from_canonical_bytes(b"core-type:money-v1")
    }

    fn receipt() -> ProtocolTypeId {
        ProtocolTypeId::from_canonical_bytes(b"core-type:receipt-v1")
    }

    #[test]
    fn declaration_order_does_not_change_protocol_id() {
        let first = ProtocolSchema::new(
            "Account",
            [
                ProtocolMember::message("Deposit", vec![money()]),
                ProtocolMember::request_reply("Balance", vec![], money()),
            ],
        )
        .unwrap();
        let second = ProtocolSchema::new(
            "Account",
            [
                ProtocolMember::request_reply("Balance", vec![], money()),
                ProtocolMember::message("Deposit", vec![money()]),
            ],
        )
        .unwrap();
        assert_eq!(first.id(), second.id());
    }

    #[test]
    fn display_name_rename_does_not_break_structural_identity() {
        let old = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Balance", vec![], money())],
        )
        .unwrap();
        let renamed = ProtocolSchema::new(
            "CustomerAccount",
            [ProtocolMember::request_reply("Balance", vec![], money())],
        )
        .unwrap();
        assert_eq!(old.id(), renamed.id());
    }

    #[test]
    fn signature_changes_change_protocol_id() {
        let one = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Withdraw", vec![money()], receipt())],
        )
        .unwrap();
        let changed_param = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Withdraw", vec![int()], receipt())],
        )
        .unwrap();
        let changed_response = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Withdraw", vec![money()], money())],
        )
        .unwrap();
        assert_ne!(one.id(), changed_param.id());
        assert_ne!(one.id(), changed_response.id());
    }

    #[test]
    fn fire_and_forget_differs_from_request_reply() {
        let message = ProtocolSchema::new(
            "Account",
            [ProtocolMember::message("Deposit", vec![money()])],
        )
        .unwrap();
        let request = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Deposit", vec![money()], receipt())],
        )
        .unwrap();
        assert_ne!(message.id(), request.id());
    }

    #[test]
    fn duplicate_behaviors_are_rejected() {
        let err = ProtocolSchema::new(
            "Broken",
            [
                ProtocolMember::message("Ping", vec![]),
                ProtocolMember::message("Ping", vec![int()]),
            ],
        )
        .unwrap_err();
        assert_eq!(err, ProtocolSchemaError::DuplicateBehavior("Ping".into()));
    }

    #[test]
    fn protocol_id_hex_round_trips() {
        let schema = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Balance", vec![], money())],
        )
        .unwrap();
        let id = schema.id();
        let encoded = id.to_string();
        assert_eq!(encoded.len(), 64);
        assert_eq!(encoded.parse::<ProtocolId>().unwrap(), id);
        assert!("abcd".parse::<ProtocolId>().is_err());
    }
}
