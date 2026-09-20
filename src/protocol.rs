//! Stable identities for typed actor protocols.
//!
//! Distributed actor references need a schema identity that is independent of
//! source declaration order, formatting, and compiler-internal `Debug` output.
//! This module provides that boundary without changing the existing actor-ref
//! representation yet.
//!
//! `ProtocolTypeId` reuses Nulang's canonical NTIR type hash. `ProtocolId` is
//! then derived from the set of behavior signatures. Human-readable protocol
//! names are deliberately not hashed, so a source-level rename does not break
//! wire compatibility when the protocol shape is unchanged.

use crate::type_ir::NtirNode;
use crate::types::Type;
use blake3::Hasher;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

const PROTOCOL_DOMAIN: &[u8] = b"nulang.protocol.v1\0";

/// Stable identity of one canonical parameter/response type.
///
/// This is exactly the existing NTIR structural hash wrapped in a protocol
/// vocabulary type. Reusing NTIR prevents actor protocols from inventing a
/// second, subtly different notion of semantic type identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolTypeId([u8; 32]);

impl ProtocolTypeId {
    pub fn from_type(ty: &Type) -> Self {
        Self::from_ntir(&ty.to_ntir())
    }

    pub fn from_ntir(ntir: &NtirNode) -> Self {
        Self(ntir.hash())
    }

    pub fn from_ntir_hash(hash: [u8; 32]) -> Self {
        Self(hash)
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
/// Directional compatibility of a receiver/implementation protocol against a
/// protocol required by an existing client/reference.
///
/// The direction matters for rolling upgrades:
/// - Exact: both structural protocols are identical.
/// - ReceiverSuperset: the receiver preserves every required behavior with
///   the exact same signature and only adds behaviors. Old clients remain safe.
/// - Incompatible: a required behavior is missing or any existing behavior
///   signature changed.
///
/// V1 deliberately uses invariant behavior signatures. Richer evolution
/// (field defaults, variant widening, parameter variance) must be introduced
/// explicitly rather than inferred from hashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolCompatibility {
    Exact,
    ReceiverSuperset,
    Incompatible,
}

impl ProtocolCompatibility {
    pub fn is_compatible(self) -> bool {
        !matches!(self, ProtocolCompatibility::Incompatible)
    }
}
/// Trusted in-process catalog of canonical actor protocol schemas.
///
/// A protocol digest is sufficient for exact equality, but not for proving
/// rolling-upgrade compatibility between two different digests. The registry
/// retains the canonical schemas needed to evaluate that directional relation
/// without sending source declarations over the wire.
///
/// Missing schemas fail closed: callers must not reinterpret an unknown digest
/// as compatible merely because the behavior name being invoked happens to
/// exist locally.
#[derive(Debug, Clone, Default)]
pub struct ProtocolRegistry {
    schemas: BTreeMap<ProtocolId, ProtocolSchema>,
}

impl ProtocolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        schema: ProtocolSchema,
    ) -> Result<ProtocolId, ProtocolRegistryError> {
        let id = schema.id();
        if let Some(existing) = self.schemas.get(&id) {
            if existing != &schema {
                return Err(ProtocolRegistryError::HashCollision(id));
            }
            return Ok(id);
        }
        self.schemas.insert(id, schema);
        Ok(id)
    }

    pub fn get(&self, id: ProtocolId) -> Option<&ProtocolSchema> {
        self.schemas.get(&id)
    }

    pub fn contains(&self, id: ProtocolId) -> bool {
        self.schemas.contains_key(&id)
    }

    /// Evaluate whether the receiver implementation can serve a client that
    /// requires the supplied protocol.
    ///
    /// Both schemas must be present. Unknown ids are explicit errors so a
    /// runtime can reject before mailbox publication rather than silently
    /// degrading to name-only dispatch.
    pub fn compatibility(
        &self,
        receiver: ProtocolId,
        required: ProtocolId,
    ) -> Result<ProtocolCompatibility, ProtocolRegistryError> {
        if receiver == required {
            // Exact identity does not need a schema lookup; the content hash
            // itself proves equality.
            return Ok(ProtocolCompatibility::Exact);
        }

        let receiver_schema = self
            .schemas
            .get(&receiver)
            .ok_or(ProtocolRegistryError::UnknownProtocol(receiver))?;
        let required_schema = self
            .schemas
            .get(&required)
            .ok_or(ProtocolRegistryError::UnknownProtocol(required))?;

        Ok(receiver_schema.compatibility_for_required(required_schema))
    }

    pub fn can_serve(
        &self,
        receiver: ProtocolId,
        required: ProtocolId,
    ) -> Result<bool, ProtocolRegistryError> {
        Ok(self.compatibility(receiver, required)?.is_compatible())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolRegistryError {
    UnknownProtocol(ProtocolId),
    HashCollision(ProtocolId),
}

impl fmt::Display for ProtocolRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolRegistryError::UnknownProtocol(id) => {
                write!(f, "unknown actor protocol schema {id}")
            }
            ProtocolRegistryError::HashCollision(id) => {
                write!(f, "actor protocol hash collision for {id}")
            }
        }
    }
}

impl Error for ProtocolRegistryError {}
/// Runtime admission policy for incoming actor messages carrying protocol identity.
///
/// Strict policies reject untyped/legacy messages before mailbox publication.
/// LegacyCompatible exists only as an explicit migration mode for mixed
/// deployments; it must never be the implicit default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolAdmissionPolicy {
    StrictExact,
    StrictCompatible,
    LegacyCompatible,
}

impl Default for ProtocolAdmissionPolicy {
    fn default() -> Self {
        ProtocolAdmissionPolicy::StrictCompatible
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolAdmission {
    Exact,
    CompatibleUpgrade,
    LegacyUntyped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolAdmissionError {
    MissingRequiredProtocol,
    MissingReceiverProtocol,
    ExactMismatch {
        receiver: ProtocolId,
        required: ProtocolId,
    },
    Incompatible {
        receiver: ProtocolId,
        required: ProtocolId,
    },
    UnknownProtocol(ProtocolId),
    RegistryHashCollision(ProtocolId),
}

impl fmt::Display for ProtocolAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolAdmissionError::MissingRequiredProtocol => {
                f.write_str("incoming actor message has no required protocol identity")
            }
            ProtocolAdmissionError::MissingReceiverProtocol => {
                f.write_str("target actor has no protocol identity")
            }
            ProtocolAdmissionError::ExactMismatch { receiver, required } => write!(
                f,
                "actor protocol mismatch: receiver {receiver}, required {required}"
            ),
            ProtocolAdmissionError::Incompatible { receiver, required } => write!(
                f,
                "actor protocol is incompatible: receiver {receiver}, required {required}"
            ),
            ProtocolAdmissionError::UnknownProtocol(id) => {
                write!(f, "unknown actor protocol schema {id}")
            }
            ProtocolAdmissionError::RegistryHashCollision(id) => {
                write!(f, "actor protocol registry hash collision for {id}")
            }
        }
    }
}

impl Error for ProtocolAdmissionError {}

/// Decide whether a message may be published to a target actor mailbox.
///
/// `receiver` is the target actor's currently installed protocol identity.
/// `required` is the protocol identity carried by the incoming client/message.
/// The function is deliberately side-effect free so runtimes can call it
/// before mutating mailbox state.
pub fn admit_protocol(
    registry: &ProtocolRegistry,
    policy: ProtocolAdmissionPolicy,
    receiver: Option<ProtocolId>,
    required: Option<ProtocolId>,
) -> Result<ProtocolAdmission, ProtocolAdmissionError> {
    match (receiver, required) {
        (Some(receiver), Some(required)) if receiver == required => {
            Ok(ProtocolAdmission::Exact)
        }
        (Some(receiver), Some(required)) => match policy {
            ProtocolAdmissionPolicy::StrictExact => Err(
                ProtocolAdmissionError::ExactMismatch { receiver, required },
            ),
            ProtocolAdmissionPolicy::StrictCompatible
            | ProtocolAdmissionPolicy::LegacyCompatible => {
                let compatibility = registry.compatibility(receiver, required).map_err(
                    |error| match error {
                        ProtocolRegistryError::UnknownProtocol(id) => {
                            ProtocolAdmissionError::UnknownProtocol(id)
                        }
                        ProtocolRegistryError::HashCollision(id) => {
                            ProtocolAdmissionError::RegistryHashCollision(id)
                        }
                    },
                )?;
                match compatibility {
                    ProtocolCompatibility::Exact => Ok(ProtocolAdmission::Exact),
                    ProtocolCompatibility::ReceiverSuperset => {
                        Ok(ProtocolAdmission::CompatibleUpgrade)
                    }
                    ProtocolCompatibility::Incompatible => Err(
                        ProtocolAdmissionError::Incompatible { receiver, required },
                    ),
                }
            }
        },
        (None, Some(_)) => Err(ProtocolAdmissionError::MissingReceiverProtocol),
        (_, None) => match policy {
            ProtocolAdmissionPolicy::LegacyCompatible => Ok(ProtocolAdmission::LegacyUntyped),
            ProtocolAdmissionPolicy::StrictExact
            | ProtocolAdmissionPolicy::StrictCompatible => {
                Err(ProtocolAdmissionError::MissingRequiredProtocol)
            }
        },
    }
}

/// One concrete reason a receiver cannot satisfy a required actor protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolCompatibilityIssue {
    MissingBehavior(String),
    ParameterContractChanged(String),
    ResponseContractChanged(String),
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

    /// Classify whether this schema, acting as the receiver/implementation,
    /// can safely serve a client compiled against `required`.
    ///
    /// This is directional. If a new receiver adds behavior B, then
    /// new.compatibility_for_required(old) is ReceiverSuperset, while
    /// old.compatibility_for_required(new) is Incompatible.
    pub fn compatibility_for_required(
        &self,
        required: &ProtocolSchema,
    ) -> ProtocolCompatibility {
        if self.id() == required.id() {
            return ProtocolCompatibility::Exact;
        }

        if self.compatibility_issues_for_required(required).is_empty() {
            ProtocolCompatibility::ReceiverSuperset
        } else {
            ProtocolCompatibility::Incompatible
        }
    }

    /// Explain incompatibilities using stable behavior-level reasons.
    pub fn compatibility_issues_for_required(
        &self,
        required: &ProtocolSchema,
    ) -> Vec<ProtocolCompatibilityIssue> {
        let mut issues = Vec::new();

        for (name, expected) in &required.members {
            let Some(actual) = self.members.get(name) else {
                issues.push(ProtocolCompatibilityIssue::MissingBehavior(name.clone()));
                continue;
            };
            if actual.params != expected.params {
                issues.push(ProtocolCompatibilityIssue::ParameterContractChanged(
                    name.clone(),
                ));
            }
            if actual.response != expected.response {
                issues.push(ProtocolCompatibilityIssue::ResponseContractChanged(
                    name.clone(),
                ));
            }
        }

        issues
    }

    pub fn can_serve(&self, required: &ProtocolSchema) -> bool {
        self.compatibility_for_required(required).is_compatible()
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
                None => {
                    hasher.update(&[0]);
                }
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

/// Runtime/wire-ready actor address carrying its exact protocol identity.
///
/// This is intentionally independent of `runtime::NodeId` so compiler,
/// package, and serialization code can use protocol metadata without depending
/// on the actor runtime. The runtime can losslessly convert its `NodeId(u64)`
/// and actor id into this envelope when distributed dispatch becomes typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtocolActorRef {
    pub node_id: u64,
    pub actor_id: u64,
    pub protocol_id: ProtocolId,
}

impl ProtocolActorRef {
    pub fn new(node_id: u64, actor_id: u64, protocol_id: ProtocolId) -> Self {
        Self {
            node_id,
            actor_id,
            protocol_id,
        }
    }

    pub fn matches_protocol(&self, expected: ProtocolId) -> bool {
        self.protocol_id == expected
    }

    /// Exact protocol-id validation. Rolling-upgrade compatibility is defined
    /// structurally by ProtocolSchema::compatibility_for_required, because a
    /// digest alone cannot prove that a different schema is an additive
    /// superset without access to both schemas.
    pub fn require_protocol(&self, expected: ProtocolId) -> Result<(), ProtocolMismatch> {
        if self.matches_protocol(expected) {
            Ok(())
        } else {
            Err(ProtocolMismatch {
                expected,
                actual: self.protocol_id,
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolMismatch {
    pub expected: ProtocolId,
    pub actual: ProtocolId,
}

impl fmt::Display for ProtocolMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "actor protocol mismatch: expected {}, got {}",
            self.expected, self.actual
        )
    }
}

impl Error for ProtocolMismatch {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolSchemaError {
    EmptyBehaviorName,
    DuplicateBehavior(String),
}

impl fmt::Display for ProtocolSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolSchemaError::EmptyBehaviorName => {
                f.write_str("protocol behavior name is empty")
            }
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
                write!(
                    f,
                    "protocol id must contain 64 hex characters, got {length}"
                )
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
    use crate::types::PrimitiveType;

    fn int() -> ProtocolTypeId {
        ProtocolTypeId::from_type(&Type::Primitive(PrimitiveType::Int))
    }

    fn money() -> ProtocolTypeId {
        ProtocolTypeId::from_type(&Type::Record(vec![(
            "cents".to_string(),
            Type::Primitive(PrimitiveType::Int),
        )]))
    }

    fn receipt() -> ProtocolTypeId {
        ProtocolTypeId::from_type(&Type::Record(vec![(
            "id".to_string(),
            Type::Primitive(PrimitiveType::String),
        )]))
    }

    #[test]
    fn protocol_type_id_reuses_ntir_hash() {
        let ty = Type::Primitive(PrimitiveType::Int);
        assert_eq!(ProtocolTypeId::from_type(&ty).0, ty.to_ntir().hash());
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
            [ProtocolMember::request_reply(
                "Withdraw",
                vec![money()],
                receipt(),
            )],
        )
        .unwrap();
        let changed_param = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply(
                "Withdraw",
                vec![int()],
                receipt(),
            )],
        )
        .unwrap();
        let changed_response = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply(
                "Withdraw",
                vec![money()],
                money(),
            )],
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
            [ProtocolMember::request_reply(
                "Deposit",
                vec![money()],
                receipt(),
            )],
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
    fn protocol_actor_ref_requires_exact_protocol() {
        let account = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Balance", vec![], money())],
        )
        .unwrap()
        .id();
        let inventory = ProtocolSchema::new(
            "Inventory",
            [ProtocolMember::request_reply("Count", vec![], int())],
        )
        .unwrap()
        .id();

        let actor = ProtocolActorRef::new(7, 42, account);
        assert_eq!(actor.node_id, 7);
        assert_eq!(actor.actor_id, 42);
        assert!(actor.require_protocol(account).is_ok());
        assert_eq!(
            actor.require_protocol(inventory),
            Err(ProtocolMismatch {
                expected: inventory,
                actual: account,
            })
        );
    }

    #[test]
    fn compatibility_is_exact_for_identical_protocols() {
        let required = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Balance", vec![], money())],
        )
        .unwrap();
        let renamed = ProtocolSchema::new(
            "RenamedAccount",
            [ProtocolMember::request_reply("Balance", vec![], money())],
        )
        .unwrap();

        assert_eq!(
            renamed.compatibility_for_required(&required),
            ProtocolCompatibility::Exact
        );
        assert!(renamed.can_serve(&required));
    }

    #[test]
    fn additive_receiver_upgrade_is_directionally_compatible() {
        let old = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Balance", vec![], money())],
        )
        .unwrap();
        let new = ProtocolSchema::new(
            "Account",
            [
                ProtocolMember::request_reply("Balance", vec![], money()),
                ProtocolMember::message("Deposit", vec![money()]),
            ],
        )
        .unwrap();

        assert_eq!(
            new.compatibility_for_required(&old),
            ProtocolCompatibility::ReceiverSuperset
        );
        assert!(new.can_serve(&old));
        assert_eq!(
            old.compatibility_for_required(&new),
            ProtocolCompatibility::Incompatible
        );
        assert_eq!(
            old.compatibility_issues_for_required(&new),
            vec![ProtocolCompatibilityIssue::MissingBehavior("Deposit".into())]
        );
    }

    #[test]
    fn changing_existing_parameter_contract_is_incompatible() {
        let required = ProtocolSchema::new(
            "Account",
            [ProtocolMember::message("Deposit", vec![money()])],
        )
        .unwrap();
        let changed = ProtocolSchema::new(
            "Account",
            [ProtocolMember::message("Deposit", vec![int()])],
        )
        .unwrap();

        assert_eq!(
            changed.compatibility_for_required(&required),
            ProtocolCompatibility::Incompatible
        );
        assert_eq!(
            changed.compatibility_issues_for_required(&required),
            vec![ProtocolCompatibilityIssue::ParameterContractChanged(
                "Deposit".into()
            )]
        );
    }

    #[test]
    fn changing_response_or_message_mode_is_incompatible() {
        let required = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Withdraw", vec![money()], receipt())],
        )
        .unwrap();
        let changed_response = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Withdraw", vec![money()], money())],
        )
        .unwrap();
        let fire_and_forget = ProtocolSchema::new(
            "Account",
            [ProtocolMember::message("Withdraw", vec![money()])],
        )
        .unwrap();

        for schema in [&changed_response, &fire_and_forget] {
            assert_eq!(
                schema.compatibility_for_required(&required),
                ProtocolCompatibility::Incompatible
            );
            assert_eq!(
                schema.compatibility_issues_for_required(&required),
                vec![ProtocolCompatibilityIssue::ResponseContractChanged(
                    "Withdraw".into()
                )]
            );
        }
    }

    #[test]
    fn protocol_registry_exact_match_needs_no_schema_lookup() {
        let schema = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Balance", vec![], money())],
        )
        .unwrap();
        let id = schema.id();
        let registry = ProtocolRegistry::new();

        assert_eq!(
            registry.compatibility(id, id).unwrap(),
            ProtocolCompatibility::Exact
        );
    }

    #[test]
    fn protocol_registry_proves_additive_receiver_compatibility() {
        let old = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Balance", vec![], money())],
        )
        .unwrap();
        let new = ProtocolSchema::new(
            "Account",
            [
                ProtocolMember::request_reply("Balance", vec![], money()),
                ProtocolMember::message("Deposit", vec![money()]),
            ],
        )
        .unwrap();

        let old_id = old.id();
        let new_id = new.id();
        let mut registry = ProtocolRegistry::new();
        registry.register(old).unwrap();
        registry.register(new).unwrap();

        assert_eq!(
            registry.compatibility(new_id, old_id).unwrap(),
            ProtocolCompatibility::ReceiverSuperset
        );
        assert!(registry.can_serve(new_id, old_id).unwrap());
        assert_eq!(
            registry.compatibility(old_id, new_id).unwrap(),
            ProtocolCompatibility::Incompatible
        );
    }

    #[test]
    fn protocol_registry_fails_closed_for_unknown_different_digest() {
        let known = ProtocolSchema::new(
            "Account",
            [ProtocolMember::request_reply("Balance", vec![], money())],
        )
        .unwrap();
        let unknown = ProtocolSchema::new(
            "Account",
            [
                ProtocolMember::request_reply("Balance", vec![], money()),
                ProtocolMember::message("Deposit", vec![money()]),
            ],
        )
        .unwrap();

        let known_id = known.id();
        let unknown_id = unknown.id();
        let mut registry = ProtocolRegistry::new();
        registry.register(known).unwrap();

        assert_eq!(
            registry.compatibility(known_id, unknown_id),
            Err(ProtocolRegistryError::UnknownProtocol(unknown_id))
        );
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
