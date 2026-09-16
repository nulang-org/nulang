//! Atomic durable-message commit staging.
//!
//! Effectively-once handling requires one crash-consistency invariant:
//! the actor state transition and the receiver's committed `MessageId` must be
//! persisted together. Persisting them independently creates two bad windows:
//! state without dedup replays work, while dedup without state suppresses work
//! that never actually committed.
//!
//! This module provides an immutable staging step. It clones the current exact
//! dedup window, commits the logical message id into the clone, and packages
//! actor state + actor sequence + dedup snapshot into one serializable bundle.
//! The caller writes that bundle atomically, then installs `next_window` only
//! after storage confirms success.

use crate::message::{DedupCommit, DedupRestoreError, DedupSnapshot, DedupWindow, MessageId};
use serde::{Deserialize, Serialize};
use std::fmt;

/// One storage record containing both durable actor state and the committed
/// receiver-side dedup window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurableInboxBundle<T> {
    /// Monotonic actor/journal sequence represented by `state`.
    pub actor_sequence: u64,
    /// Backend-specific durable state payload.
    pub state: T,
    /// Exact bounded committed-message window at the same commit point.
    pub dedup: DedupSnapshot,
}

/// Staged in-memory transition awaiting one atomic storage write.
#[derive(Debug, Clone)]
pub struct StagedDurableCommit<T> {
    bundle: DurableInboxBundle<T>,
    next_window: DedupWindow,
    dedup_outcome: DedupCommit,
}

impl<T> StagedDurableCommit<T> {
    pub fn bundle(&self) -> &DurableInboxBundle<T> {
        &self.bundle
    }

    pub fn dedup_outcome(&self) -> DedupCommit {
        self.dedup_outcome
    }

    /// Consume the staged commit after the durable write succeeds.
    ///
    /// The returned window is the only in-memory dedup state that should be
    /// installed. If the storage write fails, drop `self` and keep the old
    /// live window unchanged.
    pub fn into_committed_parts(self) -> (DurableInboxBundle<T>, DedupWindow) {
        (self.bundle, self.next_window)
    }
}

/// Prepare one atomic state+dedup write without mutating the live dedup window.
pub fn stage_durable_message_commit<T>(
    state: T,
    actor_sequence: u64,
    live_window: &DedupWindow,
    message_id: MessageId,
) -> StagedDurableCommit<T> {
    let mut next_window = live_window.clone();
    let dedup_outcome = next_window.commit(message_id);
    let bundle = DurableInboxBundle {
        actor_sequence,
        state,
        dedup: next_window.snapshot(),
    };
    StagedDurableCommit {
        bundle,
        next_window,
        dedup_outcome,
    }
}

/// Recovery result reconstructed from one atomic durable inbox record.
#[derive(Debug, Clone)]
pub struct RestoredDurableInbox<T> {
    pub actor_sequence: u64,
    pub state: T,
    pub dedup: DedupWindow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableInboxRestoreError {
    Dedup(DedupRestoreError),
}

impl fmt::Display for DurableInboxRestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dedup(error) => write!(f, "invalid durable inbox dedup state: {error}"),
        }
    }
}

impl std::error::Error for DurableInboxRestoreError {}

impl From<DedupRestoreError> for DurableInboxRestoreError {
    fn from(value: DedupRestoreError) -> Self {
        Self::Dedup(value)
    }
}

impl<T> DurableInboxBundle<T> {
    pub fn restore(self) -> Result<RestoredDurableInbox<T>, DurableInboxRestoreError> {
        let dedup = DedupWindow::restore(self.dedup)?;
        Ok(RestoredDurableInbox {
            actor_sequence: self.actor_sequence,
            state: self.state,
            dedup,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{DedupCommit, MessageId};
    use serde_json::json;

    #[test]
    fn staging_does_not_mutate_live_window_before_storage_success() {
        let live = DedupWindow::new(4);
        let id = MessageId::new(7, 1);
        let staged = stage_durable_message_commit(json!({"count": 1}), 9, &live, id);

        assert!(!live.contains(&id));
        assert!(staged.bundle().dedup.committed.contains(&id));
        assert_eq!(
            staged.dedup_outcome(),
            DedupCommit::Committed { evicted: None }
        );
    }

    #[test]
    fn successful_write_can_install_exact_staged_window() {
        let live = DedupWindow::new(4);
        let id = MessageId::new(7, 1);
        let staged = stage_durable_message_commit(json!({"count": 1}), 9, &live, id);

        // Simulate storage success: only now install the staged window.
        let (bundle, installed) = staged.into_committed_parts();
        assert_eq!(bundle.actor_sequence, 9);
        assert!(installed.contains(&id));
    }

    #[test]
    fn retry_of_committed_message_is_detected_without_refreshing_retention() {
        let mut live = DedupWindow::new(2);
        let a = MessageId::new(1, 1);
        let b = MessageId::new(1, 2);
        live.commit(a);
        live.commit(b);

        let staged = stage_durable_message_commit((), 3, &live, a);
        assert_eq!(staged.dedup_outcome(), DedupCommit::AlreadyCommitted);
        assert_eq!(staged.bundle().dedup.committed, vec![a, b]);
    }

    #[test]
    fn bundle_round_trip_recovers_state_sequence_and_dedup_together() {
        let mut live = DedupWindow::new(3);
        live.commit(MessageId::new(4, 1));
        let staged = stage_durable_message_commit(
            json!({"balance": 125}),
            44,
            &live,
            MessageId::new(4, 2),
        );
        let (bundle, _) = staged.into_committed_parts();

        let encoded = serde_json::to_vec(&bundle).unwrap();
        let decoded: DurableInboxBundle<serde_json::Value> =
            serde_json::from_slice(&encoded).unwrap();
        let restored = decoded.restore().unwrap();

        assert_eq!(restored.actor_sequence, 44);
        assert_eq!(restored.state, json!({"balance": 125}));
        assert!(restored.dedup.contains(&MessageId::new(4, 1)));
        assert!(restored.dedup.contains(&MessageId::new(4, 2)));
    }

    #[test]
    fn corrupt_dedup_snapshot_fails_recovery_closed() {
        let bundle = DurableInboxBundle {
            actor_sequence: 1,
            state: (),
            dedup: DedupSnapshot {
                capacity: 1,
                committed: vec![MessageId::new(1, 1), MessageId::new(1, 2)],
            },
        };
        assert!(matches!(
            bundle.restore(),
            Err(DurableInboxRestoreError::Dedup(
                DedupRestoreError::TooManyCommittedIds
            ))
        ));
    }

    #[test]
    fn bounded_window_eviction_is_part_of_same_commit_record() {
        let mut live = DedupWindow::new(2);
        let a = MessageId::new(1, 1);
        let b = MessageId::new(1, 2);
        let c = MessageId::new(1, 3);
        live.commit(a);
        live.commit(b);

        let staged = stage_durable_message_commit((), 10, &live, c);
        assert_eq!(
            staged.dedup_outcome(),
            DedupCommit::Committed { evicted: Some(a) }
        );
        assert_eq!(staged.bundle().dedup.committed, vec![b, c]);
        assert!(live.contains(&a)); // live state still untouched until write succeeds
    }
}
