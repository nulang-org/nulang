//! CRDT Manager for Nulang.
//!
//! The `CrdtManager` owns all local CRDT replicas and handles inter-node
//! synchronization. Actors interact with CRDTs through `CrdtHandle`s, which
//! are lightweight references to the actual CRDT stored in the manager.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::crdt::{AWORSet, Crdt, GCounter, GSet, ORSet, PNCounter};
use super::crdt_reg::{LWWRegister, MVRegister, RGA};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CrdtId(pub u64);

static CRDT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

impl CrdtId {
    /// Mint a node-scoped id: the high 32 bits carry the node id, the low 32
    /// bits a process-global counter. Folding the node id in guarantees ids
    /// created independently on different nodes never collide (each node's
    /// counter starts at the same value, so a bare counter would).
    pub fn new(node_id: u64) -> Self {
        let counter = CRDT_ID_COUNTER.fetch_add(1, Ordering::Relaxed) & 0xFFFF_FFFF;
        CrdtId((node_id << 32) | counter)
    }
}

// Re-export the canonical CrdtType from ast.
pub use crate::ast::CrdtType;

// Legacy to_u8/from_u8 are now on ast::CrdtType.
// The CrdtType import above provides them.

#[derive(Debug, Clone, PartialEq)]
pub struct CrdtOp {
    pub crdt_id: CrdtId,
    pub crdt_type: CrdtType,
    pub payload: Vec<u8>,
}

impl CrdtOp {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.crdt_id.0.to_be_bytes());
        buf.push(self.crdt_type as u8);
        buf.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 13 {
            return None;
        }
        let crdt_id = CrdtId(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]));
        let crdt_type = match bytes[8] {
            0 => CrdtType::GCounter,
            1 => CrdtType::PNCounter,
            2 => CrdtType::GSet,
            3 => CrdtType::ORSet,
            4 => CrdtType::AWORSet,
            5 => CrdtType::LWWRegister,
            6 => CrdtType::MVRegister,
            7 => CrdtType::RGA,
            _ => return None,
        };
        let payload_len = u32::from_be_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]) as usize;
        if bytes.len() < 13 + payload_len {
            return None;
        }
        let payload = bytes[13..13 + payload_len].to_vec();
        Some(CrdtOp {
            crdt_id,
            crdt_type,
            payload,
        })
    }
}

/// A CRDT sync op tagged as either a **delta** (changes since the sender's
/// last sync) or a **full-state** snapshot.
///
/// Deltas are produced by [`CrdtManager::generate_delta_sync_ops`] and ride
/// in `Packet::CrdtDeltaSync`. A delta payload is itself a valid serialized
/// CRDT state, so receivers merge it with the same `merge` used for full
/// states — the difference is only that a delta for an *unknown* entry id
/// is ignored (there is no base to apply it onto), while a full-state op
/// creates the entry, exactly like `CrdtManager::apply_op`.
#[derive(Debug, Clone, PartialEq)]
pub struct CrdtDeltaOp {
    pub op: CrdtOp,
    pub is_delta: bool,
}

impl CrdtDeltaOp {
    /// Wire layout: `[is_delta:u8][CrdtOp bytes]`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.op.payload.len() + 14);
        buf.push(if self.is_delta { 1 } else { 0 });
        buf.extend_from_slice(&self.op.to_bytes());
        buf
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let is_delta = match bytes.first()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        let op = CrdtOp::from_bytes(&bytes[1..])?;
        Some(CrdtDeltaOp { op, is_delta })
    }
}

#[derive(Debug, Clone)]
pub enum CrdtEntry {
    GCounter(GCounter),
    PNCounter(PNCounter),
    GSet(GSet<String>),
    ORSet(ORSet<String>),
    AWORSet(AWORSet<String>),
    LWWRegister(LWWRegister<String>),
    MVRegister(MVRegister<String>),
    RGA(RGA<String>),
}

/// Helper trait mapping CRDT inner types to their `CrdtEntry` variant so
/// [`CrdtManager::entry_mut`] can return typed references without per-type
/// boilerplate accessors.
pub trait CrdtEntryInner: Sized {
    fn try_from_entry(entry: &mut CrdtEntry) -> Option<&mut Self>;
}

impl CrdtEntryInner for GCounter {
    fn try_from_entry(entry: &mut CrdtEntry) -> Option<&mut Self> {
        match entry {
            CrdtEntry::GCounter(c) => Some(c),
            _ => None,
        }
    }
}
impl CrdtEntryInner for PNCounter {
    fn try_from_entry(entry: &mut CrdtEntry) -> Option<&mut Self> {
        match entry {
            CrdtEntry::PNCounter(c) => Some(c),
            _ => None,
        }
    }
}
impl CrdtEntryInner for GSet<String> {
    fn try_from_entry(entry: &mut CrdtEntry) -> Option<&mut Self> {
        match entry {
            CrdtEntry::GSet(c) => Some(c),
            _ => None,
        }
    }
}
impl CrdtEntryInner for ORSet<String> {
    fn try_from_entry(entry: &mut CrdtEntry) -> Option<&mut Self> {
        match entry {
            CrdtEntry::ORSet(c) => Some(c),
            _ => None,
        }
    }
}
impl CrdtEntryInner for AWORSet<String> {
    fn try_from_entry(entry: &mut CrdtEntry) -> Option<&mut Self> {
        match entry {
            CrdtEntry::AWORSet(c) => Some(c),
            _ => None,
        }
    }
}
impl CrdtEntryInner for LWWRegister<String> {
    fn try_from_entry(entry: &mut CrdtEntry) -> Option<&mut Self> {
        match entry {
            CrdtEntry::LWWRegister(c) => Some(c),
            _ => None,
        }
    }
}
impl CrdtEntryInner for MVRegister<String> {
    fn try_from_entry(entry: &mut CrdtEntry) -> Option<&mut Self> {
        match entry {
            CrdtEntry::MVRegister(c) => Some(c),
            _ => None,
        }
    }
}
impl CrdtEntryInner for RGA<String> {
    fn try_from_entry(entry: &mut CrdtEntry) -> Option<&mut Self> {
        match entry {
            CrdtEntry::RGA(c) => Some(c),
            _ => None,
        }
    }
}
impl CrdtEntry {
    pub fn payload_bytes(&self) -> Vec<u8> {
        match self {
            CrdtEntry::GCounter(c) => c.to_bytes(),
            CrdtEntry::PNCounter(c) => c.to_bytes(),
            CrdtEntry::GSet(c) => c.to_bytes(),
            CrdtEntry::ORSet(c) => c.to_bytes(),
            CrdtEntry::AWORSet(c) => c.to_bytes(),
            CrdtEntry::LWWRegister(c) => c.to_bytes(),
            CrdtEntry::MVRegister(c) => c.to_bytes(),
            CrdtEntry::RGA(c) => c.to_bytes(),
        }
    }

    pub fn crdt_type(&self) -> CrdtType {
        match self {
            CrdtEntry::GCounter(_) => CrdtType::GCounter,
            CrdtEntry::PNCounter(_) => CrdtType::PNCounter,
            CrdtEntry::GSet(_) => CrdtType::GSet,
            CrdtEntry::ORSet(_) => CrdtType::ORSet,
            CrdtEntry::AWORSet(_) => CrdtType::AWORSet,
            CrdtEntry::LWWRegister(_) => CrdtType::LWWRegister,
            CrdtEntry::MVRegister(_) => CrdtType::MVRegister,
            CrdtEntry::RGA(_) => CrdtType::RGA,
        }
    }

    pub fn merge_entry(&mut self, other: &CrdtEntry) -> bool {
        match (self, other) {
            (CrdtEntry::GCounter(a), CrdtEntry::GCounter(b)) => {
                a.merge(b);
                true
            }
            (CrdtEntry::PNCounter(a), CrdtEntry::PNCounter(b)) => {
                a.merge(b);
                true
            }
            (CrdtEntry::GSet(a), CrdtEntry::GSet(b)) => {
                a.merge(b);
                true
            }
            (CrdtEntry::ORSet(a), CrdtEntry::ORSet(b)) => {
                a.merge(b);
                true
            }
            (CrdtEntry::AWORSet(a), CrdtEntry::AWORSet(b)) => {
                a.merge(b);
                true
            }
            (CrdtEntry::LWWRegister(a), CrdtEntry::LWWRegister(b)) => {
                a.merge(b);
                true
            }
            (CrdtEntry::MVRegister(a), CrdtEntry::MVRegister(b)) => {
                a.merge(b);
                true
            }
            (CrdtEntry::RGA(a), CrdtEntry::RGA(b)) => {
                a.merge(b);
                true
            }
            _ => false,
        }
    }

    /// Compute the delta-state of this entry relative to `base` (the state
    /// last shipped to peers). Returns `None` when the entry did not change
    /// since `base`. A type mismatch between `self` and `base` yields the
    /// full state as a safe fallback.
    pub fn delta_since(&self, base: &CrdtEntry) -> Option<CrdtEntry> {
        match (self, base) {
            (CrdtEntry::GCounter(a), CrdtEntry::GCounter(b)) => {
                a.delta_since(b).map(CrdtEntry::GCounter)
            }
            (CrdtEntry::PNCounter(a), CrdtEntry::PNCounter(b)) => {
                a.delta_since(b).map(CrdtEntry::PNCounter)
            }
            (CrdtEntry::GSet(a), CrdtEntry::GSet(b)) => a.delta_since(b).map(CrdtEntry::GSet),
            (CrdtEntry::ORSet(a), CrdtEntry::ORSet(b)) => a.delta_since(b).map(CrdtEntry::ORSet),
            (CrdtEntry::AWORSet(a), CrdtEntry::AWORSet(b)) => {
                a.delta_since(b).map(CrdtEntry::AWORSet)
            }
            (CrdtEntry::LWWRegister(a), CrdtEntry::LWWRegister(b)) => {
                a.delta_since(b).map(CrdtEntry::LWWRegister)
            }
            (CrdtEntry::MVRegister(a), CrdtEntry::MVRegister(b)) => {
                a.delta_since(b).map(CrdtEntry::MVRegister)
            }
            (CrdtEntry::RGA(a), CrdtEntry::RGA(b)) => a.delta_since(b).map(CrdtEntry::RGA),
            _ => Some(self.clone()),
        }
    }

    /// Rewrite the replica's *local* node identity so that future local
    /// operations are tagged with `node_id`.  This is used when a replica is
    /// created from a remote sync payload: the remote counts/tags/timestamps
    /// are preserved, but new local increments/inserts must use this manager's
    /// node id.
    pub fn set_local_node_id(&mut self, node_id: u64) {
        match self {
            CrdtEntry::GCounter(c) => c.node_id = node_id,
            CrdtEntry::PNCounter(c) => {
                c.increments.node_id = node_id;
                c.decrements.node_id = node_id;
            }
            CrdtEntry::ORSet(c) => {
                c.node_id = node_id;
                c.matrix.reroot(node_id);
            }
            CrdtEntry::AWORSet(c) => {
                c.clock.node_id = node_id;
                c.matrix.reroot(node_id);
            }
            CrdtEntry::LWWRegister(c) => c.clock.node_id = node_id,
            CrdtEntry::MVRegister(c) => c.clock.node_id = node_id,
            CrdtEntry::RGA(c) => {
                c.clock.node_id = node_id;
                c.matrix.reroot(node_id);
            }
            CrdtEntry::GSet(_) => {}
        }
    }
}

pub struct CrdtManager {
    pub node_id: u64,
    pub entries: HashMap<CrdtId, CrdtEntry>,
    pub ops_synced: u64,
    /// Per-entry snapshot for delta computation.
    pub sync_base: HashMap<CrdtId, CrdtEntry>,
    /// Maps (actor_id, field_name) → CrdtId for CRDT-backed state fields.
    pub field_map: HashMap<(u64, String), CrdtId>,
    /// Reverse map: CrdtId → (actor_id, field_name) for pushing merges.
    pub field_reverse: HashMap<CrdtId, (u64, String)>,
}

/// Merge a serialized CRDT state (full state or delta — both are valid
/// serialized states) into `entry`. Returns `false` when the payload is
/// malformed.
pub fn merge_payload(entry: &mut CrdtEntry, payload: &[u8]) -> bool {
    match entry {
        CrdtEntry::GCounter(c) => GCounter::from_bytes(payload)
            .map(|r| {
                c.merge(&r);
            })
            .is_some(),
        CrdtEntry::PNCounter(c) => PNCounter::from_bytes(payload)
            .map(|r| {
                c.merge(&r);
            })
            .is_some(),
        CrdtEntry::GSet(c) => GSet::<String>::from_bytes(payload)
            .map(|r| {
                c.merge(&r);
            })
            .is_some(),
        CrdtEntry::ORSet(c) => ORSet::<String>::from_bytes(payload)
            .map(|r| {
                c.merge(&r);
            })
            .is_some(),
        CrdtEntry::AWORSet(c) => AWORSet::<String>::from_bytes(payload)
            .map(|r| {
                c.merge(&r);
            })
            .is_some(),
        CrdtEntry::LWWRegister(c) => LWWRegister::<String>::from_bytes(payload)
            .map(|r| {
                c.merge(&r);
            })
            .is_some(),
        CrdtEntry::MVRegister(c) => MVRegister::<String>::from_bytes(payload)
            .map(|r| {
                c.merge(&r);
            })
            .is_some(),
        CrdtEntry::RGA(c) => RGA::<String>::from_bytes(payload)
            .map(|r| {
                c.merge(&r);
            })
            .is_some(),
    }
}

/// Materialized value of a CRDT field after a [`CrdtManager::apply_field_op`],
/// before it is interned into an actor heap as a `Value`. `Str` results
/// (register reads/writes) are interned by the caller into the actor's heap;
/// everything else is a plain tagged `Int`.
#[derive(Debug, Clone, PartialEq)]
pub enum CrdtValue {
    Int(i64),
    Str(String),
}
impl CrdtManager {
    pub fn new(node_id: u64) -> Self {
        CrdtManager {
            node_id,
            entries: HashMap::new(),
            ops_synced: 0,
            sync_base: HashMap::new(),
            field_map: HashMap::new(),
            field_reverse: HashMap::new(),
        }
    }

    pub fn create_gcounter(&mut self) -> (CrdtId, GCounter) {
        let id = CrdtId::new(self.node_id);
        let counter = GCounter::new(self.node_id);
        self.entries
            .insert(id, CrdtEntry::GCounter(counter.clone()));
        (id, counter)
    }

    pub fn create_pncounter(&mut self) -> (CrdtId, PNCounter) {
        let id = CrdtId::new(self.node_id);
        let counter = PNCounter::new(self.node_id);
        self.entries
            .insert(id, CrdtEntry::PNCounter(counter.clone()));
        (id, counter)
    }

    pub fn create_gset(&mut self) -> (CrdtId, GSet<String>) {
        let id = CrdtId::new(self.node_id);
        let set = GSet::new();
        self.entries.insert(id, CrdtEntry::GSet(set.clone()));
        (id, set)
    }

    pub fn create_orset(&mut self) -> (CrdtId, ORSet<String>) {
        let id = CrdtId::new(self.node_id);
        let set = ORSet::new(self.node_id);
        self.entries.insert(id, CrdtEntry::ORSet(set.clone()));
        (id, set)
    }

    pub fn create_aworset(&mut self) -> (CrdtId, AWORSet<String>) {
        let id = CrdtId::new(self.node_id);
        let set = AWORSet::new(self.node_id);
        self.entries.insert(id, CrdtEntry::AWORSet(set.clone()));
        (id, set)
    }

    pub fn create_lwwregister(&mut self, initial: String) -> (CrdtId, LWWRegister<String>) {
        let id = CrdtId::new(self.node_id);
        let reg = LWWRegister::new(self.node_id, initial);
        self.entries.insert(id, CrdtEntry::LWWRegister(reg.clone()));
        (id, reg)
    }

    pub fn create_mvregister(&mut self) -> (CrdtId, MVRegister<String>) {
        let id = CrdtId::new(self.node_id);
        let reg = MVRegister::new(self.node_id);
        self.entries.insert(id, CrdtEntry::MVRegister(reg.clone()));
        (id, reg)
    }

    pub fn create_rga(&mut self) -> (CrdtId, RGA<String>) {
        let id = CrdtId::new(self.node_id);
        let rga = RGA::new(self.node_id);
        self.entries.insert(id, CrdtEntry::RGA(rga.clone()));
        (id, rga)
    }

    /// Type-safe generic accessor for any CRDT entry managed by the store.
    ///
    /// Returns `Some(&mut T)` when an entry with `id` exists and its variant
    /// matches `T`, or `None` for unknown ids / type mismatches.
    pub fn entry_mut<T: CrdtEntryInner>(&mut self, id: CrdtId) -> Option<&mut T> {
        self.entries.get_mut(&id).and_then(T::try_from_entry)
    }
    pub fn get_gcounter_mut(&mut self, id: CrdtId) -> Option<&mut GCounter> {
        match self.entries.get_mut(&id) {
            Some(CrdtEntry::GCounter(c)) => Some(c),
            _ => None,
        }
    }
    pub fn get_pncounter_mut(&mut self, id: CrdtId) -> Option<&mut PNCounter> {
        match self.entries.get_mut(&id) {
            Some(CrdtEntry::PNCounter(c)) => Some(c),
            _ => None,
        }
    }
    pub fn get_gset_mut(&mut self, id: CrdtId) -> Option<&mut GSet<String>> {
        match self.entries.get_mut(&id) {
            Some(CrdtEntry::GSet(c)) => Some(c),
            _ => None,
        }
    }
    pub fn get_orset_mut(&mut self, id: CrdtId) -> Option<&mut ORSet<String>> {
        match self.entries.get_mut(&id) {
            Some(CrdtEntry::ORSet(c)) => Some(c),
            _ => None,
        }
    }
    pub fn get_aworset_mut(&mut self, id: CrdtId) -> Option<&mut AWORSet<String>> {
        match self.entries.get_mut(&id) {
            Some(CrdtEntry::AWORSet(c)) => Some(c),
            _ => None,
        }
    }
    pub fn get_lwwregister_mut(&mut self, id: CrdtId) -> Option<&mut LWWRegister<String>> {
        match self.entries.get_mut(&id) {
            Some(CrdtEntry::LWWRegister(c)) => Some(c),
            _ => None,
        }
    }
    pub fn get_mvregister_mut(&mut self, id: CrdtId) -> Option<&mut MVRegister<String>> {
        match self.entries.get_mut(&id) {
            Some(CrdtEntry::MVRegister(c)) => Some(c),
            _ => None,
        }
    }
    pub fn get_rga_mut(&mut self, id: CrdtId) -> Option<&mut RGA<String>> {
        match self.entries.get_mut(&id) {
            Some(CrdtEntry::RGA(c)) => Some(c),
            _ => None,
        }
    }

    pub fn apply_op(&mut self, op: CrdtOp) {
        if let Some(entry) = self.entries.get_mut(&op.crdt_id) {
            // Guard against stale/misrouted ops whose declared type no longer
            // matches the local replica.
            if entry.crdt_type() != op.crdt_type {
                return;
            }
            if merge_payload(entry, &op.payload) {
                self.ops_synced += 1;
            }
        } else {
            let mut entry = match op.crdt_type {
                CrdtType::GCounter => GCounter::from_bytes(&op.payload).map(CrdtEntry::GCounter),
                CrdtType::PNCounter => PNCounter::from_bytes(&op.payload).map(CrdtEntry::PNCounter),
                CrdtType::GSet => GSet::<String>::from_bytes(&op.payload).map(CrdtEntry::GSet),
                CrdtType::ORSet => ORSet::<String>::from_bytes(&op.payload).map(CrdtEntry::ORSet),
                CrdtType::AWORSet => {
                    AWORSet::<String>::from_bytes(&op.payload).map(CrdtEntry::AWORSet)
                }
                CrdtType::LWWRegister => {
                    LWWRegister::<String>::from_bytes(&op.payload).map(CrdtEntry::LWWRegister)
                }
                CrdtType::MVRegister => {
                    MVRegister::<String>::from_bytes(&op.payload).map(CrdtEntry::MVRegister)
                }
                CrdtType::RGA => RGA::<String>::from_bytes(&op.payload).map(CrdtEntry::RGA),
            };
            if let Some(ref mut e) = entry {
                e.set_local_node_id(self.node_id);
                self.entries.insert(op.crdt_id, e.clone());
                self.ops_synced += 1;
            }
        }
    }

    pub fn generate_sync_ops(&mut self) -> Vec<CrdtOp> {
        self.entries
            .iter()
            .map(|(id, entry)| CrdtOp {
                crdt_id: *id,
                crdt_type: entry.crdt_type(),
                payload: entry.payload_bytes(),
            })
            .collect()
    }

    /// Generate delta-state sync ops for all entries.
    ///
    /// Entries without a recorded sync base (never synced before — e.g.
    /// freshly created or learned during join) ship as full-state ops; all
    /// others ship only the changes since the last call. Unchanged entries
    /// produce no op at all. The current state becomes the new base for the
    /// next round.
    ///
    /// Convergence is identical to shipping full states: for a peer that
    /// holds the base, merging the delta produces exactly the state that
    /// merging the full entry would.
    ///
    /// **Delivery assumption:** the base advances when the ops are
    /// *generated*, so a delta lost in transit is not re-sent. Periodic
    /// full-state syncs ([`generate_sync_ops`](CrdtManager::generate_sync_ops))
    /// remain the repair mechanism after message loss.
    pub fn generate_delta_sync_ops(&mut self) -> Vec<CrdtDeltaOp> {
        let mut ops = Vec::new();
        for (id, entry) in &self.entries {
            match self.sync_base.get(id) {
                None => {
                    // Entry never synced: ship full state and record the base.
                    self.sync_base.insert(*id, entry.clone());
                    ops.push(CrdtDeltaOp {
                        op: CrdtOp {
                            crdt_id: *id,
                            crdt_type: entry.crdt_type(),
                            payload: entry.payload_bytes(),
                        },
                        is_delta: false,
                    });
                }
                Some(base) => {
                    if let Some(delta) = entry.delta_since(base) {
                        // Entry changed since last sync: ship delta and advance base.
                        self.sync_base.insert(*id, entry.clone());
                        ops.push(CrdtDeltaOp {
                            op: CrdtOp {
                                crdt_id: *id,
                                crdt_type: delta.crdt_type(),
                                payload: delta.payload_bytes(),
                            },
                            is_delta: true,
                        });
                    }
                    // Unchanged: no op, no base update — avoids wasteful clone.
                }
            }
        }
        ops
    }

    /// Generate op-based sync ops for all entries.
    ///
    /// Mirrors [`generate_delta_sync_ops`](CrdtManager::generate_delta_sync_ops)
    /// but returns individual [`CrdtOp`]s rather than delta-tagged ops —
    /// each op is shipped as its own `Packet::CrdtOp` for the lowest-bandwidth
    /// replication path. The sync base advances at generation, and the
    /// periodic full-state syncs remain the repair mechanism after message
    /// loss.
    pub fn generate_op_syncs(&mut self) -> Vec<CrdtOp> {
        let mut ops = Vec::new();
        for (id, entry) in &self.entries {
            match self.sync_base.get(id) {
                None => {
                    // Entry never synced: ship full state and record the base.
                    self.sync_base.insert(*id, entry.clone());
                    ops.push(CrdtOp {
                        crdt_id: *id,
                        crdt_type: entry.crdt_type(),
                        payload: entry.payload_bytes(),
                    });
                }
                Some(base) => {
                    if let Some(delta) = entry.delta_since(base) {
                        // Entry changed since last sync: ship delta and advance base.
                        self.sync_base.insert(*id, entry.clone());
                        ops.push(CrdtOp {
                            crdt_id: *id,
                            crdt_type: delta.crdt_type(),
                            payload: delta.payload_bytes(),
                        });
                    }
                    // Unchanged: no op.
                }
            }
        }
        ops
    }

    /// Serialize all entries into a snapshot suitable for persistence.
    pub fn snapshot(&self) -> HashMap<CrdtId, (CrdtType, Vec<u8>)> {
        self.entries
            .iter()
            .map(|(id, entry)| (*id, (entry.crdt_type(), entry.payload_bytes())))
            .collect()
    }

    /// Restore CRDT state from a previously saved snapshot.
    pub fn restore(&mut self, snapshot: HashMap<CrdtId, (CrdtType, Vec<u8>)>) {
        for (id, (crdt_type, bytes)) in snapshot {
            let mut entry: Option<CrdtEntry> = match crdt_type {
                CrdtType::GCounter => GCounter::from_bytes(&bytes).map(CrdtEntry::GCounter),
                CrdtType::PNCounter => PNCounter::from_bytes(&bytes).map(CrdtEntry::PNCounter),
                CrdtType::GSet => GSet::<String>::from_bytes(&bytes).map(CrdtEntry::GSet),
                CrdtType::ORSet => ORSet::<String>::from_bytes(&bytes).map(CrdtEntry::ORSet),
                CrdtType::AWORSet => AWORSet::<String>::from_bytes(&bytes).map(CrdtEntry::AWORSet),
                CrdtType::LWWRegister => {
                    LWWRegister::<String>::from_bytes(&bytes).map(CrdtEntry::LWWRegister)
                }
                CrdtType::MVRegister => {
                    MVRegister::<String>::from_bytes(&bytes).map(CrdtEntry::MVRegister)
                }
                CrdtType::RGA => RGA::<String>::from_bytes(&bytes).map(CrdtEntry::RGA),
            };
            if let Some(ref mut e) = entry {
                e.set_local_node_id(self.node_id);
                self.entries.insert(id, e.clone());
            }
        }
    }

    /// Apply a delta-tagged sync op received from a peer.
    ///
    /// Full-state ops behave exactly like [`apply_op`](CrdtManager::apply_op)
    /// (including creating the entry on first sight). Delta ops only merge
    /// into an entry this manager already has: a delta is meaningless
    /// without the base it was computed against, so unknown ids are ignored
    /// — the entry will arrive via a full-state op (the join fallback).
    pub fn apply_delta_op(&mut self, delta_op: CrdtDeltaOp) {
        if !delta_op.is_delta {
            self.apply_op(delta_op.op);
            return;
        }
        let op = delta_op.op;
        if let Some(entry) = self.entries.get_mut(&op.crdt_id) {
            // Same staleness guard as apply_op.
            if entry.crdt_type() != op.crdt_type {
                return;
            }
            if merge_payload(entry, &op.payload) {
                self.ops_synced += 1;
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn ops_synced(&self) -> u64 {
        self.ops_synced
    }

    /// Register a CRDT-backed state field for an actor.
    ///
    /// Creates a CRDT entry of the given type initialized from `initial_value`,
    /// and records the mapping so merges can be pushed back to the actor.
    pub fn register_actor_field(
        &mut self,
        actor_id: u64,
        field_name: &str,
        crdt_type: CrdtType,
        initial_value: crate::vm::Value,
    ) {
        let id = CrdtId::new(self.node_id);
        let initial_i64 = initial_value.as_int().unwrap_or(0);
        let entry = match crdt_type {
            CrdtType::GCounter => {
                let mut c = GCounter::new(self.node_id);
                for _ in 0..initial_i64 {
                    c.increment();
                }
                CrdtEntry::GCounter(c)
            }
            CrdtType::PNCounter => {
                let mut c = PNCounter::new(self.node_id);
                for _ in 0..initial_i64 {
                    c.increment();
                }
                CrdtEntry::PNCounter(c)
            }
            CrdtType::GSet => CrdtEntry::GSet(GSet::new()),
            CrdtType::ORSet => CrdtEntry::ORSet(ORSet::new(self.node_id)),
            CrdtType::AWORSet => CrdtEntry::AWORSet(AWORSet::new(self.node_id)),
            CrdtType::LWWRegister => {
                CrdtEntry::LWWRegister(LWWRegister::new(self.node_id, String::new()))
            }
            CrdtType::MVRegister => CrdtEntry::MVRegister(MVRegister::new(self.node_id)),
            CrdtType::RGA => CrdtEntry::RGA(RGA::new(self.node_id)),
        };
        let key = (actor_id, field_name.to_string());
        self.entries.insert(id, entry);
        self.field_map.insert(key.clone(), id);
        self.field_reverse.insert(id, key);
    }

    /// Get the CRDT ID for a specific actor field, if registered.
    pub fn get_field_id(&self, actor_id: u64, field_name: &str) -> Option<CrdtId> {
        self.field_map
            .get(&(actor_id, field_name.to_string()))
            .copied()
    }

    /// Get a mutable reference to a CRDT entry by actor ID and field name.
    pub fn get_field_mut<T: CrdtEntryInner>(
        &mut self,
        actor_id: u64,
        field_name: &str,
    ) -> Option<&mut T> {
        self.get_field_id(actor_id, field_name)
            .and_then(|id| self.entries.get_mut(&id))
            .and_then(|e| T::try_from_entry(e))
    }

    /// Apply a `Crdt.*` effect operation to a registered actor field.
    ///
    /// Returns `Some(value)` carrying the field's materialized value after a
    /// mutation (or its current value for `read`), and `None` when the field
    /// is unknown, the op is not recognized, or the op is not in the field's
    /// CRDT type's operation set (e.g. `decrement` on a `gcounter`). The
    /// per-type operation sets are:
    ///
    /// | CrdtType       | operations                                   |
    /// |----------------|----------------------------------------------|
    /// | `gcounter`     | `increment`, `read`                          |
    /// | `pncounter`    | `increment`, `decrement`, `read`             |
    /// | `gset`         | `add`, `read`                                |
    /// | `orset`        | `add`, `remove`, `read`                      |
    /// | `aworset`      | `add`, `remove`, `read`                      |
    /// | `lwwregister`  | `set`, `read`                                |
    /// | `mvregister`   | `set`, `read`                                |
    /// | `rga`          | `read` (insert/delete not yet surfaced)      |
    pub fn apply_field_op(
        &mut self,
        actor_id: u64,
        field_name: &str,
        op: &str,
        arg: Option<&str>,
    ) -> Option<CrdtValue> {
        let id = self.get_field_id(actor_id, field_name)?;
        match op {
            "increment" => match self.entries.get_mut(&id)? {
                CrdtEntry::GCounter(c) => {
                    c.increment();
                    Some(CrdtValue::Int(c.value() as i64))
                }
                CrdtEntry::PNCounter(c) => {
                    c.increment();
                    Some(CrdtValue::Int(c.value() as i64))
                }
                _ => None,
            },
            "decrement" => match self.entries.get_mut(&id)? {
                CrdtEntry::PNCounter(c) => {
                    c.decrement();
                    Some(CrdtValue::Int(c.value() as i64))
                }
                _ => None,
            },
            "add" => {
                let item = arg?.to_string();
                match self.entries.get_mut(&id)? {
                    CrdtEntry::GSet(s) => {
                        s.insert(item);
                        Some(CrdtValue::Int(s.len() as i64))
                    }
                    CrdtEntry::ORSet(s) => {
                        s.add(item);
                        Some(CrdtValue::Int(s.len() as i64))
                    }
                    CrdtEntry::AWORSet(s) => {
                        s.add(item);
                        Some(CrdtValue::Int(s.len() as i64))
                    }
                    _ => None,
                }
            }
            "remove" => {
                let item = arg?.to_string();
                match self.entries.get_mut(&id)? {
                    CrdtEntry::ORSet(s) => {
                        s.remove(&item);
                        Some(CrdtValue::Int(s.len() as i64))
                    }
                    CrdtEntry::AWORSet(s) => {
                        s.remove(&item);
                        Some(CrdtValue::Int(s.len() as i64))
                    }
                    _ => None,
                }
            }
            "set" => {
                let value = arg?.to_string();
                match self.entries.get_mut(&id)? {
                    CrdtEntry::LWWRegister(r) => {
                        r.write(value.clone());
                        Some(CrdtValue::Str(value))
                    }
                    CrdtEntry::MVRegister(r) => {
                        r.write(value);
                        Some(CrdtValue::Int(r.read().len() as i64))
                    }
                    _ => None,
                }
            }
            "read" => match self.entries.get(&id)? {
                CrdtEntry::GCounter(c) => Some(CrdtValue::Int(c.value() as i64)),
                CrdtEntry::PNCounter(c) => Some(CrdtValue::Int(c.value() as i64)),
                CrdtEntry::GSet(s) => Some(CrdtValue::Int(s.len() as i64)),
                CrdtEntry::ORSet(s) => Some(CrdtValue::Int(s.len() as i64)),
                CrdtEntry::AWORSet(s) => Some(CrdtValue::Int(s.len() as i64)),
                CrdtEntry::LWWRegister(r) => Some(CrdtValue::Str(r.read().clone())),
                CrdtEntry::MVRegister(r) => Some(CrdtValue::Int(r.read().len() as i64)),
                CrdtEntry::RGA(r) => Some(CrdtValue::Int(r.len() as i64)),
            },
            _ => None,
        }
    }

    /// Garbage-collect tombstones that are causally stable.
    ///
    /// A tombstone (ORSet/AWORSet removal, RGA deletion) is dropped once
    /// every healthy replica — plus the local node — has observed it, as
    /// established by the per-CRDT [`MatrixClock`](crate::runtime::crdt::MatrixClock)
    /// embedded in each entry. `healthy` lists the peer node ids; the local
    /// replica is always considered. A healthy peer that has not (yet)
    /// acknowledged an entry's removals blocks GC, so no element can be
    /// resurrected by a replica that never observed the removal. Rows for
    /// departed peers are pruned so the clock does not grow unboundedly as
    /// nodes churn.
    ///
    /// With no peers (`healthy` empty) the watermark collapses to the local
    /// replica's own observation: everything it holds is trivially stable, so
    /// tombstones are reclaimed for standalone use too.
    pub fn gc_stable_tombstones(&mut self, healthy: &[u64]) {
        let mut healthy = healthy.to_vec();
        if !healthy.contains(&self.node_id) {
            healthy.push(self.node_id);
        }
        for entry in self.entries.values_mut() {
            match entry {
                CrdtEntry::ORSet(s) => {
                    s.matrix.prune(&healthy);
                    let wm = s.matrix.watermark(&healthy);
                    s.gc_tombstones(&wm);
                }
                CrdtEntry::AWORSet(s) => {
                    s.matrix.prune(&healthy);
                    let wm = s.matrix.watermark(&healthy);
                    s.gc_tombstones(&wm);
                }
                CrdtEntry::RGA(s) => {
                    s.matrix.prune(&healthy);
                    let wm = s.matrix.watermark(&healthy);
                    s.gc_tombstones(&wm);
                }
                _ => {}
            }
        }
    }

    /// Push merged CRDT values back to actor state fields.
    ///
    /// After a sync round, call this to update actor state with the
    /// latest merged CRDT values. Only fields whose CRDT entry has
    /// changed (differs from the sync base) are updated.
    pub fn push_to_actors(&self, runtime: &mut crate::runtime::Runtime) {
        for (id, entry) in &self.entries {
            if let Some(&(actor_id, ref field_name)) = self.field_reverse.get(id) {
                let value = match entry {
                    CrdtEntry::GCounter(c) => crate::vm::Value::int(c.value() as i64),
                    CrdtEntry::PNCounter(c) => crate::vm::Value::int(c.value() as i64),
                    _ => crate::vm::Value::int(0), // Other types: placeholder
                };
                if let Some(actor) = runtime.actors.get_mut(&actor_id) {
                    actor.set_state_field(field_name.clone(), value);
                }
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply every generated sync op from `source` to `target`.
    fn sync_all(source: &mut CrdtManager, target: &mut CrdtManager) {
        let ops = source.generate_sync_ops();
        for op in ops {
            target.apply_op(op);
        }
    }

    // -----------------------------------------------------------------------
    // Convergence happy paths
    // -----------------------------------------------------------------------

    #[test]
    fn test_gcounter_convergence() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, mut counter) = a.create_gcounter();
            counter.increment_by(3);
            a.entries.insert(id, CrdtEntry::GCounter(counter));
            id
        };

        // B learns the CRDT from A's sync ops.
        sync_all(&mut a, &mut b);
        assert_eq!(b.len(), 1);

        // Divergent updates.
        a.get_gcounter_mut(id).unwrap().increment_by(2);
        b.get_gcounter_mut(id).unwrap().increment_by(5);

        // Exchange ops both ways.
        sync_all(&mut a, &mut b);
        sync_all(&mut b, &mut a);

        assert_eq!(
            a.get_gcounter_mut(id).unwrap().value(),
            b.get_gcounter_mut(id).unwrap().value()
        );
        assert_eq!(a.get_gcounter_mut(id).unwrap().value(), 10);
    }

    #[test]
    fn test_pncounter_convergence() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, mut counter) = a.create_pncounter();
            counter.increment_by(4);
            a.entries.insert(id, CrdtEntry::PNCounter(counter));
            id
        };

        sync_all(&mut a, &mut b);

        a.get_pncounter_mut(id).unwrap().increment_by(3);
        b.get_pncounter_mut(id).unwrap().decrement_by(2);

        sync_all(&mut a, &mut b);
        sync_all(&mut b, &mut a);

        assert_eq!(
            a.get_pncounter_mut(id).unwrap().value(),
            b.get_pncounter_mut(id).unwrap().value()
        );
        assert_eq!(a.get_pncounter_mut(id).unwrap().value(), 5);
    }

    #[test]
    fn test_orset_convergence() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, mut set) = a.create_orset();
            set.add("apple".to_string());
            a.entries.insert(id, CrdtEntry::ORSet(set));
            id
        };

        sync_all(&mut a, &mut b);

        a.get_orset_mut(id).unwrap().add("banana".to_string());
        b.get_orset_mut(id).unwrap().add("cherry".to_string());

        sync_all(&mut a, &mut b);
        sync_all(&mut b, &mut a);

        let va = a.get_orset_mut(id).unwrap().value();
        let vb = b.get_orset_mut(id).unwrap().value();
        assert_eq!(va, vb);
        assert!(va.contains("apple"));
        assert!(va.contains("banana"));
        assert!(va.contains("cherry"));
    }

    #[test]
    fn test_lwwregister_convergence() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, reg) = a.create_lwwregister("initial".to_string());
            a.entries.insert(id, CrdtEntry::LWWRegister(reg));
            id
        };

        sync_all(&mut a, &mut b);

        a.get_lwwregister_mut(id)
            .unwrap()
            .write("A-wins".to_string());
        b.get_lwwregister_mut(id)
            .unwrap()
            .write("B-loses".to_string());

        sync_all(&mut a, &mut b);
        sync_all(&mut b, &mut a);

        let va = a.get_lwwregister_mut(id).unwrap().value();
        let vb = b.get_lwwregister_mut(id).unwrap().value();
        assert_eq!(va, vb);
        // One of the two writes wins deterministically by Lamport timestamp.
        assert!(va == "A-wins" || va == "B-loses");
    }

    #[test]
    fn test_rga_convergence() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, rga) = a.create_rga();
            a.entries.insert(id, CrdtEntry::RGA(rga));
            id
        };

        sync_all(&mut a, &mut b);

        a.get_rga_mut(id).unwrap().insert_at(0, "first".to_string());
        b.get_rga_mut(id)
            .unwrap()
            .insert_at(0, "second".to_string());

        sync_all(&mut a, &mut b);
        sync_all(&mut b, &mut a);

        let va = a.get_rga_mut(id).unwrap().value();
        let vb = b.get_rga_mut(id).unwrap().value();
        assert_eq!(va, vb);
        assert_eq!(va.len(), 2);
        assert!(va.contains(&"first".to_string()));
        assert!(va.contains(&"second".to_string()));
    }

    // -----------------------------------------------------------------------
    // Network fault tolerance
    // -----------------------------------------------------------------------

    #[test]
    fn test_sync_ops_are_idempotent() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, mut set) = a.create_orset();
            set.add("x".to_string());
            a.entries.insert(id, CrdtEntry::ORSet(set));
            id
        };

        let ops = a.generate_sync_ops();
        for op in ops.clone() {
            b.apply_op(op);
        }
        for op in ops.clone() {
            b.apply_op(op);
        }
        for op in ops {
            b.apply_op(op);
        }

        assert_eq!(b.get_orset_mut(id).unwrap().value().len(), 1);
    }

    #[test]
    fn test_packet_loss_and_late_delivery_still_converge() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, mut counter) = a.create_gcounter();
            counter.increment_by(7);
            a.entries.insert(id, CrdtEntry::GCounter(counter));
            id
        };

        // First sync is partially dropped: only the first op (if any) is delivered.
        let ops = a.generate_sync_ops();
        if let Some(first) = ops.first() {
            b.apply_op(first.clone());
        }

        // More updates on A before the next sync.
        a.get_gcounter_mut(id).unwrap().increment_by(3);

        // Eventually all pending ops are delivered.
        sync_all(&mut a, &mut b);
        sync_all(&mut b, &mut a);

        assert_eq!(
            a.get_gcounter_mut(id).unwrap().value(),
            b.get_gcounter_mut(id).unwrap().value()
        );
        assert_eq!(a.get_gcounter_mut(id).unwrap().value(), 10);
    }

    #[test]
    fn test_partition_healing_convergence() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, mut set) = a.create_orset();
            set.add("base".to_string());
            a.entries.insert(id, CrdtEntry::ORSet(set));
            id
        };

        // B learns the CRDT.
        sync_all(&mut a, &mut b);

        // Partition: both sides update independently.
        a.get_orset_mut(id).unwrap().add("left".to_string());
        b.get_orset_mut(id).unwrap().add("right".to_string());

        // Healing: exchange all buffered ops in both directions.
        let a_ops = a.generate_sync_ops();
        let b_ops = b.generate_sync_ops();
        for op in a_ops {
            b.apply_op(op);
        }
        for op in b_ops {
            a.apply_op(op);
        }

        let va = a.get_orset_mut(id).unwrap().value();
        let vb = b.get_orset_mut(id).unwrap().value();
        assert_eq!(va, vb);
        assert!(va.contains("left"));
        assert!(va.contains("right"));
    }

    // -----------------------------------------------------------------------
    // Tombstone garbage collection (causal-stability watermark)
    // -----------------------------------------------------------------------

    /// A tombstone is reclaimed only once every considered replica has
    /// observed the removal. A peer that never synced back (partitioned)
    /// blocks GC, because dropping the tombstone would let it resurrect the
    /// element on rejoin.
    #[test]
    fn test_tombstone_gc_requires_peer_observation() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);
        let mut c = CrdtManager::new(3);

        // Node 1 adds then removes an element -> one tombstone. Mutations go
        // through `get_orset_mut` so the manager's stored copy reflects them.
        let id = {
            let (id, _) = a.create_orset();
            a.get_orset_mut(id).unwrap().add("x".to_string());
            a.get_orset_mut(id).unwrap().remove(&"x".to_string());
            id
        };
        // B and C learn the CRDT *and* confirm they observed the removal by
        // syncing their state back to A.
        sync_all(&mut a, &mut b);
        sync_all(&mut b, &mut a);
        sync_all(&mut a, &mut c);
        sync_all(&mut c, &mut a);

        // Both healthy peers observed the removal -> causally stable.
        a.gc_stable_tombstones(&[2, 3]);
        match &a.entries[&id] {
            CrdtEntry::ORSet(s) => assert!(
                s.removed.is_empty(),
                "tombstone must be reclaimed once every peer observed it"
            ),
            _ => panic!("expected ORSet"),
        }

        // A second tombstone on the same manager, with an un-observing peer
        // (node 4) that never syncs back: GC must retain it.
        let id2 = {
            let (id, _) = a.create_orset();
            a.get_orset_mut(id).unwrap().add("y".to_string());
            a.get_orset_mut(id).unwrap().remove(&"y".to_string());
            id
        };
        a.gc_stable_tombstones(&[2, 3, 4]);
        match &a.entries[&id2] {
            CrdtEntry::ORSet(s) => assert_eq!(
                s.removed.len(),
                1,
                "un-observing peer must block tombstone GC"
            ),
            _ => panic!("expected ORSet"),
        }
    }

    /// Distribution-disabled path: with no peers, the watermark collapses to
    /// the local replica's own observation, so a tombstone it created is
    /// immediately causally stable and reclaimed.
    #[test]
    fn test_tombstone_gc_standalone_reclaims_locally() {
        let mut a = CrdtManager::new(1);
        let id = {
            let (id, _) = a.create_orset();
            a.get_orset_mut(id).unwrap().add("x".to_string());
            a.get_orset_mut(id).unwrap().remove(&"x".to_string());
            id
        };
        assert_eq!(
            match &a.entries[&id] {
                CrdtEntry::ORSet(s) => s.removed.len(),
                _ => panic!("expected ORSet"),
            },
            1,
            "sanity: a tombstone exists"
        );
        // No peers: the local replica is the only one that must have observed
        // the removal (it created it), so GC reclaims it.
        a.gc_stable_tombstones(&[]);
        match &a.entries[&id] {
            CrdtEntry::ORSet(s) => assert!(
                s.removed.is_empty(),
                "standalone tombstone must be reclaimed (no peers to miss it)"
            ),
            _ => panic!("expected ORSet"),
        }
    }

    // -----------------------------------------------------------------------
    // Invalid / corrupted ops
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_op_rejects_mismatched_type() {
        let mut a = CrdtManager::new(1);
        let id = {
            let (id, mut counter) = a.create_gcounter();
            counter.increment_by(5);
            a.entries.insert(id, CrdtEntry::GCounter(counter));
            id
        };

        let synced_before = a.ops_synced();

        // Feed a valid ORSet payload with its real type to a GCounter entry.
        let mut orset_manager = CrdtManager::new(99);
        let (_, mut set) = orset_manager.create_orset();
        set.add("sneaky".to_string());
        let bad_op = CrdtOp {
            crdt_id: id,
            crdt_type: CrdtType::ORSet,
            payload: set.to_bytes(),
        };

        a.apply_op(bad_op);
        // The existing GCounter entry should be unchanged.
        assert_eq!(a.get_gcounter_mut(id).unwrap().value(), 5);
        // No successful sync should have been recorded.
        assert_eq!(a.ops_synced(), synced_before);
    }

    #[test]
    fn test_apply_op_rejects_corrupted_payload() {
        let mut a = CrdtManager::new(1);
        let id = {
            let (id, mut counter) = a.create_gcounter();
            counter.increment_by(5);
            a.entries.insert(id, CrdtEntry::GCounter(counter));
            id
        };

        let bad_op = CrdtOp {
            crdt_id: id,
            crdt_type: CrdtType::GCounter,
            payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };

        a.apply_op(bad_op);
        assert_eq!(a.get_gcounter_mut(id).unwrap().value(), 5);
    }

    #[test]
    fn test_crdt_op_round_trip() {
        let mut a = CrdtManager::new(1);
        let (id, mut counter) = a.create_gcounter();
        counter.increment_by(42);
        a.entries.insert(id, CrdtEntry::GCounter(counter));

        let ops = a.generate_sync_ops();
        assert_eq!(ops.len(), 1);
        let bytes = ops[0].to_bytes();
        let round_tripped = CrdtOp::from_bytes(&bytes).expect("CrdtOp round-trips");
        assert_eq!(round_tripped.crdt_id, id);
        assert_eq!(round_tripped.crdt_type, CrdtType::GCounter);

        let mut b = CrdtManager::new(2);
        b.apply_op(round_tripped);
        assert_eq!(b.get_gcounter_mut(id).unwrap().value(), 42);
    }

    // -----------------------------------------------------------------------
    // Delta-state replication
    // -----------------------------------------------------------------------

    /// Apply every generated delta sync op from `source` to `target`
    /// (the mock-network counterpart of `sync_all`).
    fn sync_all_delta(source: &mut CrdtManager, target: &mut CrdtManager) {
        let ops = source.generate_delta_sync_ops();
        for op in ops {
            target.apply_delta_op(op);
        }
    }

    #[test]
    fn test_delta_op_round_trip() {
        let mut a = CrdtManager::new(1);
        let (id, mut counter) = a.create_gcounter();
        counter.increment_by(42);
        a.entries.insert(id, CrdtEntry::GCounter(counter));

        let ops = a.generate_delta_sync_ops();
        assert_eq!(ops.len(), 1);
        let bytes = ops[0].to_bytes();
        let round_tripped = CrdtDeltaOp::from_bytes(&bytes).expect("CrdtDeltaOp round-trips");
        assert_eq!(round_tripped, ops[0]);
    }

    #[test]
    fn test_first_delta_sync_ships_full_state() {
        // Join fallback: an entry that was never synced must ship whole.
        let mut a = CrdtManager::new(1);
        let (id, mut counter) = a.create_gcounter();
        counter.increment_by(7);
        a.entries.insert(id, CrdtEntry::GCounter(counter));

        let ops = a.generate_delta_sync_ops();
        assert_eq!(ops.len(), 1);
        assert!(!ops[0].is_delta, "first sync must be a full-state op");

        let mut b = CrdtManager::new(2);
        b.apply_delta_op(ops[0].clone());
        assert_eq!(b.get_gcounter_mut(id).unwrap().value(), 7);
    }

    #[test]
    fn test_second_delta_sync_ships_only_changes() {
        let mut a = CrdtManager::new(1);
        let (id, mut counter) = a.create_gcounter();
        counter.increment_by(7);
        // Give the counter a second per-node entry (as if learned from a
        // peer) so a one-entry delta is strictly smaller than full state.
        let mut foreign = GCounter::new(2);
        foreign.increment_by(100);
        counter.merge(&foreign);
        a.entries.insert(id, CrdtEntry::GCounter(counter));

        let full = a.generate_delta_sync_ops();
        a.get_gcounter_mut(id).unwrap().increment_by(3);
        let delta = a.generate_delta_sync_ops();

        assert_eq!(delta.len(), 1);
        assert!(delta[0].is_delta, "second sync must be a delta op");
        // The delta carries only the changed entry; the full state carries
        // both per-node entries.
        assert!(delta[0].op.payload.len() < full[0].op.payload.len());
        // An unchanged entry produces no op at all.
        assert!(a.generate_delta_sync_ops().is_empty());
    }

    #[test]
    fn test_delta_ignored_for_unknown_entry() {
        // A delta without its base is meaningless: it must not create the
        // entry (that is what the full-state join fallback is for).
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);
        let (id, mut counter) = a.create_gcounter();
        counter.increment_by(7);
        a.entries.insert(id, CrdtEntry::GCounter(counter));

        // Establish A's base, then produce a genuine delta B has no base for.
        let _ = a.generate_delta_sync_ops();
        a.get_gcounter_mut(id).unwrap().increment_by(1);
        let delta = a.generate_delta_sync_ops();
        assert!(delta[0].is_delta);

        let synced_before = b.ops_synced();
        b.apply_delta_op(delta[0].clone());
        assert!(b.is_empty(), "delta must not create an unknown entry");
        assert_eq!(b.ops_synced(), synced_before);
    }

    #[test]
    fn test_gcounter_delta_convergence() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, mut counter) = a.create_gcounter();
            counter.increment_by(3);
            a.entries.insert(id, CrdtEntry::GCounter(counter));
            id
        };

        // B joins: first delta sync ships the full state.
        sync_all_delta(&mut a, &mut b);
        assert_eq!(b.len(), 1);

        // Divergent updates, then delta exchange in both directions.
        a.get_gcounter_mut(id).unwrap().increment_by(2);
        b.get_gcounter_mut(id).unwrap().increment_by(5);

        sync_all_delta(&mut a, &mut b);
        sync_all_delta(&mut b, &mut a);

        assert_eq!(
            a.get_gcounter_mut(id).unwrap().value(),
            b.get_gcounter_mut(id).unwrap().value()
        );
        assert_eq!(a.get_gcounter_mut(id).unwrap().value(), 10);
    }

    #[test]
    fn test_orset_delta_convergence() {
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, mut set) = a.create_orset();
            set.add("apple".to_string());
            a.entries.insert(id, CrdtEntry::ORSet(set));
            id
        };

        sync_all_delta(&mut a, &mut b);

        a.get_orset_mut(id).unwrap().add("banana".to_string());
        b.get_orset_mut(id).unwrap().add("cherry".to_string());

        sync_all_delta(&mut a, &mut b);
        sync_all_delta(&mut b, &mut a);

        let va = a.get_orset_mut(id).unwrap().value();
        let vb = b.get_orset_mut(id).unwrap().value();
        assert_eq!(va, vb);
        assert!(va.contains("apple"));
        assert!(va.contains("banana"));
        assert!(va.contains("cherry"));
    }

    #[test]
    fn test_delta_sync_matches_full_sync_result() {
        // Same workload converged two ways — full-state ops vs delta ops —
        // must yield identical resulting state.
        let build = |m: &mut CrdtManager| -> CrdtId {
            let (id, mut counter) = m.create_pncounter();
            counter.increment_by(4);
            m.entries.insert(id, CrdtEntry::PNCounter(counter));
            id
        };

        // Full-state path.
        let mut a_full = CrdtManager::new(1);
        let mut b_full = CrdtManager::new(2);
        let id_full = build(&mut a_full);
        sync_all(&mut a_full, &mut b_full);
        a_full.get_pncounter_mut(id_full).unwrap().increment_by(3);
        b_full.get_pncounter_mut(id_full).unwrap().decrement_by(2);
        sync_all(&mut a_full, &mut b_full);
        sync_all(&mut b_full, &mut a_full);

        // Delta path.
        let mut a_delta = CrdtManager::new(1);
        let mut b_delta = CrdtManager::new(2);
        let id_delta = build(&mut a_delta);
        sync_all_delta(&mut a_delta, &mut b_delta);
        a_delta.get_pncounter_mut(id_delta).unwrap().increment_by(3);
        b_delta.get_pncounter_mut(id_delta).unwrap().decrement_by(2);
        sync_all_delta(&mut a_delta, &mut b_delta);
        sync_all_delta(&mut b_delta, &mut a_delta);

        assert_eq!(
            a_full.get_pncounter_mut(id_full).unwrap(),
            a_delta.get_pncounter_mut(id_delta).unwrap()
        );
        assert_eq!(
            b_full.get_pncounter_mut(id_full).unwrap(),
            b_delta.get_pncounter_mut(id_delta).unwrap()
        );
    }

    #[test]
    fn test_full_sync_repairs_after_delta_loss() {
        // A dropped delta is not re-sent (the base already advanced), but a
        // full-state sync repairs the divergence — the documented fallback.
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let id = {
            let (id, mut counter) = a.create_gcounter();
            counter.increment_by(3);
            a.entries.insert(id, CrdtEntry::GCounter(counter));
            id
        };
        sync_all_delta(&mut a, &mut b);

        // This update's delta is "lost": generated (advancing the base) but
        // never applied to B.
        a.get_gcounter_mut(id).unwrap().increment_by(5);
        let _lost = a.generate_delta_sync_ops();

        // Full-state fallback repairs B.
        sync_all(&mut a, &mut b);
        assert_eq!(b.get_gcounter_mut(id).unwrap().value(), 8);
    }

    // -----------------------------------------------------------------------
    // Node-scoped ids
    // -----------------------------------------------------------------------

    #[test]
    fn test_crdt_id_disjoint_across_nodes() {
        // Two managers that each mint several CRDTs must never produce the
        // same id, even though their local counters start at the same value.
        let mut a = CrdtManager::new(1);
        let mut b = CrdtManager::new(2);

        let mut ids_a = Vec::new();
        let mut ids_b = Vec::new();
        for _ in 0..4 {
            ids_a.push(a.create_gcounter().0);
            ids_b.push(b.create_gcounter().0);
        }

        for ia in &ids_a {
            for ib in &ids_b {
                assert_ne!(ia, ib, "ids from different nodes must not collide");
            }
        }

        // The node id is folded into the high 32 bits, so a counter reset on
        // another node still can't collide with this node's ids.
        assert!(
            ids_a.iter().all(|id| id.0 >> 32 == 1),
            "node 1 ids must carry node id in the high bits"
        );
        assert!(
            ids_b.iter().all(|id| id.0 >> 32 == 2),
            "node 2 ids must carry node id in the high bits"
        );
    }

    #[test]
    fn test_apply_field_op_counter_operation_sets() {
        let mut mgr = CrdtManager::new(1);
        mgr.register_actor_field(10, "count", CrdtType::GCounter, crate::vm::Value::int(0));

        assert_eq!(
            mgr.apply_field_op(10, "count", "increment", None),
            Some(CrdtValue::Int(1))
        );
        assert_eq!(
            mgr.apply_field_op(10, "count", "increment", None),
            Some(CrdtValue::Int(2))
        );
        assert_eq!(
            mgr.apply_field_op(10, "count", "read", None),
            Some(CrdtValue::Int(2))
        );
        // decrement/add/remove/set are outside the gcounter operation set.
        assert_eq!(mgr.apply_field_op(10, "count", "decrement", None), None);
        assert_eq!(mgr.apply_field_op(10, "count", "add", Some("x")), None);
        assert_eq!(mgr.apply_field_op(10, "count", "set", Some("x")), None);
        // Unknown field and unknown op.
        assert_eq!(mgr.apply_field_op(10, "missing", "read", None), None);
        assert_eq!(mgr.apply_field_op(10, "count", "bogus", None), None);
    }

    #[test]
    fn test_apply_field_op_pncounter_and_sets() {
        let mut mgr = CrdtManager::new(1);
        mgr.register_actor_field(1, "pn", CrdtType::PNCounter, crate::vm::Value::int(0));
        mgr.register_actor_field(2, "gs", CrdtType::GSet, crate::vm::Value::nil());
        mgr.register_actor_field(3, "ors", CrdtType::ORSet, crate::vm::Value::nil());

        assert_eq!(
            mgr.apply_field_op(1, "pn", "increment", None),
            Some(CrdtValue::Int(1))
        );
        assert_eq!(
            mgr.apply_field_op(1, "pn", "decrement", None),
            Some(CrdtValue::Int(0))
        );
        assert_eq!(
            mgr.apply_field_op(1, "pn", "decrement", None),
            Some(CrdtValue::Int(-1))
        );

        assert_eq!(
            mgr.apply_field_op(2, "gs", "add", Some("a")),
            Some(CrdtValue::Int(1))
        );
        assert_eq!(
            mgr.apply_field_op(2, "gs", "add", Some("b")),
            Some(CrdtValue::Int(2))
        );
        // remove is outside the gset operation set.
        assert_eq!(mgr.apply_field_op(2, "gs", "remove", Some("a")), None);

        assert_eq!(
            mgr.apply_field_op(3, "ors", "add", Some("a")),
            Some(CrdtValue::Int(1))
        );
        assert_eq!(
            mgr.apply_field_op(3, "ors", "add", Some("b")),
            Some(CrdtValue::Int(2))
        );
        assert_eq!(
            mgr.apply_field_op(3, "ors", "remove", Some("a")),
            Some(CrdtValue::Int(1))
        );
        // add without an item argument is rejected.
        assert_eq!(mgr.apply_field_op(3, "ors", "add", None), None);
    }

    #[test]
    fn test_apply_field_op_registers() {
        let mut mgr = CrdtManager::new(1);
        mgr.register_actor_field(1, "lww", CrdtType::LWWRegister, crate::vm::Value::nil());
        mgr.register_actor_field(2, "mv", CrdtType::MVRegister, crate::vm::Value::nil());

        assert_eq!(
            mgr.apply_field_op(1, "lww", "set", Some("hello")),
            Some(CrdtValue::Str("hello".to_string()))
        );
        assert_eq!(
            mgr.apply_field_op(1, "lww", "read", None),
            Some(CrdtValue::Str("hello".to_string()))
        );
        // increment is outside the lwwregister operation set.
        assert_eq!(mgr.apply_field_op(1, "lww", "increment", None), None);

        assert_eq!(
            mgr.apply_field_op(2, "mv", "set", Some("v")),
            Some(CrdtValue::Int(1))
        );
        assert_eq!(
            mgr.apply_field_op(2, "mv", "read", None),
            Some(CrdtValue::Int(1))
        );
        // decrement is outside the mvregister operation set.
        assert_eq!(mgr.apply_field_op(2, "mv", "decrement", None), None);
    }
}
