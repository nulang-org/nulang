//! Per-peer acknowledged CRDT synchronization frontiers.
//!
//! The legacy CRDT delta path advances a single `CrdtManager::sync_base` when
//! a batch is generated. That is sufficient for best-effort periodic repair,
//! but it cannot represent what each receiver has actually acknowledged.
//!
//! `PeerCrdtFrontiers` keeps that knowledge outside `CrdtManager`: batches are
//! always computed from a receiver's last acknowledged state, generation does
//! not advance the frontier, and only an acknowledgement for that receiver can
//! move it forward.

use std::collections::{BTreeMap, HashMap};

use crate::runtime::{CrdtDeltaOp, CrdtEntry, CrdtId, CrdtManager, CrdtOp};

const DEFAULT_MAX_PENDING_BATCHES_PER_PEER: usize = 32;

/// One receiver-specific CRDT synchronization batch.
#[derive(Debug, Clone, PartialEq)]
pub struct CrdtPeerSyncBatch {
    /// Monotonically increasing sender-local identity used by acknowledgements.
    pub batch_id: u64,
    /// Full-state or delta operations computed from this peer's acknowledged
    /// frontier.
    pub ops: Vec<CrdtDeltaOp>,
}

#[derive(Debug, Default)]
struct PeerFrontier {
    /// State the receiver has explicitly acknowledged.
    acknowledged: HashMap<CrdtId, CrdtEntry>,
    /// State represented by each emitted-but-unacknowledged batch.
    pending: BTreeMap<u64, HashMap<CrdtId, CrdtEntry>>,
    /// Highest batch acknowledgement accepted for this peer. Older ACKs are
    /// stale and must never move the frontier backwards.
    last_acked_batch: u64,
}

/// Tracks what each peer is proven to know about the local CRDT set.
///
/// This component intentionally does not own CRDT state. Callers provide a
/// `CrdtManager` when generating a batch, making the separation between local
/// truth and receiver knowledge explicit.
#[derive(Debug)]
pub struct PeerCrdtFrontiers {
    next_batch_id: u64,
    peers: HashMap<u64, PeerFrontier>,
    max_pending_batches_per_peer: usize,
}

impl Default for PeerCrdtFrontiers {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerCrdtFrontiers {
    /// Create frontiers with a bounded number of remembered in-flight batches
    /// per peer. Evicting an old pending batch is safe: an ACK for it is then
    /// ignored and the next generation still starts from the last acknowledged
    /// state.
    pub fn new() -> Self {
        Self::with_pending_limit(DEFAULT_MAX_PENDING_BATCHES_PER_PEER)
    }

    /// Create frontiers with an explicit in-flight batch bound per peer.
    pub fn with_pending_limit(max_pending_batches_per_peer: usize) -> Self {
        Self {
            next_batch_id: 1,
            peers: HashMap::new(),
            max_pending_batches_per_peer: max_pending_batches_per_peer.max(1),
        }
    }

    /// Generate the information `peer_id` still needs relative to its last
    /// acknowledged frontier.
    ///
    /// Crucially, generating a batch does **not** advance receiver knowledge.
    /// Until [`acknowledge`](Self::acknowledge) succeeds, a subsequent call is
    /// allowed to retransmit equivalent information.
    pub fn generate(
        &mut self,
        manager: &CrdtManager,
        peer_id: u64,
    ) -> Option<CrdtPeerSyncBatch> {
        let peer = self.peers.entry(peer_id).or_default();
        let mut ops = Vec::new();
        let mut represented_state = HashMap::new();

        // HashMap iteration order is deliberately not part of the wire
        // contract. Stable CRDT-id ordering makes repeated generations from
        // the same state byte-for-byte deterministic once packet framing is
        // wired to this component.
        let mut entries: Vec<_> = manager.entries.iter().collect();
        entries.sort_unstable_by_key(|(id, _)| id.0);

        for (id, entry) in entries {
            match peer.acknowledged.get(id) {
                None => {
                    ops.push(CrdtDeltaOp {
                        op: CrdtOp {
                            crdt_id: *id,
                            crdt_type: entry.crdt_type(),
                            payload: entry.payload_bytes(),
                        },
                        is_delta: false,
                    });
                    represented_state.insert(*id, entry.clone());
                }
                Some(base) => {
                    if let Some(delta) = entry.delta_since(base) {
                        ops.push(CrdtDeltaOp {
                            op: CrdtOp {
                                crdt_id: *id,
                                crdt_type: delta.crdt_type(),
                                payload: delta.payload_bytes(),
                            },
                            is_delta: true,
                        });
                        represented_state.insert(*id, entry.clone());
                    }
                }
            }
        }

        if ops.is_empty() {
            return None;
        }

        let batch_id = self.next_batch_id;
        self.next_batch_id = self
            .next_batch_id
            .checked_add(1)
            .expect("CRDT peer sync batch id exhausted");

        peer.pending.insert(batch_id, represented_state);
        while peer.pending.len() > self.max_pending_batches_per_peer {
            let Some(oldest) = peer.pending.keys().next().copied() else {
                break;
            };
            peer.pending.remove(&oldest);
        }

        Some(CrdtPeerSyncBatch { batch_id, ops })
    }

    /// Advance one peer's frontier to the state represented by `batch_id`.
    ///
    /// Returns `true` only when the batch is still pending and newer than the
    /// last accepted acknowledgement. Unknown, evicted, duplicate, or stale
    /// acknowledgements are ignored.
    pub fn acknowledge(&mut self, peer_id: u64, batch_id: u64) -> bool {
        let Some(peer) = self.peers.get_mut(&peer_id) else {
            return false;
        };
        if batch_id <= peer.last_acked_batch {
            peer.pending.remove(&batch_id);
            return false;
        }
        let Some(represented_state) = peer.pending.remove(&batch_id) else {
            return false;
        };

        for (id, state) in represented_state {
            peer.acknowledged.insert(id, state);
        }
        peer.last_acked_batch = batch_id;

        // Every older batch was computed from a frontier no newer than the one
        // just acknowledged. Its eventual ACK cannot add useful knowledge and
        // must not be allowed to roll state backwards.
        peer.pending.retain(|pending_id, _| *pending_id > batch_id);
        true
    }

    /// Forget all receiver knowledge for a departed/replaced peer. A future
    /// generation for the same id starts with a full-state join batch.
    pub fn forget_peer(&mut self, peer_id: u64) {
        self.peers.remove(&peer_id);
    }

    /// Number of entries this peer has explicitly acknowledged.
    pub fn acknowledged_entry_count(&self, peer_id: u64) -> usize {
        self.peers
            .get(&peer_id)
            .map(|peer| peer.acknowledged.len())
            .unwrap_or(0)
    }

    /// Number of emitted batches still eligible for acknowledgement.
    pub fn pending_batch_count(&self, peer_id: u64) -> usize {
        self.peers
            .get(&peer_id)
            .map(|peer| peer.pending.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_batches_are_bounded_without_advancing_acknowledged_state() {
        let mut manager = CrdtManager::new(1);
        let (id, mut counter) = manager.create_gcounter();
        counter.increment_by(1);
        manager.entries.insert(id, CrdtEntry::GCounter(counter));

        let mut frontiers = PeerCrdtFrontiers::with_pending_limit(2);
        let first = frontiers.generate(&manager, 2).unwrap();
        let second = frontiers.generate(&manager, 2).unwrap();
        let third = frontiers.generate(&manager, 2).unwrap();

        assert_eq!(frontiers.pending_batch_count(2), 2);
        assert_eq!(frontiers.acknowledged_entry_count(2), 0);
        assert!(!frontiers.acknowledge(2, first.batch_id));
        assert!(frontiers.acknowledge(2, second.batch_id));
        assert_eq!(frontiers.acknowledged_entry_count(2), 1);
        assert!(frontiers.acknowledge(2, third.batch_id));
        assert!(!frontiers.acknowledge(2, second.batch_id));
    }

    #[test]
    fn forgetting_peer_forces_full_state_again() {
        let mut manager = CrdtManager::new(1);
        let (id, mut counter) = manager.create_gcounter();
        counter.increment_by(1);
        manager.entries.insert(id, CrdtEntry::GCounter(counter));

        let mut frontiers = PeerCrdtFrontiers::new();
        let first = frontiers.generate(&manager, 9).unwrap();
        assert!(frontiers.acknowledge(9, first.batch_id));
        assert!(frontiers.generate(&manager, 9).is_none());

        frontiers.forget_peer(9);
        let rejoin = frontiers.generate(&manager, 9).unwrap();
        assert!(!rejoin.ops[0].is_delta);
    }

    #[test]
    fn operation_order_is_stable_by_crdt_id() {
        let mut manager = CrdtManager::new(1);
        let first = manager.create_gcounter().0;
        let second = manager.create_gcounter().0;

        let mut frontiers = PeerCrdtFrontiers::new();
        let batch = frontiers.generate(&manager, 2).unwrap();
        let ids: Vec<_> = batch.ops.iter().map(|op| op.op.crdt_id.0).collect();

        assert_eq!(ids, vec![first.0.min(second.0), first.0.max(second.0)]);
    }
}
