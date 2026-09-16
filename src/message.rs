//! Stable message identity and causal metadata.
//!
//! Message identity is deliberately independent of packet sequence numbers,
//! actor ids, and tracing ids. A logical delivery keeps the same `MessageId`
//! across retries so receivers can deduplicate replayed work without confusing
//! a retry for a new message.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Stable 128-bit identity for one logical actor message.
///
/// `origin` identifies the runtime/node that allocated the id. `sequence` is
/// monotonically increasing within that origin. Distributed runtimes should
/// use their stable `NodeId` as the origin; standalone runtimes may use zero
/// when identity only needs to be unique within one process/runtime boundary.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
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
        assert_eq!(
            id.as_u128(),
            0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00u128
        );
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
}
