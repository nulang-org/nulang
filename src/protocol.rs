//! Stable identities for typed actor protocols.
//!
//! Protocol identity is derived from the compiler-owned actor behavior type,
//! not from formatting, source declaration order, runtime behavior ids, or
//! delivery mode. Each behavior hashes its full canonical function contract:
//! argument pack, return type, effect row, and capability.
//!
//! Human-readable actor/protocol names remain outside the structural hash, so
//! a pure source-level rename does not break compatibility when the behavior
//! contract is unchanged.

use crate::types::{
    canonical_type_bytes, Capability, EffectRow, Type, RECORD_ROW_TAIL_FIELD,
    UNSPECIFIED_ACTOR_PROTOCOL_TYPE_NAME,
};
use blake3::Hasher;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

const PROTOCOL_DOMAIN: &[u8] = b"nulang.protocol.v2\0";
const PROTOCOL_TYPE_DOMAIN: &[u8] = b"nulang.protocol.type.v1\0";

/// Stable identity of one canonical protocol type/signature.
///
/// This deliberately uses `canonical_type_bytes`, not NTIR. NTIR is allowed
/// to erase distinctions for compiler equality fast paths; protocol identity
/// has no such backstop and must preserve nominal wrappers, primitive
/// distinctions, effect rows, capabilities, and the rest of the canonical
/// type contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolTypeId([u8; 32]);

impl ProtocolTypeId {
    pub fn from_type(ty: &Type) -> Self {
        let bytes = canonical_type_bytes(ty);
        let mut hasher = Hasher::new();
        hasher.update(PROTOCOL_TYPE_DOMAIN);
        put_bytes(&mut hasher, &bytes);
        Self(*hasher.finalize().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One compiler-level behavior contract in an actor protocol.
///
/// `signature` is authoritative for structural identity and includes the full
/// normalized function type. `params` and `response` are retained as
/// behavior-level metadata for compatibility diagnostics and future protocol
/// diff tooling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolMember {
    pub behavior: String,
    pub params: Vec<ProtocolTypeId>,
    pub response: ProtocolTypeId,
    pub signature: ProtocolTypeId,
}

impl ProtocolMember {
    /// Build a member from the compiler's behavior function type.
    ///
    /// Actor behavior parameters use an explicit argument-pack convention:
    /// `()` is zero arguments, `(A, B)` is two, and a scalar source
    /// parameter is normalized to a one-element pack. Delivery mode
    /// (`send` vs `ask`) is intentionally not part of the behavior schema.
    pub fn from_signature(
        behavior: impl Into<String>,
        signature: &Type,
    ) -> Result<Self, ProtocolSchemaError> {
        let behavior = behavior.into();
        let Type::Function {
            param,
            ret,
            effect,
            cap,
        } = signature
        else {
            return Err(ProtocolSchemaError::InvalidBehaviorSignature(behavior));
        };

        let packed_param = match param.as_ref() {
            Type::Tuple(items) => Type::Tuple(items.clone()),
            other => Type::Tuple(vec![other.clone()]),
        };
        let normalized = Type::Function {
            param: Box::new(packed_param.clone()),
            ret: ret.clone(),
            effect: effect.clone(),
            cap: *cap,
        };

        validate_stable_protocol_type(&normalized, &behavior)?;

        let Type::Tuple(params) = packed_param else {
            unreachable!("actor protocol parameter normalization always yields a tuple");
        };

        Ok(Self {
            behavior,
            params: params.iter().map(ProtocolTypeId::from_type).collect(),
            response: ProtocolTypeId::from_type(ret),
            signature: ProtocolTypeId::from_type(&normalized),
        })
    }

    /// Convenience constructor for tests/tooling that already has concrete
    /// compiler types. This produces the same identity as `from_signature`.
    pub fn behavior(
        behavior: impl Into<String>,
        params: Vec<Type>,
        response: Type,
        effect: EffectRow,
        cap: Capability,
    ) -> Result<Self, ProtocolSchemaError> {
        Self::from_signature(
            behavior,
            &Type::Function {
                param: Box::new(Type::Tuple(params)),
                ret: Box::new(response),
                effect,
                cap,
            },
        )
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

/// One stable behavior-level reason a receiver cannot satisfy a required
/// protocol. Parameters and return types are surfaced separately for useful
/// diagnostics; if those match but the authoritative signature hash differs,
/// the remaining drift is effect/capability contract drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolCompatibilityIssue {
    MissingBehavior(String),
    ParameterContractChanged(String),
    ResponseContractChanged(String),
    EffectOrCapabilityContractChanged(String),
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

    /// Construct a canonical schema directly from a typechecker's actor type.
    pub fn from_actor_type(
        name: impl Into<String>,
        actor_type: &Type,
    ) -> Result<Self, ProtocolSchemaError> {
        let Type::Actor { behavior, .. } = actor_type else {
            return Err(ProtocolSchemaError::ExpectedActorType);
        };
        Self::from_behavior_type(name, behavior)
    }

    /// Construct a canonical schema from the compiler-owned behavior record in
    /// `Type::Actor.behavior`.
    pub fn from_behavior_type(
        name: impl Into<String>,
        behavior_type: &Type,
    ) -> Result<Self, ProtocolSchemaError> {
        let Type::Record(fields) = behavior_type else {
            return Err(ProtocolSchemaError::ExpectedBehaviorRecord);
        };

        let mut members = Vec::with_capacity(fields.len());
        for (behavior, signature) in fields {
            if behavior == RECORD_ROW_TAIL_FIELD {
                return Err(ProtocolSchemaError::UnresolvedBehaviorType(
                    behavior.clone(),
                ));
            }
            members.push(ProtocolMember::from_signature(behavior.clone(), signature)?);
        }
        Self::new(name, members)
    }

    pub fn members(&self) -> impl Iterator<Item = &ProtocolMember> {
        self.members.values()
    }

    pub fn id(&self) -> ProtocolId {
        ProtocolId::from_schema(self)
    }

    /// Classify whether this receiver/implementation can serve a client that
    /// was compiled against `required`.
    ///
    /// Compatibility is directional: additive receiver behaviors are safe for
    /// an older client, but removing or changing any required behavior is not.
    pub fn compatibility_for_required(&self, required: &ProtocolSchema) -> ProtocolCompatibility {
        if self.id() == required.id() {
            return ProtocolCompatibility::Exact;
        }
        if self.compatibility_issues_for_required(required).is_empty() {
            ProtocolCompatibility::ReceiverSuperset
        } else {
            ProtocolCompatibility::Incompatible
        }
    }

    /// Explain incompatibilities using the compiler-owned behavior contract.
    ///
    /// V1 is intentionally invariant: no field defaults, record widening,
    /// parameter variance, or implicit coercions are inferred here.
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
            if actual.params == expected.params
                && actual.response == expected.response
                && actual.signature != expected.signature
            {
                issues.push(
                    ProtocolCompatibilityIssue::EffectOrCapabilityContractChanged(name.clone()),
                );
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

        // BTreeMap iteration canonicalizes declaration order. Behavior names
        // remain semantic; the authoritative member signature hash includes
        // params, return, effects, and capability.
        for member in schema.members.values() {
            put_bytes(&mut hasher, member.behavior.as_bytes());
            hasher.update(member.signature.as_bytes());
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
    /// structurally by `ProtocolSchema::compatibility_for_required`, because a
    /// different digest alone cannot prove that the receiver is an additive
    /// compatible superset.
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
    ExpectedActorType,
    ExpectedBehaviorRecord,
    InvalidBehaviorSignature(String),
    UnresolvedBehaviorType(String),
    OpenBehaviorEffect(String),
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
            ProtocolSchemaError::ExpectedActorType => {
                f.write_str("protocol schema source must be an actor type")
            }
            ProtocolSchemaError::ExpectedBehaviorRecord => {
                f.write_str("actor protocol behavior metadata must be a record")
            }
            ProtocolSchemaError::InvalidBehaviorSignature(name) => {
                write!(
                    f,
                    "actor protocol behavior '{name}' is not a function signature"
                )
            }
            ProtocolSchemaError::UnresolvedBehaviorType(name) => {
                write!(
                    f,
                    "actor protocol behavior '{name}' contains an unresolved type"
                )
            }
            ProtocolSchemaError::OpenBehaviorEffect(name) => {
                write!(f, "actor protocol behavior '{name}' has an open effect row")
            }
        }
    }
}

impl Error for ProtocolSchemaError {}

fn validate_stable_protocol_type(ty: &Type, behavior: &str) -> Result<(), ProtocolSchemaError> {
    match ty {
        Type::Var(_) | Type::Skolem(_) | Type::Scheme { .. } => Err(
            ProtocolSchemaError::UnresolvedBehaviorType(behavior.to_string()),
        ),
        Type::Primitive(_) => Ok(()),
        Type::Tuple(items) => {
            for item in items {
                validate_stable_protocol_type(item, behavior)?;
            }
            Ok(())
        }
        Type::Record(fields) => {
            for (name, field) in fields {
                if name == RECORD_ROW_TAIL_FIELD {
                    return Err(ProtocolSchemaError::UnresolvedBehaviorType(
                        behavior.to_string(),
                    ));
                }
                validate_stable_protocol_type(field, behavior)?;
            }
            Ok(())
        }
        Type::Variant(variants) => {
            for (_, payload) in variants {
                if let Some(payload) = payload {
                    validate_stable_protocol_type(payload, behavior)?;
                }
            }
            Ok(())
        }
        Type::Array(inner) | Type::Reference { inner, .. } => {
            validate_stable_protocol_type(inner, behavior)
        }
        Type::Function {
            param, ret, effect, ..
        } => {
            if matches!(effect, EffectRow::Open(..)) {
                return Err(ProtocolSchemaError::OpenBehaviorEffect(
                    behavior.to_string(),
                ));
            }
            validate_stable_protocol_type(param, behavior)?;
            validate_stable_protocol_type(ret, behavior)
        }
        Type::Actor {
            state,
            behavior: actor_behavior,
        } => {
            validate_stable_protocol_type(state, behavior)?;
            validate_stable_protocol_type(actor_behavior, behavior)
        }
        Type::App { constructor, args } => {
            validate_stable_protocol_type(constructor, behavior)?;
            for arg in args {
                validate_stable_protocol_type(arg, behavior)?;
            }
            Ok(())
        }
        Type::Nominal { name, underlying } => {
            if name == UNSPECIFIED_ACTOR_PROTOCOL_TYPE_NAME {
                return Err(ProtocolSchemaError::UnresolvedBehaviorType(
                    behavior.to_string(),
                ));
            }
            validate_stable_protocol_type(underlying, behavior)
        }
    }
}

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
    use crate::types::{Effect, PrimitiveType, Region, TypeVar};

    fn int_ty() -> Type {
        Type::Primitive(PrimitiveType::Int)
    }

    fn money_ty() -> Type {
        Type::Record(vec![("cents".to_string(), int_ty())])
    }

    fn receipt_ty() -> Type {
        Type::Record(vec![(
            "id".to_string(),
            Type::Primitive(PrimitiveType::String),
        )])
    }

    fn member(behavior: &str, params: Vec<Type>, response: Type) -> ProtocolMember {
        ProtocolMember::behavior(
            behavior,
            params,
            response,
            EffectRow::empty(),
            Capability::Ref,
        )
        .unwrap()
    }

    fn actor_type(members: Vec<(&str, Type)>) -> Type {
        Type::Actor {
            state: Box::new(Type::unit()),
            behavior: Box::new(Type::Record(
                members
                    .into_iter()
                    .map(|(name, signature)| (name.to_string(), signature))
                    .collect(),
            )),
        }
    }

    fn behavior_sig(params: Vec<Type>, ret: Type, effect: EffectRow, cap: Capability) -> Type {
        Type::Function {
            param: Box::new(Type::Tuple(params)),
            ret: Box::new(ret),
            effect,
            cap,
        }
    }

    #[test]
    fn protocol_type_id_uses_canonical_type_identity_not_ntir() {
        let never = Type::Primitive(PrimitiveType::Never);
        let unit = Type::unit();
        assert_eq!(never.to_ntir().hash(), unit.to_ntir().hash());
        assert_ne!(
            ProtocolTypeId::from_type(&never),
            ProtocolTypeId::from_type(&unit)
        );

        let a = Type::Nominal {
            name: "CustomerId".into(),
            underlying: Box::new(Type::int()),
        };
        let b = Type::Nominal {
            name: "OrderId".into(),
            underlying: Box::new(Type::int()),
        };
        assert_ne!(ProtocolTypeId::from_type(&a), ProtocolTypeId::from_type(&b));
    }

    #[test]
    fn declaration_order_does_not_change_protocol_id() {
        let first = ProtocolSchema::new(
            "Account",
            [
                member("Deposit", vec![money_ty()], Type::unit()),
                member("Balance", vec![], money_ty()),
            ],
        )
        .unwrap();
        let second = ProtocolSchema::new(
            "Account",
            [
                member("Balance", vec![], money_ty()),
                member("Deposit", vec![money_ty()], Type::unit()),
            ],
        )
        .unwrap();
        assert_eq!(first.id(), second.id());
    }

    #[test]
    fn display_name_rename_does_not_break_structural_identity() {
        let member = member("Balance", vec![], money_ty());
        let old = ProtocolSchema::new("Account", [member.clone()]).unwrap();
        let renamed = ProtocolSchema::new("CustomerAccount", [member]).unwrap();
        assert_eq!(old.id(), renamed.id());
    }

    #[test]
    fn full_behavior_contract_changes_protocol_id() {
        let base = ProtocolSchema::new(
            "Account",
            [ProtocolMember::behavior(
                "Withdraw",
                vec![money_ty()],
                receipt_ty(),
                EffectRow::empty(),
                Capability::Ref,
            )
            .unwrap()],
        )
        .unwrap();

        let changed_param = ProtocolSchema::new(
            "Account",
            [member("Withdraw", vec![int_ty()], receipt_ty())],
        )
        .unwrap();
        let changed_response = ProtocolSchema::new(
            "Account",
            [member("Withdraw", vec![money_ty()], money_ty())],
        )
        .unwrap();
        let changed_effect = ProtocolSchema::new(
            "Account",
            [ProtocolMember::behavior(
                "Withdraw",
                vec![money_ty()],
                receipt_ty(),
                EffectRow::Closed(vec![Effect::IO]),
                Capability::Ref,
            )
            .unwrap()],
        )
        .unwrap();
        let changed_capability = ProtocolSchema::new(
            "Account",
            [ProtocolMember::behavior(
                "Withdraw",
                vec![money_ty()],
                receipt_ty(),
                EffectRow::empty(),
                Capability::Box,
            )
            .unwrap()],
        )
        .unwrap();

        assert_ne!(base.id(), changed_param.id());
        assert_ne!(base.id(), changed_response.id());
        assert_ne!(base.id(), changed_effect.id());
        assert_ne!(base.id(), changed_capability.id());
    }

    #[test]
    fn compiler_actor_type_generates_canonical_protocol_schema() {
        let get = behavior_sig(vec![], Type::int(), EffectRow::empty(), Capability::Ref);
        let add = behavior_sig(
            vec![Type::int()],
            Type::unit(),
            EffectRow::Closed(vec![Effect::Send]),
            Capability::Ref,
        );

        let first = ProtocolSchema::from_actor_type(
            "Counter",
            &actor_type(vec![("get", get.clone()), ("add", add.clone())]),
        )
        .unwrap();
        let reordered = ProtocolSchema::from_actor_type(
            "RenamedCounter",
            &actor_type(vec![("add", add), ("get", get)]),
        )
        .unwrap();

        assert_eq!(first.id(), reordered.id());
        assert_eq!(first.members().count(), 2);
    }

    #[test]
    fn tuple_payload_and_two_argument_pack_have_distinct_protocol_ids() {
        let pair = Type::Tuple(vec![Type::int(), Type::string()]);
        let one_tuple_arg =
            ProtocolSchema::new("Sink", [member("push", vec![pair], Type::unit())]).unwrap();
        let two_args = ProtocolSchema::new(
            "Sink",
            [member(
                "push",
                vec![Type::int(), Type::string()],
                Type::unit(),
            )],
        )
        .unwrap();

        assert_ne!(one_tuple_arg.id(), two_args.id());
    }

    #[test]
    fn schema_generation_fails_closed_for_unresolved_contracts() {
        let unspecified = Type::Nominal {
            name: UNSPECIFIED_ACTOR_PROTOCOL_TYPE_NAME.to_string(),
            underlying: Box::new(Type::unit()),
        };
        let unresolved = actor_type(vec![(
            "add",
            behavior_sig(
                vec![unspecified],
                Type::unit(),
                EffectRow::empty(),
                Capability::Ref,
            ),
        )]);
        assert!(matches!(
            ProtocolSchema::from_actor_type("Counter", &unresolved),
            Err(ProtocolSchemaError::UnresolvedBehaviorType(name)) if name == "add"
        ));

        let open_effect = actor_type(vec![(
            "get",
            behavior_sig(
                vec![],
                Type::int(),
                EffectRow::Open(vec![Effect::IO], Region::fresh()),
                Capability::Ref,
            ),
        )]);
        assert!(matches!(
            ProtocolSchema::from_actor_type("Counter", &open_effect),
            Err(ProtocolSchemaError::OpenBehaviorEffect(name)) if name == "get"
        ));

        let unresolved_var = actor_type(vec![(
            "get",
            behavior_sig(
                vec![],
                Type::Var(TypeVar::fresh()),
                EffectRow::empty(),
                Capability::Ref,
            ),
        )]);
        assert!(matches!(
            ProtocolSchema::from_actor_type("Counter", &unresolved_var),
            Err(ProtocolSchemaError::UnresolvedBehaviorType(name)) if name == "get"
        ));
    }

    #[test]
    fn duplicate_behaviors_are_rejected() {
        let ping = member("Ping", vec![], Type::unit());
        let err = ProtocolSchema::new("Broken", [ping.clone(), ping]).unwrap_err();
        assert_eq!(err, ProtocolSchemaError::DuplicateBehavior("Ping".into()));
    }

    #[test]
    fn protocol_actor_ref_requires_exact_protocol() {
        let account = ProtocolSchema::new("Account", [member("Balance", vec![], money_ty())])
            .unwrap()
            .id();
        let inventory = ProtocolSchema::new("Inventory", [member("Count", vec![], int_ty())])
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
    fn compatibility_is_exact_for_identical_compiler_protocols() {
        let required =
            ProtocolSchema::new("Account", [member("Balance", vec![], money_ty())]).unwrap();
        let renamed =
            ProtocolSchema::new("RenamedAccount", [member("Balance", vec![], money_ty())]).unwrap();

        assert_eq!(
            renamed.compatibility_for_required(&required),
            ProtocolCompatibility::Exact
        );
        assert!(renamed.can_serve(&required));
    }

    #[test]
    fn additive_receiver_upgrade_is_directionally_compatible() {
        let old = ProtocolSchema::new("Account", [member("Balance", vec![], money_ty())]).unwrap();
        let new = ProtocolSchema::new(
            "Account",
            [
                member("Balance", vec![], money_ty()),
                member("Deposit", vec![money_ty()], Type::unit()),
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
            vec![ProtocolCompatibilityIssue::MissingBehavior(
                "Deposit".into()
            )]
        );
    }

    #[test]
    fn parameter_and_response_contract_changes_are_incompatible() {
        let required = ProtocolSchema::new(
            "Account",
            [member("Withdraw", vec![money_ty()], receipt_ty())],
        )
        .unwrap();
        let changed_param = ProtocolSchema::new(
            "Account",
            [member("Withdraw", vec![int_ty()], receipt_ty())],
        )
        .unwrap();
        let changed_response = ProtocolSchema::new(
            "Account",
            [member("Withdraw", vec![money_ty()], money_ty())],
        )
        .unwrap();

        assert_eq!(
            changed_param.compatibility_issues_for_required(&required),
            vec![ProtocolCompatibilityIssue::ParameterContractChanged(
                "Withdraw".into()
            )]
        );
        assert_eq!(
            changed_response.compatibility_issues_for_required(&required),
            vec![ProtocolCompatibilityIssue::ResponseContractChanged(
                "Withdraw".into()
            )]
        );
    }

    #[test]
    fn effect_or_capability_drift_is_incompatible() {
        let required = ProtocolSchema::new(
            "Account",
            [ProtocolMember::behavior(
                "Withdraw",
                vec![money_ty()],
                receipt_ty(),
                EffectRow::empty(),
                Capability::Ref,
            )
            .unwrap()],
        )
        .unwrap();
        let changed_effect = ProtocolSchema::new(
            "Account",
            [ProtocolMember::behavior(
                "Withdraw",
                vec![money_ty()],
                receipt_ty(),
                EffectRow::Closed(vec![Effect::IO]),
                Capability::Ref,
            )
            .unwrap()],
        )
        .unwrap();
        let changed_cap = ProtocolSchema::new(
            "Account",
            [ProtocolMember::behavior(
                "Withdraw",
                vec![money_ty()],
                receipt_ty(),
                EffectRow::empty(),
                Capability::Box,
            )
            .unwrap()],
        )
        .unwrap();

        for changed in [&changed_effect, &changed_cap] {
            assert_eq!(
                changed.compatibility_for_required(&required),
                ProtocolCompatibility::Incompatible
            );
            assert_eq!(
                changed.compatibility_issues_for_required(&required),
                vec![
                    ProtocolCompatibilityIssue::EffectOrCapabilityContractChanged(
                        "Withdraw".into()
                    )
                ]
            );
        }
    }

    #[test]
    fn compiler_generated_schemas_use_same_rolling_compatibility_rule() {
        let get = behavior_sig(vec![], Type::int(), EffectRow::empty(), Capability::Ref);
        let add = behavior_sig(
            vec![Type::int()],
            Type::unit(),
            EffectRow::Closed(vec![Effect::Send]),
            Capability::Ref,
        );
        let old =
            ProtocolSchema::from_actor_type("Counter", &actor_type(vec![("get", get.clone())]))
                .unwrap();
        let new = ProtocolSchema::from_actor_type(
            "Counter",
            &actor_type(vec![("add", add), ("get", get)]),
        )
        .unwrap();

        assert_eq!(
            new.compatibility_for_required(&old),
            ProtocolCompatibility::ReceiverSuperset
        );
        assert!(new.can_serve(&old));
        assert!(!old.can_serve(&new));
    }

    #[test]
    fn protocol_id_hex_round_trips() {
        let schema =
            ProtocolSchema::new("Account", [member("Balance", vec![], money_ty())]).unwrap();
        let id = schema.id();
        let encoded = id.to_string();
        assert_eq!(encoded.len(), 64);
        assert_eq!(encoded.parse::<ProtocolId>().unwrap(), id);
        assert!("abcd".parse::<ProtocolId>().is_err());
    }
}
