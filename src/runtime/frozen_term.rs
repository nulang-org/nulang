//! Pointer-free immutable term graphs stored in the node-shared object store.
//!
//! The encoded form contains only scalar values, UTF-8/byte payloads, and
//! 32-bit node indices. It never embeds process pointers, actor-heap addresses,
//! or Rust object addresses, so the same bytes can be shared across runtime
//! shards and copied across nodes through the existing object wire path.
//!
//! Format v1:
//! - 4 bytes magic ("NFT1")
//! - 1 byte version
//! - 1 byte flags (currently zero)
//! - 2 reserved zero bytes
//! - u32 root node id
//! - u32 node count
//! - node_count * u32 absolute node offsets
//! - variable-length node records
//!
//! The offset table makes random node lookup O(1) without materializing a
//! pointer graph. Child relationships are node ids, so DAG sharing is preserved.

use std::fmt;

pub type FrozenNodeId = u32;

const MAGIC: &[u8; 4] = b"NFT1";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 16;

const TAG_NIL: u8 = 0;
const TAG_UNIT: u8 = 1;
const TAG_BOOL: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_FLOAT: u8 = 4;
const TAG_BYTES: u8 = 5;
const TAG_STRING: u8 = 6;
const TAG_ARRAY: u8 = 7;
const TAG_TUPLE: u8 = 8;
const TAG_RECORD: u8 = 9;
const TAG_MAP: u8 = 10;
const TAG_VARIANT: u8 = 11;

/// Hard decode limits prevent hostile length prefixes from causing unbounded
/// allocation or iteration work before the object is rejected.
pub const MAX_FROZEN_NODES: usize = 1_000_000;
pub const MAX_FROZEN_ITEMS: usize = 1_000_000;
pub const MAX_FROZEN_SCALAR_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum FrozenNode {
    Nil,
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Bytes(Vec<u8>),
    String(String),
    Array(Vec<FrozenNodeId>),
    Tuple(Vec<FrozenNodeId>),
    Record(Vec<(String, FrozenNodeId)>),
    Map(Vec<(FrozenNodeId, FrozenNodeId)>),
    Variant {
        tag: String,
        payload: Option<FrozenNodeId>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrozenGraph {
    nodes: Vec<FrozenNode>,
    root: FrozenNodeId,
}

impl FrozenGraph {
    pub fn new(nodes: Vec<FrozenNode>, root: FrozenNodeId) -> Result<Self, FrozenTermError> {
        let graph = Self { nodes, root };
        graph.validate()?;
        Ok(graph)
    }

    pub fn root(&self) -> FrozenNodeId {
        self.root
    }

    pub fn nodes(&self) -> &[FrozenNode] {
        &self.nodes
    }

    pub fn encode(&self) -> Result<Vec<u8>, FrozenTermError> {
        self.validate()?;

        let index_bytes = self
            .nodes
            .len()
            .checked_mul(4)
            .ok_or(FrozenTermError::TooLarge("offset table"))?;
        let body_start = HEADER_LEN
            .checked_add(index_bytes)
            .ok_or(FrozenTermError::TooLarge("header"))?;
        if body_start > u32::MAX as usize {
            return Err(FrozenTermError::TooLarge("header"));
        }

        let mut out = vec![0u8; body_start];
        out[0..4].copy_from_slice(MAGIC);
        out[4] = VERSION;
        out[5] = 0;
        out[6] = 0;
        out[7] = 0;
        write_u32_at(&mut out, 8, self.root);
        write_u32_at(&mut out, 12, self.nodes.len() as u32);

        for (idx, node) in self.nodes.iter().enumerate() {
            if out.len() > u32::MAX as usize {
                return Err(FrozenTermError::TooLarge("encoded graph"));
            }
            let node_offset = out.len() as u32;
            write_u32_at(&mut out, HEADER_LEN + idx * 4, node_offset);
            encode_node(&mut out, node)?;
        }

        if out.len() > u32::MAX as usize {
            return Err(FrozenTermError::TooLarge("encoded graph"));
        }
        Ok(out)
    }

    fn validate(&self) -> Result<(), FrozenTermError> {
        let count = self.nodes.len();
        if count == 0 {
            return Err(FrozenTermError::EmptyGraph);
        }
        if count > MAX_FROZEN_NODES || count > u32::MAX as usize {
            return Err(FrozenTermError::TooManyNodes(count));
        }
        if self.root as usize >= count {
            return Err(FrozenTermError::RootOutOfRange {
                root: self.root,
                nodes: count,
            });
        }

        for (idx, node) in self.nodes.iter().enumerate() {
            validate_owned_node(idx as u32, node, count)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrozenTermError {
    EmptyGraph,
    TooManyNodes(usize),
    TooManyItems(usize),
    ScalarTooLarge(usize),
    TooLarge(&'static str),
    RootOutOfRange {
        root: FrozenNodeId,
        nodes: usize,
    },
    ChildOutOfRange {
        node: FrozenNodeId,
        child: FrozenNodeId,
        nodes: usize,
    },
    Truncated,
    InvalidMagic,
    UnsupportedVersion(u8),
    UnsupportedFlags(u8),
    InvalidOffset,
    InvalidTag(u8),
    InvalidBool(u8),
    InvalidUtf8,
    TrailingNodeBytes,
}

impl fmt::Display for FrozenTermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGraph => write!(f, "frozen graph must contain at least one node"),
            Self::TooManyNodes(n) => write!(f, "frozen graph has too many nodes: {n}"),
            Self::TooManyItems(n) => write!(f, "frozen collection has too many items: {n}"),
            Self::ScalarTooLarge(n) => write!(f, "frozen scalar payload is too large: {n} bytes"),
            Self::TooLarge(what) => write!(f, "frozen {what} exceeds the v1 format limit"),
            Self::RootOutOfRange { root, nodes } => {
                write!(f, "frozen root {root} is outside {nodes} nodes")
            }
            Self::ChildOutOfRange { node, child, nodes } => {
                write!(
                    f,
                    "frozen node {node} references child {child} outside {nodes} nodes"
                )
            }
            Self::Truncated => write!(f, "truncated frozen graph"),
            Self::InvalidMagic => write!(f, "invalid frozen graph magic"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported frozen graph version {v}"),
            Self::UnsupportedFlags(v) => write!(f, "unsupported frozen graph flags {v:#x}"),
            Self::InvalidOffset => write!(f, "invalid frozen node offset table"),
            Self::InvalidTag(tag) => write!(f, "invalid frozen node tag {tag}"),
            Self::InvalidBool(v) => write!(f, "invalid frozen bool payload {v}"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 in frozen graph"),
            Self::TrailingNodeBytes => write!(f, "frozen node contains trailing bytes"),
        }
    }
}

impl std::error::Error for FrozenTermError {}

/// Zero-copy validated view over an encoded frozen graph.
#[derive(Debug, Clone, Copy)]
pub struct FrozenGraphView<'a> {
    bytes: &'a [u8],
    root: FrozenNodeId,
    node_count: u32,
    body_start: usize,
}

impl<'a> FrozenGraphView<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, FrozenTermError> {
        if bytes.len() < HEADER_LEN {
            return Err(FrozenTermError::Truncated);
        }
        if &bytes[0..4] != MAGIC {
            return Err(FrozenTermError::InvalidMagic);
        }
        if bytes[4] != VERSION {
            return Err(FrozenTermError::UnsupportedVersion(bytes[4]));
        }
        if bytes[5] != 0 {
            return Err(FrozenTermError::UnsupportedFlags(bytes[5]));
        }
        if bytes[6] != 0 || bytes[7] != 0 {
            return Err(FrozenTermError::UnsupportedFlags(bytes[6] | bytes[7]));
        }

        let root = read_u32_at(bytes, 8)?;
        let node_count = read_u32_at(bytes, 12)?;
        let count = node_count as usize;
        if count == 0 {
            return Err(FrozenTermError::EmptyGraph);
        }
        if count > MAX_FROZEN_NODES {
            return Err(FrozenTermError::TooManyNodes(count));
        }
        if root >= node_count {
            return Err(FrozenTermError::RootOutOfRange { root, nodes: count });
        }

        let index_bytes = count
            .checked_mul(4)
            .ok_or(FrozenTermError::TooLarge("offset table"))?;
        let body_start = HEADER_LEN
            .checked_add(index_bytes)
            .ok_or(FrozenTermError::TooLarge("header"))?;
        if body_start > bytes.len() {
            return Err(FrozenTermError::Truncated);
        }

        let view = Self {
            bytes,
            root,
            node_count,
            body_start,
        };

        let mut previous = None;
        for node_id in 0..node_count {
            let start = view.node_offset(node_id)?;
            if start < body_start || start >= bytes.len() {
                return Err(FrozenTermError::InvalidOffset);
            }
            if node_id == 0 && start != body_start {
                return Err(FrozenTermError::InvalidOffset);
            }
            if let Some(prev) = previous {
                if start <= prev {
                    return Err(FrozenTermError::InvalidOffset);
                }
            }
            previous = Some(start);
        }

        for node_id in 0..node_count {
            let (start, end) = view.node_bounds(node_id)?;
            validate_encoded_node(node_id, &bytes[start..end], node_count)?;
        }

        Ok(view)
    }

    pub fn root(&self) -> FrozenNodeId {
        self.root
    }

    pub fn node_count(&self) -> usize {
        self.node_count as usize
    }

    pub fn encoded_len(&self) -> usize {
        self.bytes.len()
    }

    pub fn node(&self, id: FrozenNodeId) -> Result<FrozenNodeView<'a>, FrozenTermError> {
        if id >= self.node_count {
            return Err(FrozenTermError::ChildOutOfRange {
                node: id,
                child: id,
                nodes: self.node_count as usize,
            });
        }
        let (start, end) = self.node_bounds(id)?;
        decode_node_view(&self.bytes[start..end])
    }

    fn node_offset(&self, id: FrozenNodeId) -> Result<usize, FrozenTermError> {
        if id >= self.node_count {
            return Err(FrozenTermError::InvalidOffset);
        }
        Ok(read_u32_at(self.bytes, HEADER_LEN + id as usize * 4)? as usize)
    }

    fn node_bounds(&self, id: FrozenNodeId) -> Result<(usize, usize), FrozenTermError> {
        let start = self.node_offset(id)?;
        let end = if id + 1 < self.node_count {
            self.node_offset(id + 1)?
        } else {
            self.bytes.len()
        };
        if start < self.body_start || start >= end || end > self.bytes.len() {
            return Err(FrozenTermError::InvalidOffset);
        }
        Ok((start, end))
    }
}

#[derive(Debug, Clone)]
pub enum FrozenNodeView<'a> {
    Nil,
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Bytes(&'a [u8]),
    String(&'a str),
    Array(FrozenRefs<'a>),
    Tuple(FrozenRefs<'a>),
    Record(FrozenRecordFields<'a>),
    Map(FrozenMapEntries<'a>),
    Variant {
        tag: &'a str,
        payload: Option<FrozenNodeId>,
    },
}

#[derive(Debug, Clone)]
pub struct FrozenRefs<'a> {
    bytes: &'a [u8],
    pos: usize,
    remaining: usize,
}

impl Iterator for FrozenRefs<'_> {
    type Item = FrozenNodeId;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let id = u32::from_le_bytes(self.bytes[self.pos..self.pos + 4].try_into().ok()?);
        self.pos += 4;
        self.remaining -= 1;
        Some(id)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for FrozenRefs<'_> {}

#[derive(Debug, Clone)]
pub struct FrozenRecordFields<'a> {
    bytes: &'a [u8],
    pos: usize,
    remaining: usize,
}

impl<'a> Iterator for FrozenRecordFields<'a> {
    type Item = (&'a str, FrozenNodeId);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let name_len = read_u32_slice(self.bytes, &mut self.pos).ok()? as usize;
        let end = self.pos.checked_add(name_len)?;
        let name = std::str::from_utf8(self.bytes.get(self.pos..end)?).ok()?;
        self.pos = end;
        let child = read_u32_slice(self.bytes, &mut self.pos).ok()?;
        self.remaining -= 1;
        Some((name, child))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for FrozenRecordFields<'_> {}

#[derive(Debug, Clone)]
pub struct FrozenMapEntries<'a> {
    bytes: &'a [u8],
    pos: usize,
    remaining: usize,
}

impl Iterator for FrozenMapEntries<'_> {
    type Item = (FrozenNodeId, FrozenNodeId);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let key = read_u32_slice(self.bytes, &mut self.pos).ok()?;
        let value = read_u32_slice(self.bytes, &mut self.pos).ok()?;
        self.remaining -= 1;
        Some((key, value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for FrozenMapEntries<'_> {}

fn validate_owned_node(
    node_id: FrozenNodeId,
    node: &FrozenNode,
    node_count: usize,
) -> Result<(), FrozenTermError> {
    let check_ref = |child: FrozenNodeId| {
        if child as usize >= node_count {
            Err(FrozenTermError::ChildOutOfRange {
                node: node_id,
                child,
                nodes: node_count,
            })
        } else {
            Ok(())
        }
    };
    let check_items = |n: usize| {
        if n > MAX_FROZEN_ITEMS || n > u32::MAX as usize {
            Err(FrozenTermError::TooManyItems(n))
        } else {
            Ok(())
        }
    };
    let check_scalar = |n: usize| {
        if n > MAX_FROZEN_SCALAR_BYTES || n > u32::MAX as usize {
            Err(FrozenTermError::ScalarTooLarge(n))
        } else {
            Ok(())
        }
    };

    match node {
        FrozenNode::Nil
        | FrozenNode::Unit
        | FrozenNode::Bool(_)
        | FrozenNode::Int(_)
        | FrozenNode::Float(_) => {}
        FrozenNode::Bytes(bytes) => check_scalar(bytes.len())?,
        FrozenNode::String(text) => check_scalar(text.len())?,
        FrozenNode::Array(children) | FrozenNode::Tuple(children) => {
            check_items(children.len())?;
            for &child in children {
                check_ref(child)?;
            }
        }
        FrozenNode::Record(fields) => {
            check_items(fields.len())?;
            for (name, child) in fields {
                check_scalar(name.len())?;
                check_ref(*child)?;
            }
        }
        FrozenNode::Map(entries) => {
            check_items(entries.len())?;
            for &(key, value) in entries {
                check_ref(key)?;
                check_ref(value)?;
            }
        }
        FrozenNode::Variant { tag, payload } => {
            check_scalar(tag.len())?;
            if let Some(child) = payload {
                check_ref(*child)?;
            }
        }
    }
    Ok(())
}

fn encode_node(out: &mut Vec<u8>, node: &FrozenNode) -> Result<(), FrozenTermError> {
    match node {
        FrozenNode::Nil => out.push(TAG_NIL),
        FrozenNode::Unit => out.push(TAG_UNIT),
        FrozenNode::Bool(value) => {
            out.push(TAG_BOOL);
            out.push(u8::from(*value));
        }
        FrozenNode::Int(value) => {
            out.push(TAG_INT);
            out.extend_from_slice(&value.to_le_bytes());
        }
        FrozenNode::Float(value) => {
            out.push(TAG_FLOAT);
            let bits = crate::value_layout::float_bits(*value);
            out.extend_from_slice(&bits.to_le_bytes());
        }
        FrozenNode::Bytes(bytes) => {
            out.push(TAG_BYTES);
            push_len_prefixed(out, bytes)?;
        }
        FrozenNode::String(text) => {
            out.push(TAG_STRING);
            push_len_prefixed(out, text.as_bytes())?;
        }
        FrozenNode::Array(children) => {
            out.push(TAG_ARRAY);
            push_u32(out, children.len() as u32);
            for child in children {
                push_u32(out, *child);
            }
        }
        FrozenNode::Tuple(children) => {
            out.push(TAG_TUPLE);
            push_u32(out, children.len() as u32);
            for child in children {
                push_u32(out, *child);
            }
        }
        FrozenNode::Record(fields) => {
            out.push(TAG_RECORD);
            push_u32(out, fields.len() as u32);
            for (name, child) in fields {
                push_len_prefixed(out, name.as_bytes())?;
                push_u32(out, *child);
            }
        }
        FrozenNode::Map(entries) => {
            out.push(TAG_MAP);
            push_u32(out, entries.len() as u32);
            for (key, value) in entries {
                push_u32(out, *key);
                push_u32(out, *value);
            }
        }
        FrozenNode::Variant { tag, payload } => {
            out.push(TAG_VARIANT);
            push_len_prefixed(out, tag.as_bytes())?;
            match payload {
                Some(child) => {
                    out.push(1);
                    push_u32(out, *child);
                }
                None => out.push(0),
            }
        }
    }
    Ok(())
}

fn validate_encoded_node(
    node_id: FrozenNodeId,
    bytes: &[u8],
    node_count: u32,
) -> Result<(), FrozenTermError> {
    let mut cur = Cursor::new(bytes);
    let tag = cur.u8()?;
    match tag {
        TAG_NIL | TAG_UNIT => {}
        TAG_BOOL => match cur.u8()? {
            0 | 1 => {}
            value => return Err(FrozenTermError::InvalidBool(value)),
        },
        TAG_INT | TAG_FLOAT => {
            cur.take(8)?;
        }
        TAG_BYTES => {
            let bytes = cur.len_prefixed()?;
            if bytes.len() > MAX_FROZEN_SCALAR_BYTES {
                return Err(FrozenTermError::ScalarTooLarge(bytes.len()));
            }
        }
        TAG_STRING => {
            let value = cur.len_prefixed()?;
            if value.len() > MAX_FROZEN_SCALAR_BYTES {
                return Err(FrozenTermError::ScalarTooLarge(value.len()));
            }
            std::str::from_utf8(value).map_err(|_| FrozenTermError::InvalidUtf8)?;
        }
        TAG_ARRAY | TAG_TUPLE => {
            let count = cur.u32()? as usize;
            check_encoded_item_count(count)?;
            for _ in 0..count {
                check_encoded_ref(node_id, cur.u32()?, node_count)?;
            }
        }
        TAG_RECORD => {
            let count = cur.u32()? as usize;
            check_encoded_item_count(count)?;
            for _ in 0..count {
                let name = cur.len_prefixed()?;
                if name.len() > MAX_FROZEN_SCALAR_BYTES {
                    return Err(FrozenTermError::ScalarTooLarge(name.len()));
                }
                std::str::from_utf8(name).map_err(|_| FrozenTermError::InvalidUtf8)?;
                check_encoded_ref(node_id, cur.u32()?, node_count)?;
            }
        }
        TAG_MAP => {
            let count = cur.u32()? as usize;
            check_encoded_item_count(count)?;
            for _ in 0..count {
                check_encoded_ref(node_id, cur.u32()?, node_count)?;
                check_encoded_ref(node_id, cur.u32()?, node_count)?;
            }
        }
        TAG_VARIANT => {
            let name = cur.len_prefixed()?;
            if name.len() > MAX_FROZEN_SCALAR_BYTES {
                return Err(FrozenTermError::ScalarTooLarge(name.len()));
            }
            std::str::from_utf8(name).map_err(|_| FrozenTermError::InvalidUtf8)?;
            match cur.u8()? {
                0 => {}
                1 => check_encoded_ref(node_id, cur.u32()?, node_count)?,
                value => return Err(FrozenTermError::InvalidBool(value)),
            }
        }
        other => return Err(FrozenTermError::InvalidTag(other)),
    }
    if !cur.is_done() {
        return Err(FrozenTermError::TrailingNodeBytes);
    }
    Ok(())
}

fn decode_node_view(bytes: &[u8]) -> Result<FrozenNodeView<'_>, FrozenTermError> {
    let mut cur = Cursor::new(bytes);
    let tag = cur.u8()?;
    let node = match tag {
        TAG_NIL => FrozenNodeView::Nil,
        TAG_UNIT => FrozenNodeView::Unit,
        TAG_BOOL => match cur.u8()? {
            0 => FrozenNodeView::Bool(false),
            1 => FrozenNodeView::Bool(true),
            value => return Err(FrozenTermError::InvalidBool(value)),
        },
        TAG_INT => FrozenNodeView::Int(i64::from_le_bytes(
            cur.take(8)?
                .try_into()
                .map_err(|_| FrozenTermError::Truncated)?,
        )),
        TAG_FLOAT => FrozenNodeView::Float(f64::from_bits(u64::from_le_bytes(
            cur.take(8)?.try_into().map_err(|_| FrozenTermError::Truncated)?,
        ))),
        TAG_BYTES => FrozenNodeView::Bytes(cur.len_prefixed()?),
        TAG_STRING => FrozenNodeView::String(
            std::str::from_utf8(cur.len_prefixed()?).map_err(|_| FrozenTermError::InvalidUtf8)?,
        ),
        TAG_ARRAY | TAG_TUPLE => {
            let count = cur.u32()? as usize;
            let data = cur.take(
                count
                    .checked_mul(4)
                    .ok_or(FrozenTermError::TooLarge("reference list"))?,
            )?;
            let refs = FrozenRefs {
                bytes: data,
                pos: 0,
                remaining: count,
            };
            if tag == TAG_ARRAY {
                FrozenNodeView::Array(refs)
            } else {
                FrozenNodeView::Tuple(refs)
            }
        }
        TAG_RECORD => {
            let count = cur.u32()? as usize;
            let data = cur.remaining();
            cur.pos = cur.bytes.len();
            FrozenNodeView::Record(FrozenRecordFields {
                bytes: data,
                pos: 0,
                remaining: count,
            })
        }
        TAG_MAP => {
            let count = cur.u32()? as usize;
            let data = cur.take(
                count
                    .checked_mul(8)
                    .ok_or(FrozenTermError::TooLarge("map entries"))?,
            )?;
            FrozenNodeView::Map(FrozenMapEntries {
                bytes: data,
                pos: 0,
                remaining: count,
            })
        }
        TAG_VARIANT => {
            let tag = std::str::from_utf8(cur.len_prefixed()?)
                .map_err(|_| FrozenTermError::InvalidUtf8)?;
            let payload = match cur.u8()? {
                0 => None,
                1 => Some(cur.u32()?),
                value => return Err(FrozenTermError::InvalidBool(value)),
            };
            FrozenNodeView::Variant { tag, payload }
        }
        other => return Err(FrozenTermError::InvalidTag(other)),
    };

    if !cur.is_done() {
        return Err(FrozenTermError::TrailingNodeBytes);
    }
    Ok(node)
}

fn check_encoded_item_count(count: usize) -> Result<(), FrozenTermError> {
    if count > MAX_FROZEN_ITEMS {
        Err(FrozenTermError::TooManyItems(count))
    } else {
        Ok(())
    }
}

fn check_encoded_ref(
    node: FrozenNodeId,
    child: FrozenNodeId,
    node_count: u32,
) -> Result<(), FrozenTermError> {
    if child >= node_count {
        Err(FrozenTermError::ChildOutOfRange {
            node,
            child,
            nodes: node_count as usize,
        })
    } else {
        Ok(())
    }
}

fn push_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), FrozenTermError> {
    if bytes.len() > MAX_FROZEN_SCALAR_BYTES || bytes.len() > u32::MAX as usize {
        return Err(FrozenTermError::ScalarTooLarge(bytes.len()));
    }
    push_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
    Ok(())
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u32_at(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn read_u32_at(bytes: &[u8], at: usize) -> Result<u32, FrozenTermError> {
    let end = at.checked_add(4).ok_or(FrozenTermError::Truncated)?;
    let slice = bytes.get(at..end).ok_or(FrozenTermError::Truncated)?;
    Ok(u32::from_le_bytes(
        slice.try_into().map_err(|_| FrozenTermError::Truncated)?,
    ))
}

fn read_u32_slice(bytes: &[u8], pos: &mut usize) -> Result<u32, FrozenTermError> {
    let value = read_u32_at(bytes, *pos)?;
    *pos += 4;
    Ok(value)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn u8(&mut self) -> Result<u8, FrozenTermError> {
        let value = *self.bytes.get(self.pos).ok_or(FrozenTermError::Truncated)?;
        self.pos += 1;
        Ok(value)
    }

    fn u32(&mut self) -> Result<u32, FrozenTermError> {
        read_u32_slice(self.bytes, &mut self.pos)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], FrozenTermError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(FrozenTermError::Truncated)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(FrozenTermError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn len_prefixed(&mut self) -> Result<&'a [u8], FrozenTermError> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }

    fn is_done(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_node_kinds() {
        let graph = FrozenGraph::new(
            vec![
                FrozenNode::Nil,
                FrozenNode::Unit,
                FrozenNode::Bool(true),
                FrozenNode::Int(-42),
                FrozenNode::Float(3.5),
                FrozenNode::Bytes(vec![1, 2, 3]),
                FrozenNode::String("hello".to_string()),
                FrozenNode::Array(vec![0, 2, 3]),
                FrozenNode::Tuple(vec![4, 6]),
                FrozenNode::Record(vec![("name".to_string(), 6), ("value".to_string(), 3)]),
                FrozenNode::Map(vec![(6, 3)]),
                FrozenNode::Variant {
                    tag: "Some".to_string(),
                    payload: Some(3),
                },
            ],
            9,
        )
        .unwrap();

        let encoded = graph.encode().unwrap();
        let view = FrozenGraphView::new(&encoded).unwrap();
        assert_eq!(view.root(), 9);
        assert_eq!(view.node_count(), 12);

        assert!(matches!(view.node(0).unwrap(), FrozenNodeView::Nil));
        assert!(matches!(view.node(1).unwrap(), FrozenNodeView::Unit));
        assert!(matches!(view.node(2).unwrap(), FrozenNodeView::Bool(true)));
        assert!(matches!(view.node(3).unwrap(), FrozenNodeView::Int(-42)));
        assert!(matches!(view.node(4).unwrap(), FrozenNodeView::Float(v) if v == 3.5));
        assert!(matches!(view.node(5).unwrap(), FrozenNodeView::Bytes(v) if v == [1, 2, 3]));
        assert!(matches!(
            view.node(6).unwrap(),
            FrozenNodeView::String("hello")
        ));

        let FrozenNodeView::Array(items) = view.node(7).unwrap() else {
            panic!("array expected");
        };
        assert_eq!(items.collect::<Vec<_>>(), vec![0, 2, 3]);

        let FrozenNodeView::Record(fields) = view.node(9).unwrap() else {
            panic!("record expected");
        };
        assert_eq!(fields.collect::<Vec<_>>(), vec![("name", 6), ("value", 3)]);

        let FrozenNodeView::Map(entries) = view.node(10).unwrap() else {
            panic!("map expected");
        };
        assert_eq!(entries.collect::<Vec<_>>(), vec![(6, 3)]);

        assert!(matches!(
            view.node(11).unwrap(),
            FrozenNodeView::Variant {
                tag: "Some",
                payload: Some(3)
            }
        ));
    }

    #[test]
    fn shared_child_ids_preserve_dag_shape() {
        let graph = FrozenGraph::new(
            vec![
                FrozenNode::String("shared".to_string()),
                FrozenNode::Array(vec![0, 0, 0]),
            ],
            1,
        )
        .unwrap();

        let encoded = graph.encode().unwrap();
        let view = FrozenGraphView::new(&encoded).unwrap();
        let FrozenNodeView::Array(items) = view.node(1).unwrap() else {
            panic!("array expected");
        };
        assert_eq!(items.collect::<Vec<_>>(), vec![0, 0, 0]);
    }

    #[test]
    fn malformed_child_reference_fails_closed() {
        let graph = FrozenGraph::new(vec![FrozenNode::Nil, FrozenNode::Array(vec![0])], 1).unwrap();
        let mut encoded = graph.encode().unwrap();
        let view = FrozenGraphView::new(&encoded).unwrap();
        let (start, _) = view.node_bounds(1).unwrap();
        let child_at = start + 1 + 4;
        encoded[child_at..child_at + 4].copy_from_slice(&99u32.to_le_bytes());

        assert!(matches!(
            FrozenGraphView::new(&encoded),
            Err(FrozenTermError::ChildOutOfRange { child: 99, .. })
        ));
    }

    #[test]
    fn malformed_offset_table_fails_closed() {
        let graph = FrozenGraph::new(vec![FrozenNode::Nil], 0)
            .unwrap()
            .encode()
            .unwrap();
        let mut encoded = graph;
        encoded[16..20].copy_from_slice(&21u32.to_le_bytes());
        assert!(matches!(
            FrozenGraphView::new(&encoded),
            Err(FrozenTermError::InvalidOffset)
        ));
    }

    #[test]
    fn owned_graph_rejects_invalid_reference() {
        assert!(matches!(
            FrozenGraph::new(vec![FrozenNode::Array(vec![1])], 0),
            Err(FrozenTermError::ChildOutOfRange { child: 1, .. })
        ));
    }

    #[test]
    fn nan_encoding_is_canonical() {
        let a = FrozenGraph::new(vec![FrozenNode::Float(f64::NAN)], 0)
            .unwrap()
            .encode()
            .unwrap();
        let b = FrozenGraph::new(
            vec![FrozenNode::Float(f64::from_bits(0x7ff8_1234_5678_9abc))],
            0,
        )
        .unwrap()
        .encode()
        .unwrap();
        assert_eq!(a, b);
    }
}
