//! Renderer-neutral protocol types for Nulang universal applications.
//!
//! `nulang-ui/1` defines semantic UI documents and patches.
//! `nulang-ui-msg/1` defines runtime/host message envelopes.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const UI_PROTOCOL_VERSION: &str = "nulang-ui/1";
pub const UI_MESSAGE_PROTOCOL_VERSION: &str = "nulang-ui-msg/1";

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(DocumentId);
string_id!(NodeId);
string_id!(ActionId);
string_id!(CorrelationId);
string_id!(IdempotencyKey);

/// Monotonic revision encoded as decimal text so every JSON host preserves
/// the full `u64` range rather than truncating at JavaScript's 53-bit limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(pub u64);

impl Serialize for Revision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Revision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse::<u64>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// `i64` encoded as decimal text so hosts that parse JSON numbers as `f64`
/// cannot silently lose precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LosslessI64(pub i64);

impl Serialize for LosslessI64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for LosslessI64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse::<i64>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl From<i64> for LosslessI64 {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

/// Exact IEEE-754 payload encoded as sixteen hexadecimal digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct F64Bits(pub u64);

impl F64Bits {
    pub fn from_f64(value: f64) -> Self {
        Self(value.to_bits())
    }

    pub fn to_f64(self) -> f64 {
        f64::from_bits(self.0)
    }
}

impl Serialize for F64Bits {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{:016x}", self.0))
    }
}

impl<'de> Deserialize<'de> for F64Bits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.len() != 16 {
            return Err(serde::de::Error::custom(
                "f64 bit payload must contain exactly 16 hexadecimal digits",
            ));
        }
        u64::from_str_radix(&value, 16)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Lossless values crossing the UI/action ABI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum WireValue {
    Null,
    Bool(bool),
    I64(LosslessI64),
    F64(F64Bits),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<WireValue>),
    Object(BTreeMap<String, WireValue>),
}

impl From<i64> for WireValue {
    fn from(value: i64) -> Self {
        Self::I64(value.into())
    }
}

impl From<bool> for WireValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<String> for WireValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for WireValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionPlacement {
    Client,
    Server,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionBinding {
    pub event: String,
    pub action_id: ActionId,
    pub placement: ActionPlacement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiNode {
    pub id: NodeId,
    pub kind: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, WireValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<NodeId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionBinding>,
}

impl UiNode {
    pub fn new(id: impl Into<NodeId>, kind: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
            properties: BTreeMap::new(),
            children: Vec::new(),
            actions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiDocument {
    pub protocol: String,
    pub document_id: DocumentId,
    pub revision: Revision,
    pub root: NodeId,
    pub nodes: Vec<UiNode>,
}

impl UiDocument {
    pub fn new(
        document_id: impl Into<DocumentId>,
        revision: Revision,
        root: impl Into<NodeId>,
        mut nodes: Vec<UiNode>,
    ) -> Self {
        nodes.sort_by(|left, right| left.id.cmp(&right.id));
        Self {
            protocol: UI_PROTOCOL_VERSION.to_owned(),
            document_id: document_id.into(),
            revision,
            root: root.into(),
            nodes,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        require_protocol(UI_PROTOCOL_VERSION, &self.protocol)?;
        require_id("document_id", self.document_id.as_str())?;
        require_id("root", self.root.as_str())?;

        let mut nodes = BTreeMap::<NodeId, &UiNode>::new();
        for node in &self.nodes {
            require_id("node_id", node.id.as_str())?;
            if node.kind.trim().is_empty() {
                return Err(ProtocolError::EmptyNodeKind(node.id.clone()));
            }
            for binding in &node.actions {
                require_id("action_id", binding.action_id.as_str())?;
                if binding.event.trim().is_empty() {
                    return Err(ProtocolError::EmptyActionEvent(node.id.clone()));
                }
            }
            if nodes.insert(node.id.clone(), node).is_some() {
                return Err(ProtocolError::DuplicateNode(node.id.clone()));
            }
        }

        if !nodes.contains_key(&self.root) {
            return Err(ProtocolError::MissingNode(self.root.clone()));
        }

        let mut parents = BTreeMap::<NodeId, usize>::new();
        for node in &self.nodes {
            let mut local = BTreeSet::new();
            for child in &node.children {
                if !nodes.contains_key(child) {
                    return Err(ProtocolError::DanglingChild {
                        parent: node.id.clone(),
                        child: child.clone(),
                    });
                }
                if !local.insert(child.clone()) {
                    return Err(ProtocolError::DuplicateChild {
                        parent: node.id.clone(),
                        child: child.clone(),
                    });
                }
                let count = parents.entry(child.clone()).or_default();
                *count += 1;
                if *count > 1 {
                    return Err(ProtocolError::MultipleParents(child.clone()));
                }
            }
        }

        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        visit(&self.root, &nodes, &mut visiting, &mut visited)?;
        if visited.len() != nodes.len() {
            let node = nodes
                .keys()
                .find(|node| !visited.contains(*node))
                .expect("node counts differ")
                .clone();
            return Err(ProtocolError::UnreachableNode(node));
        }
        Ok(())
    }

    /// Apply an ordered patch atomically. `self` is unchanged if any operation
    /// or the final document violates a protocol invariant.
    pub fn apply_patch(&mut self, patch: &UiPatch) -> Result<(), ProtocolError> {
        self.validate()?;
        patch.validate()?;
        if self.document_id != patch.document_id {
            return Err(ProtocolError::DocumentIdMismatch {
                expected: self.document_id.clone(),
                found: patch.document_id.clone(),
            });
        }
        if self.revision != patch.base_revision {
            return Err(ProtocolError::RevisionMismatch {
                expected: self.revision,
                found: patch.base_revision,
            });
        }

        let mut next = self.clone();
        for operation in &patch.operations {
            operation.apply(&mut next)?;
        }
        next.revision = patch.revision;
        next.nodes.sort_by(|left, right| left.id.cmp(&right.id));
        next.validate()?;
        *self = next;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiPatch {
    pub protocol: String,
    pub document_id: DocumentId,
    pub base_revision: Revision,
    pub revision: Revision,
    pub operations: Vec<PatchOperation>,
}

impl UiPatch {
    pub fn new(
        document_id: impl Into<DocumentId>,
        base_revision: Revision,
        revision: Revision,
        operations: Vec<PatchOperation>,
    ) -> Self {
        Self {
            protocol: UI_PROTOCOL_VERSION.to_owned(),
            document_id: document_id.into(),
            base_revision,
            revision,
            operations,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        require_protocol(UI_PROTOCOL_VERSION, &self.protocol)?;
        require_id("document_id", self.document_id.as_str())?;
        if self.revision <= self.base_revision {
            return Err(ProtocolError::NonIncreasingRevision {
                base: self.base_revision,
                next: self.revision,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PatchOperation {
    UpsertNode {
        node: UiNode,
    },
    RemoveNode {
        node_id: NodeId,
    },
    SetRoot {
        node_id: NodeId,
    },
    SetProperty {
        node_id: NodeId,
        name: String,
        value: WireValue,
    },
    RemoveProperty {
        node_id: NodeId,
        name: String,
    },
    ReplaceChildren {
        node_id: NodeId,
        children: Vec<NodeId>,
    },
    ReplaceActions {
        node_id: NodeId,
        actions: Vec<ActionBinding>,
    },
}

impl PatchOperation {
    fn apply(&self, document: &mut UiDocument) -> Result<(), ProtocolError> {
        match self {
            Self::UpsertNode { node } => {
                if let Some(existing) = document.nodes.iter_mut().find(|item| item.id == node.id) {
                    *existing = node.clone();
                } else {
                    document.nodes.push(node.clone());
                }
            }
            Self::RemoveNode { node_id } => {
                if !document.nodes.iter().any(|node| &node.id == node_id) {
                    return Err(ProtocolError::MissingNode(node_id.clone()));
                }
                document.nodes.retain(|node| &node.id != node_id);
            }
            Self::SetRoot { node_id } => document.root = node_id.clone(),
            Self::SetProperty {
                node_id,
                name,
                value,
            } => {
                node_mut(document, node_id)?
                    .properties
                    .insert(name.clone(), value.clone());
            }
            Self::RemoveProperty { node_id, name } => {
                node_mut(document, node_id)?.properties.remove(name);
            }
            Self::ReplaceChildren { node_id, children } => {
                node_mut(document, node_id)?.children = children.clone();
            }
            Self::ReplaceActions { node_id, actions } => {
                node_mut(document, node_id)?.actions = actions.clone();
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRequest {
    pub document_id: DocumentId,
    pub revision: Revision,
    pub action_id: ActionId,
    pub placement: ActionPlacement,
    pub correlation_id: CorrelationId,
    pub idempotency_key: IdempotencyKey,
    pub payload: WireValue,
}

impl ActionRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        require_id("document_id", self.document_id.as_str())?;
        require_id("action_id", self.action_id.as_str())?;
        require_id("correlation_id", self.correlation_id.as_str())?;
        require_id("idempotency_key", self.idempotency_key.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeToHostMessage {
    Snapshot {
        protocol: String,
        document: UiDocument,
    },
    Patch {
        protocol: String,
        patch: UiPatch,
    },
}

impl RuntimeToHostMessage {
    pub fn snapshot(document: UiDocument) -> Self {
        Self::Snapshot {
            protocol: UI_MESSAGE_PROTOCOL_VERSION.to_owned(),
            document,
        }
    }

    pub fn patch(patch: UiPatch) -> Self {
        Self::Patch {
            protocol: UI_MESSAGE_PROTOCOL_VERSION.to_owned(),
            patch,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Snapshot { protocol, document } => {
                require_protocol(UI_MESSAGE_PROTOCOL_VERSION, protocol)?;
                document.validate()
            }
            Self::Patch { protocol, patch } => {
                require_protocol(UI_MESSAGE_PROTOCOL_VERSION, protocol)?;
                patch.validate()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToRuntimeMessage {
    InvokeAction {
        protocol: String,
        request: ActionRequest,
    },
}

impl HostToRuntimeMessage {
    pub fn invoke_action(request: ActionRequest) -> Self {
        Self::InvokeAction {
            protocol: UI_MESSAGE_PROTOCOL_VERSION.to_owned(),
            request,
        }
    }

    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::InvokeAction { protocol, request } => {
                require_protocol(UI_MESSAGE_PROTOCOL_VERSION, protocol)?;
                request.validate()
            }
        }
    }
}

pub fn encode_runtime_message(message: &RuntimeToHostMessage) -> serde_json::Result<String> {
    serde_json::to_string(message)
}

pub fn decode_runtime_message(input: &str) -> serde_json::Result<RuntimeToHostMessage> {
    serde_json::from_str(input)
}

pub fn encode_host_message(message: &HostToRuntimeMessage) -> serde_json::Result<String> {
    serde_json::to_string(message)
}

pub fn decode_host_message(input: &str) -> serde_json::Result<HostToRuntimeMessage> {
    serde_json::from_str(input)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    UnsupportedProtocol {
        expected: &'static str,
        found: String,
    },
    EmptyIdentifier(&'static str),
    EmptyNodeKind(NodeId),
    EmptyActionEvent(NodeId),
    DuplicateNode(NodeId),
    MissingNode(NodeId),
    DanglingChild {
        parent: NodeId,
        child: NodeId,
    },
    DuplicateChild {
        parent: NodeId,
        child: NodeId,
    },
    MultipleParents(NodeId),
    Cycle(NodeId),
    UnreachableNode(NodeId),
    DocumentIdMismatch {
        expected: DocumentId,
        found: DocumentId,
    },
    RevisionMismatch {
        expected: Revision,
        found: Revision,
    },
    NonIncreasingRevision {
        base: Revision,
        next: Revision,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProtocol { expected, found } => {
                write!(f, "unsupported protocol {found:?}; expected {expected:?}")
            }
            Self::EmptyIdentifier(field) => write!(f, "{field} must not be empty"),
            Self::EmptyNodeKind(node) => write!(f, "node {node} has an empty kind"),
            Self::EmptyActionEvent(node) => write!(f, "node {node} has an empty action event"),
            Self::DuplicateNode(node) => write!(f, "duplicate node id {node}"),
            Self::MissingNode(node) => write!(f, "node {node} does not exist"),
            Self::DanglingChild { parent, child } => {
                write!(f, "node {parent} references missing child {child}")
            }
            Self::DuplicateChild { parent, child } => {
                write!(f, "node {parent} references child {child} more than once")
            }
            Self::MultipleParents(node) => write!(f, "node {node} has more than one parent"),
            Self::Cycle(node) => write!(f, "semantic UI tree contains a cycle at {node}"),
            Self::UnreachableNode(node) => write!(f, "node {node} is unreachable from the root"),
            Self::DocumentIdMismatch { expected, found } => {
                write!(f, "patch targets document {found}, expected {expected}")
            }
            Self::RevisionMismatch { expected, found } => {
                write!(
                    f,
                    "patch base revision is {}, expected {}",
                    found.0, expected.0
                )
            }
            Self::NonIncreasingRevision { base, next } => write!(
                f,
                "patch revision {} must be greater than base revision {}",
                next.0, base.0
            ),
        }
    }
}

impl std::error::Error for ProtocolError {}

fn require_protocol(expected: &'static str, found: &str) -> Result<(), ProtocolError> {
    if found == expected {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedProtocol {
            expected,
            found: found.to_owned(),
        })
    }
}

fn require_id(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    if value.trim().is_empty() {
        Err(ProtocolError::EmptyIdentifier(field))
    } else {
        Ok(())
    }
}

fn node_mut<'a>(
    document: &'a mut UiDocument,
    node_id: &NodeId,
) -> Result<&'a mut UiNode, ProtocolError> {
    document
        .nodes
        .iter_mut()
        .find(|node| &node.id == node_id)
        .ok_or_else(|| ProtocolError::MissingNode(node_id.clone()))
}

fn visit(
    node_id: &NodeId,
    nodes: &BTreeMap<NodeId, &UiNode>,
    visiting: &mut BTreeSet<NodeId>,
    visited: &mut BTreeSet<NodeId>,
) -> Result<(), ProtocolError> {
    if visited.contains(node_id) {
        return Ok(());
    }
    if !visiting.insert(node_id.clone()) {
        return Err(ProtocolError::Cycle(node_id.clone()));
    }
    for child in &nodes
        .get(node_id)
        .expect("child references are checked before traversal")
        .children
    {
        visit(child, nodes, visiting, visited)?;
    }
    visiting.remove(node_id);
    visited.insert(node_id.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: &str, kind: &str) -> UiNode {
        UiNode::new(NodeId::from(id), kind)
    }

    fn document() -> UiDocument {
        let mut root = UiNode::new(NodeId::from("root"), "column");
        root.children.push(NodeId::from("task-1"));
        UiDocument::new(
            DocumentId::from("tasks"),
            Revision(1),
            NodeId::from("root"),
            vec![root, leaf("task-1", "text")],
        )
    }

    #[test]
    fn revision_round_trips_full_u64_range_as_text() {
        let revision = Revision(u64::MAX);
        let json = serde_json::to_string(&revision).unwrap();
        assert_eq!(json, format!("\"{}\"", u64::MAX));
        assert_eq!(serde_json::from_str::<Revision>(&json).unwrap(), revision);
    }

    #[test]
    fn wire_values_round_trip_without_numeric_or_binary_loss() {
        let nan_bits = 0x7ff8_0000_0000_0042_u64;
        let values = [
            WireValue::Null,
            WireValue::Bool(true),
            WireValue::I64(LosslessI64(i64::MIN)),
            WireValue::I64(LosslessI64(i64::MAX)),
            WireValue::F64(F64Bits(nan_bits)),
            WireValue::String("héllo 🌍".to_owned()),
            WireValue::Bytes(vec![0, 1, 127, 128, 255]),
        ];
        for value in values {
            let json = serde_json::to_string(&value).unwrap();
            assert_eq!(serde_json::from_str::<WireValue>(&json).unwrap(), value);
        }

        let integer = serde_json::to_value(WireValue::from(i64::MAX)).unwrap();
        assert_eq!(
            integer["value"],
            serde_json::Value::String(i64::MAX.to_string())
        );
        let float = serde_json::to_value(WireValue::F64(F64Bits(nan_bits))).unwrap();
        assert_eq!(
            float["value"],
            serde_json::Value::String("7ff8000000000042".into())
        );
        assert_eq!(F64Bits(nan_bits).to_f64().to_bits(), nan_bits);
    }

    #[test]
    fn object_encoding_is_deterministic() {
        let mut object = BTreeMap::new();
        object.insert("z".to_owned(), WireValue::from(2_i64));
        object.insert("a".to_owned(), WireValue::from(1_i64));
        let value = WireValue::Object(object);
        let first = serde_json::to_string(&value).unwrap();
        let second = serde_json::to_string(&value).unwrap();
        assert_eq!(first, second);
        assert!(first.find("\"a\"").unwrap() < first.find("\"z\"").unwrap());
    }

    #[test]
    fn action_messages_require_delivery_and_placement_metadata() {
        let request = ActionRequest {
            document_id: DocumentId::from("tasks"),
            revision: Revision(7),
            action_id: ActionId::from("task.complete"),
            placement: ActionPlacement::Server,
            correlation_id: CorrelationId::from("trace-123"),
            idempotency_key: IdempotencyKey::from("mutation-456"),
            payload: WireValue::from(i64::MAX),
        };
        let message = HostToRuntimeMessage::invoke_action(request);
        message.validate().unwrap();
        let json = encode_host_message(&message).unwrap();
        assert_eq!(decode_host_message(&json).unwrap(), message);
    }

    #[test]
    fn patches_are_ordered_revision_checked_and_atomic() {
        let mut state = document();
        let patch = UiPatch::new(
            DocumentId::from("tasks"),
            Revision(1),
            Revision(2),
            vec![
                PatchOperation::UpsertNode {
                    node: leaf("task-2", "text"),
                },
                PatchOperation::SetProperty {
                    node_id: NodeId::from("task-2"),
                    name: "text".to_owned(),
                    value: WireValue::from("second task"),
                },
                PatchOperation::ReplaceChildren {
                    node_id: NodeId::from("root"),
                    children: vec![NodeId::from("task-1"), NodeId::from("task-2")],
                },
            ],
        );
        state.apply_patch(&patch).unwrap();
        assert_eq!(state.revision, Revision(2));
        assert!(matches!(
            state.apply_patch(&patch),
            Err(ProtocolError::RevisionMismatch { .. })
        ));

        let before = state.clone();
        let invalid = UiPatch::new(
            DocumentId::from("tasks"),
            Revision(2),
            Revision(3),
            vec![
                PatchOperation::SetProperty {
                    node_id: NodeId::from("task-1"),
                    name: "text".to_owned(),
                    value: WireValue::from("changed"),
                },
                PatchOperation::RemoveNode {
                    node_id: NodeId::from("missing"),
                },
            ],
        );
        assert!(state.apply_patch(&invalid).is_err());
        assert_eq!(state, before);
    }

    #[test]
    fn tree_validation_rejects_cycles_orphans_and_wrong_versions() {
        let mut root = UiNode::new(NodeId::from("root"), "column");
        root.children.push(NodeId::from("child"));
        let mut child = UiNode::new(NodeId::from("child"), "column");
        child.children.push(NodeId::from("root"));
        let cycle = UiDocument::new(
            DocumentId::from("cycle"),
            Revision(1),
            NodeId::from("root"),
            vec![root, child],
        );
        assert!(matches!(cycle.validate(), Err(ProtocolError::Cycle(_))));

        let orphan = UiDocument::new(
            DocumentId::from("orphan"),
            Revision(1),
            NodeId::from("root"),
            vec![leaf("root", "column"), leaf("detached", "text")],
        );
        assert!(matches!(
            orphan.validate(),
            Err(ProtocolError::UnreachableNode(_))
        ));

        let mut wrong = document();
        wrong.protocol = "nulang-ui/999".to_owned();
        assert!(matches!(
            wrong.validate(),
            Err(ProtocolError::UnsupportedProtocol { .. })
        ));
    }

    #[test]
    fn runtime_message_round_trip_preserves_patch_order() {
        let patch = UiPatch::new(
            DocumentId::from("tasks"),
            Revision(4),
            Revision(5),
            vec![
                PatchOperation::SetProperty {
                    node_id: NodeId::from("task-1"),
                    name: "completed".to_owned(),
                    value: WireValue::Bool(true),
                },
                PatchOperation::SetProperty {
                    node_id: NodeId::from("task-1"),
                    name: "title".to_owned(),
                    value: WireValue::from("done"),
                },
            ],
        );
        let message = RuntimeToHostMessage::patch(patch);
        message.validate().unwrap();
        let json = encode_runtime_message(&message).unwrap();
        assert_eq!(decode_runtime_message(&json).unwrap(), message);
    }
}
