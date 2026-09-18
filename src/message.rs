//! Stable message identity, causal metadata, and bounded deduplication.
//!
//! Message identity is deliberately independent of packet sequence numbers,
//! actor ids, and tracing ids. A logical delivery keeps the same `MessageId`
//! across retries so receivers can deduplicate replayed work without confusing
//! a retry for a new message.

use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Stable 128-bit identity for one logical actor message.
///
/// `origin` identifies the runtime/node that allocated the id. `sequence` is
/// monotonically increasing within that origin. Distributed runtimes should
/// use their stable `NodeId` as the origin; standalone runtimes may use zero
/// when identity only needs to be unique within one process/runtime boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MessageId {
    pub origin: u64,
    pub sequence: u64,
}

impl MessageId {
    pub const fn new(origin: u64, sequence: u64) -> Self {
        Self { origin, sequence }
    }

    /// Canonical big-endian wire representation.
    pub fn to_bytes(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&self.origin.to_be_bytes());
        out[8..].copy_from_slice(&self.sequence.to_be_bytes());
        out
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        let mut origin = [0u8; 8];
        origin.copy_from_slice(&bytes[..8]);
        let mut sequence = [0u8; 8];
        sequence.copy_from_slice(&bytes[8..]);
        Self {
            origin: u64::from_be_bytes(origin),
            sequence: u64::from_be_bytes(sequence),
        }
    }

    pub fn as_u128(self) -> u128 {
        ((self.origin as u128) << 64) | self.sequence as u128
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}{:016x}", self.origin, self.sequence)
    }
}

/// Error returned when an origin consumes its entire 64-bit sequence space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageIdExhausted;

impl fmt::Display for MessageIdExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "message id sequence space exhausted")
    }
}

impl std::error::Error for MessageIdExhausted {}

/// Lock-free allocator for message ids belonging to one origin.
#[derive(Debug)]
pub struct MessageIdGenerator {
    origin: u64,
    next_sequence: AtomicU64,
}

impl MessageIdGenerator {
    pub fn new(origin: u64) -> Self {
        Self {
            origin,
            // Reserve sequence zero as an invalid/unassigned sentinel.
            next_sequence: AtomicU64::new(1),
        }
    }

    pub fn origin(&self) -> u64 {
        self.origin
    }

    pub fn next_id(&self) -> Result<MessageId, MessageIdExhausted> {
        let mut current = self.next_sequence.load(Ordering::Relaxed);
        loop {
            if current == 0 || current == u64::MAX {
                return Err(MessageIdExhausted);
            }
            match self.next_sequence.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(MessageId::new(self.origin, current)),
                Err(observed) => current = observed,
            }
        }
    }

    #[cfg(test)]
    fn with_next_sequence(origin: u64, next_sequence: u64) -> Self {
        Self {
            origin,
            next_sequence: AtomicU64::new(next_sequence),
        }
    }
}

/// Delivery metadata shared by local, remote, and durable messages.
///
/// Correlation groups a logical request tree. Causation points at the direct
/// parent message. A retry increments `attempt` but deliberately preserves
/// `id`, `correlation_id`, and `causation_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMeta {
    pub id: MessageId,
    pub correlation_id: MessageId,
    pub causation_id: Option<MessageId>,
    pub attempt: u32,
    /// Optional absolute Unix deadline in milliseconds.
    pub deadline_unix_ms: Option<u64>,
}

impl MessageMeta {
    /// Start a new causal tree. The root message is its own correlation id.
    pub fn root(id: MessageId) -> Self {
        Self {
            id,
            correlation_id: id,
            causation_id: None,
            attempt: 0,
            deadline_unix_ms: None,
        }
    }

    /// Create a new logical child message caused by `parent`.
    pub fn child(id: MessageId, parent: &MessageMeta) -> Self {
        Self {
            id,
            correlation_id: parent.correlation_id,
            causation_id: Some(parent.id),
            attempt: 0,
            deadline_unix_ms: parent.deadline_unix_ms,
        }
    }

    pub fn with_deadline(mut self, deadline_unix_ms: u64) -> Self {
        self.deadline_unix_ms = Some(deadline_unix_ms);
        self
    }

    /// Return metadata for another delivery attempt of the *same* logical
    /// message. Keeping `id` stable is the key deduplication invariant.
    pub fn retry(&self) -> Self {
        let mut retry = self.clone();
        retry.attempt = retry.attempt.saturating_add(1);
        retry
    }
}

/// Result of recording a message id after its associated state transition has
/// been durably committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupCommit {
    /// The window is disabled (`capacity == 0`), so no id was retained.
    TrackingDisabled,
    /// The message id had already been committed. Its eviction age is not
    /// refreshed; repeated duplicates therefore cannot pin themselves in the
    /// dedup window forever.
    AlreadyCommitted,
    /// The id was newly committed. `evicted` is the oldest committed id that
    /// fell out of the bounded retention window, if any.
    Committed { evicted: Option<MessageId> },
}

/// Serializable, ordered representation of a bounded dedup window.
///
/// `committed` is oldest-to-newest. Keeping order explicit makes retention
/// behavior deterministic across snapshot/recovery rather than depending on a
/// hash-table iteration order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupSnapshot {
    pub capacity: usize,
    pub committed: Vec<MessageId>,
}

/// Validation failure while restoring a persisted dedup snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupRestoreError {
    DisabledWindowContainsIds,
    TooManyCommittedIds,
    DuplicateCommittedId,
}

impl fmt::Display for DedupRestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DedupRestoreError::DisabledWindowContainsIds => {
                write!(f, "disabled dedup window cannot contain committed ids")
            }
            DedupRestoreError::TooManyCommittedIds => {
                write!(f, "dedup snapshot contains more ids than its capacity")
            }
            DedupRestoreError::DuplicateCommittedId => {
                write!(f, "dedup snapshot contains a duplicate message id")
            }
        }
    }
}

impl std::error::Error for DedupRestoreError {}

/// Exact bounded set of committed logical message ids.
///
/// This intentionally uses an exact `HashSet` + FIFO order instead of a Bloom
/// filter: a false positive in message deduplication could suppress legitimate
/// work. The capacity bound makes memory use explicit and operationally finite.
///
/// The runtime should call [`DedupWindow::contains`] before processing and
/// [`DedupWindow::commit`] only after the state transition and message-id
/// commit are durably recorded together. This type does not itself provide the
/// storage transaction; it provides deterministic retention semantics for that
/// persistence layer.
#[derive(Debug, Clone)]
pub struct DedupWindow {
    capacity: usize,
    committed: HashSet<MessageId>,
    order: VecDeque<MessageId>,
}

impl DedupWindow {
    /// Create a window retaining at most `capacity` committed ids.
    ///
    /// Capacity zero explicitly disables tracking. This is useful for transient
    /// actors, while durable/effectively-once paths should choose a non-zero
    /// retention policy.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            committed: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn contains(&self, id: &MessageId) -> bool {
        self.committed.contains(id)
    }

    /// Record a logical message id after its work has been committed.
    ///
    /// Recommitting an existing id returns `AlreadyCommitted` without moving
    /// the id to the back of the FIFO. This prevents duplicate traffic from
    /// extending its own dedup retention indefinitely.
    pub fn commit(&mut self, id: MessageId) -> DedupCommit {
        if self.capacity == 0 {
            return DedupCommit::TrackingDisabled;
        }
        if self.committed.contains(&id) {
            return DedupCommit::AlreadyCommitted;
        }

        let evicted = if self.order.len() == self.capacity {
            let oldest = self
                .order
                .pop_front()
                .expect("non-zero full dedup window must have an oldest id");
            self.committed.remove(&oldest);
            Some(oldest)
        } else {
            None
        };

        self.order.push_back(id);
        self.committed.insert(id);
        DedupCommit::Committed { evicted }
    }

    pub fn snapshot(&self) -> DedupSnapshot {
        DedupSnapshot {
            capacity: self.capacity,
            committed: self.order.iter().copied().collect(),
        }
    }

    /// Restore a previously persisted exact window while validating its
    /// capacity and uniqueness invariants. Corrupt snapshots fail closed rather
    /// than silently widening or weakening deduplication.
    pub fn restore(snapshot: DedupSnapshot) -> Result<Self, DedupRestoreError> {
        if snapshot.capacity == 0 && !snapshot.committed.is_empty() {
            return Err(DedupRestoreError::DisabledWindowContainsIds);
        }
        if snapshot.committed.len() > snapshot.capacity {
            return Err(DedupRestoreError::TooManyCommittedIds);
        }

        let mut window = Self::new(snapshot.capacity);
        for id in snapshot.committed {
            if !window.committed.insert(id) {
                return Err(DedupRestoreError::DuplicateCommittedId);
            }
            window.order.push_back(id);
        }
        Ok(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn message_id_wire_round_trip() {
        let id = MessageId::new(0x1122_3344_5566_7788, 0x99aa_bbcc_ddee_ff00);
        assert_eq!(MessageId::from_bytes(id.to_bytes()), id);
        assert_eq!(id.as_u128(), 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00u128);
    }

    #[test]
    fn generator_is_monotonic() {
        let generator = MessageIdGenerator::new(7);
        assert_eq!(generator.next_id().unwrap(), MessageId::new(7, 1));
        assert_eq!(generator.next_id().unwrap(), MessageId::new(7, 2));
    }

    #[test]
    fn generator_is_unique_across_threads() {
        let generator = Arc::new(MessageIdGenerator::new(42));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let generator = Arc::clone(&generator);
            threads.push(thread::spawn(move || {
                (0..1_000)
                    .map(|_| generator.next_id().unwrap())
                    .collect::<Vec<_>>()
            }));
        }

        let ids: HashSet<MessageId> = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 8_000);
    }

    #[test]
    fn generator_fails_closed_at_exhaustion() {
        let generator = MessageIdGenerator::with_next_sequence(1, u64::MAX);
        assert_eq!(generator.next_id(), Err(MessageIdExhausted));
    }

    #[test]
    fn retry_preserves_logical_identity() {
        let root = MessageMeta::root(MessageId::new(1, 10)).with_deadline(1234);
        let retry = root.retry();
        assert_eq!(retry.id, root.id);
        assert_eq!(retry.correlation_id, root.correlation_id);
        assert_eq!(retry.causation_id, root.causation_id);
        assert_eq!(retry.attempt, 1);
        assert_eq!(retry.deadline_unix_ms, Some(1234));
    }

    #[test]
    fn child_tracks_correlation_and_causation() {
        let root = MessageMeta::root(MessageId::new(1, 10));
        let child = MessageMeta::child(MessageId::new(1, 11), &root);
        assert_eq!(child.correlation_id, root.id);
        assert_eq!(child.causation_id, Some(root.id));
        assert_ne!(child.id, root.id);
    }

    #[test]
    fn dedup_window_recognizes_committed_retry() {
        let meta = MessageMeta::root(MessageId::new(9, 1));
        let retry = meta.retry();
        let mut window = DedupWindow::new(8);

        assert_eq!(
            window.commit(meta.id),
            DedupCommit::Committed { evicted: None }
        );
        assert!(window.contains(&retry.id));
        assert_eq!(window.commit(retry.id), DedupCommit::AlreadyCommitted);
    }

    #[test]
    fn duplicate_does_not_refresh_fifo_retention() {
        let a = MessageId::new(1, 1);
        let b = MessageId::new(1, 2);
        let c = MessageId::new(1, 3);
        let mut window = DedupWindow::new(2);

        window.commit(a);
        window.commit(b);
        assert_eq!(window.commit(a), DedupCommit::AlreadyCommitted);
        assert_eq!(
            window.commit(c),
            DedupCommit::Committed { evicted: Some(a) }
        );

        assert!(!window.contains(&a));
        assert!(window.contains(&b));
        assert!(window.contains(&c));
    }

    #[test]
    fn same_sequence_from_different_origins_is_not_duplicate() {
        let a = MessageId::new(10, 7);
        let b = MessageId::new(11, 7);
        let mut window = DedupWindow::new(4);

        assert_eq!(
            window.commit(a),
            DedupCommit::Committed { evicted: None }
        );
        assert_eq!(
            window.commit(b),
            DedupCommit::Committed { evicted: None }
        );
        assert_eq!(window.len(), 2);
    }

    #[test]
    fn zero_capacity_explicitly_disables_tracking() {
        let id = MessageId::new(1, 1);
        let mut window = DedupWindow::new(0);
        assert_eq!(window.commit(id), DedupCommit::TrackingDisabled);
        assert!(!window.contains(&id));
        assert!(window.is_empty());
    }

    #[test]
    fn dedup_snapshot_round_trips_fifo_order() {
        let ids = [
            MessageId::new(3, 1),
            MessageId::new(3, 2),
            MessageId::new(3, 3),
        ];
        let mut original = DedupWindow::new(3);
        for id in ids {
            original.commit(id);
        }

        let snapshot = original.snapshot();
        assert_eq!(snapshot.committed, ids);
        let mut restored = DedupWindow::restore(snapshot).unwrap();
        assert_eq!(restored.len(), 3);
        assert_eq!(
            restored.commit(MessageId::new(3, 4)),
            DedupCommit::Committed {
                evicted: Some(ids[0])
            }
        );
    }

    #[test]
    fn corrupt_dedup_snapshots_fail_closed() {
        let id = MessageId::new(1, 1);
        assert_eq!(
            DedupWindow::restore(DedupSnapshot {
                capacity: 0,
                committed: vec![id],
            })
            .unwrap_err(),
            DedupRestoreError::DisabledWindowContainsIds
        );
        assert_eq!(
            DedupWindow::restore(DedupSnapshot {
                capacity: 1,
                committed: vec![id, MessageId::new(1, 2)],
            })
            .unwrap_err(),
            DedupRestoreError::TooManyCommittedIds
        );
        assert_eq!(
            DedupWindow::restore(DedupSnapshot {
                capacity: 2,
                committed: vec![id, id],
            })
            .unwrap_err(),
            DedupRestoreError::DuplicateCommittedId
        );
    }
}
