//! Stable parent-scoped NodeId generation for server UI templates and SSR hosts.
//!
//! Keys identify siblings, not global nodes. Unkeyed children deliberately use
//! positional IDs; callers must provide a key for reorder-stable identity.

use crate::{DocumentId, NodeId, ProtocolError, Revision, UiDocument, UiNode, WireValue};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

const MAX_KEY_BYTES: usize = 256;
const MAX_ID_BYTES: usize = 1024;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedNode {
    pub kind: String,
    pub key: Option<String>,
    pub properties: BTreeMap<String, WireValue>,
    pub children: Vec<Self>,
}

impl KeyedNode {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            key: None,
            properties: BTreeMap::new(),
            children: Vec::new(),
        }
    }

    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn with_property(mut self, name: impl Into<String>, value: WireValue) -> Self {
        self.properties.insert(name.into(), value);
        self
    }

    pub fn with_children(mut self, children: Vec<Self>) -> Self {
        self.children = children;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyedTreeError {
    InvalidKey,
    DuplicateSiblingKey(String),
    DepthExceeded,
    TooManyNodes,
    NodeIdTooLong,
    Protocol(ProtocolError),
}

impl fmt::Display for KeyedTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for KeyedTreeError {}

/// Produce exactly the same IDs as the browser SSR adapter:
/// `root/k<lowercase-utf8-hex>` for keyed siblings, `root/p<index>`
/// for positional ones. The same rule recurses under each parent ID.
pub fn document_from_keyed_tree(
    document_id: impl Into<DocumentId>,
    revision: Revision,
    root: &KeyedNode,
) -> Result<UiDocument, KeyedTreeError> {
    fn visit(
        template: &KeyedNode,
        id: String,
        depth: usize,
        nodes: &mut Vec<UiNode>,
    ) -> Result<(), KeyedTreeError> {
        if depth > MAX_DEPTH {
            return Err(KeyedTreeError::DepthExceeded);
        }
        if id.len() > MAX_ID_BYTES {
            return Err(KeyedTreeError::NodeIdTooLong);
        }
        if nodes.len() >= MAX_NODES {
            return Err(KeyedTreeError::TooManyNodes);
        }
        let mut node = UiNode::new(NodeId::new(id.clone()), template.kind.clone());
        node.properties = template.properties.clone();
        let mut keyed_siblings = BTreeSet::new();
        for (position, child) in template.children.iter().enumerate() {
            let segment = match &child.key {
                Some(key) => {
                    if key.trim().is_empty()
                        || key.len() > MAX_KEY_BYTES
                        || key.chars().any(char::is_control)
                    {
                        return Err(KeyedTreeError::InvalidKey);
                    }
                    if !keyed_siblings.insert(key.as_str()) {
                        return Err(KeyedTreeError::DuplicateSiblingKey(key.clone()));
                    }
                    let mut out = String::with_capacity(1 + key.len() * 2);
                    out.push('k');
                    for byte in key.bytes() {
                        use std::fmt::Write as _;
                        write!(&mut out, "{byte:02x}").expect("writing into string");
                    }
                    out
                }
                None => format!("p{position}"),
            };
            node.children.push(NodeId::new(format!("{id}/{segment}")));
        }
        // Push parent first; the final document constructor sorts by ID.
        nodes.push(node.clone());
        for (child, child_id) in template.children.iter().zip(node.children) {
            visit(child, child_id.0, depth + 1, nodes)?;
        }
        Ok(())
    }

    if root.key.is_some() {
        return Err(KeyedTreeError::InvalidKey);
    }
    let mut nodes = Vec::new();
    visit(root, "root".into(), 0, &mut nodes)?;
    let document = UiDocument::new(document_id, revision, NodeId::from("root"), nodes);
    document.validate().map_err(KeyedTreeError::Protocol)?;
    Ok(document)
}
