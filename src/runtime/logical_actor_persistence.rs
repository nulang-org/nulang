//! Logical-identity keyed persistence with authoritative epoch fencing.
//!
//! This is a reference in-memory store for the persistence semantics required
//! by distributed virtual actors. It intentionally lives alongside the legacy
//! `PersistenceStore` API, whose durable key is still `actor_id: u64`.
//!
//! The important invariant here is executable: once ownership advances to a
//! newer epoch, a stale node cannot commit durable state even if it continues
//! running briefly after a partition or pause.

use std::collections::HashMap;
use std::fmt;

use super::{
    ActivationEpoch, ActivationHandle, ActorSnapshot, GrainId, JournalEntry,
    LogicalActorOwnershipDirectory, LogicalActorOwnershipError, LogicalActorOwnershipRecord, NodeId,
};

/// Authority stamp attached to one durable logical-actor write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalActorCommitStamp {
    pub grain_id: GrainId,
    pub node_id: NodeId,
    pub epoch: ActivationEpoch,
}

impl LogicalActorCommitStamp {
    pub fn new(grain_id: GrainId, node_id: NodeId, epoch: ActivationEpoch) -> Self {
        Self {
            grain_id,
            node_id,
            epoch,
        }
    }
}

/// Durable-write rejection from the logical-actor store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalActorCommitError {
    Unauthorized {
        grain_id: GrainId,
        node_id: NodeId,
        epoch: ActivationEpoch,
        current: Option<LogicalActorOwnershipRecord>,
    },
    StaleSnapshotSequence {
        grain_id: GrainId,
        current: u64,
        attempted: u64,
    },
    ConflictingJournalSequence {
        grain_id: GrainId,
        sequence: u64,
    },
}

impl fmt::Display for LogicalActorCommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogicalActorCommitError::Unauthorized {
                grain_id,
                node_id,
                epoch,
                ..
            } => write!(
                f,
                "unauthorized logical-actor commit for {} from {:?} at epoch {}",
                grain_id.actor_name(),
                node_id,
                epoch.get()
            ),
            LogicalActorCommitError::StaleSnapshotSequence {
                grain_id,
                current,
                attempted,
            } => write!(
                f,
                "stale snapshot sequence for {}: attempted {}, current {}",
                grain_id.actor_name(),
                attempted,
                current
            ),
            LogicalActorCommitError::ConflictingJournalSequence {
                grain_id,
                sequence,
            } => write!(
                f,
                "conflicting journal payload for {} at committed sequence {}",
                grain_id.actor_name(),
                sequence
            ),
        }
    }
}

impl std::error::Error for LogicalActorCommitError {}


/// Common fenced durable-state boundary for logical actors.
///
/// Implementations must authorize every write using the full logical identity,
/// owner node, and authoritative activation epoch. The authorization check and
/// durable mutation must be one atomic storage operation in persistent
/// backends; checking authority in process memory before a separate write is
/// not sufficient fencing.
pub trait LogicalActorPersistenceStore: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn save_logical_snapshot(
        &mut self,
        stamp: &LogicalActorCommitStamp,
        snapshot: ActorSnapshot,
    ) -> Result<(), Self::Error>;

    fn load_logical_snapshot(
        &self,
        grain_id: &GrainId,
    ) -> Result<Option<ActorSnapshot>, Self::Error>;

    fn append_logical_journal(
        &mut self,
        stamp: &LogicalActorCommitStamp,
        entry: JournalEntry,
    ) -> Result<(), Self::Error>;

    fn read_logical_journal(
        &self,
        grain_id: &GrainId,
    ) -> Result<Vec<JournalEntry>, Self::Error>;
}

/// In-memory reference implementation of logical-identity keyed durable state.
///
/// Production backends should preserve the same authorization rules while
/// storing ownership and state in a linearizable/transactional system.
#[derive(Debug, Default)]
pub struct FencedLogicalActorStore {
    ownership: LogicalActorOwnershipDirectory,
    snapshots: HashMap<GrainId, ActorSnapshot>,
    journals: HashMap<GrainId, Vec<JournalEntry>>,
}

impl FencedLogicalActorStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn grant_ownership(
        &mut self,
        grain_id: GrainId,
        node_id: NodeId,
        activation_handle: ActivationHandle,
        epoch: ActivationEpoch,
    ) -> Result<LogicalActorOwnershipRecord, LogicalActorOwnershipError> {
        self.ownership
            .grant(grain_id, node_id, activation_handle, epoch)
    }

    pub fn fence_ownership(
        &mut self,
        grain_id: GrainId,
        epoch: ActivationEpoch,
    ) -> Result<Option<LogicalActorOwnershipRecord>, LogicalActorOwnershipError> {
        self.ownership.fence(grain_id, epoch)
    }

    pub fn ownership(&self) -> &LogicalActorOwnershipDirectory {
        &self.ownership
    }

    /// Save a snapshot under the full logical identity after validating the
    /// authoritative owner node and epoch.
    ///
    /// `ActorSnapshot.actor_id` is retained only as legacy payload metadata;
    /// it is not used as this store's key.
    pub fn save_snapshot(
        &mut self,
        stamp: &LogicalActorCommitStamp,
        snapshot: ActorSnapshot,
    ) -> Result<(), LogicalActorCommitError> {
        self.authorize(stamp)?;

        if let Some(current) = self.snapshots.get(&stamp.grain_id) {
            if snapshot.sequence < current.sequence {
                return Err(LogicalActorCommitError::StaleSnapshotSequence {
                    grain_id: stamp.grain_id.clone(),
                    current: current.sequence,
                    attempted: snapshot.sequence,
                });
            }
        }

        self.snapshots.insert(stamp.grain_id.clone(), snapshot);
        Ok(())
    }

    pub fn load_snapshot(&self, grain_id: &GrainId) -> Option<ActorSnapshot> {
        self.snapshots.get(grain_id).cloned()
    }

    /// Append a journal record only for the current authoritative owner.
    pub fn append_journal(
        &mut self,
        stamp: &LogicalActorCommitStamp,
        entry: JournalEntry,
    ) -> Result<(), LogicalActorCommitError> {
        self.authorize(stamp)?;
        let journal = self.journals.entry(stamp.grain_id.clone()).or_default();
        if let Some(existing) = journal.iter().find(|existing| existing.sequence == entry.sequence) {
            if existing.behavior_id == entry.behavior_id && existing.payload == entry.payload {
                return Ok(());
            }
            return Err(LogicalActorCommitError::ConflictingJournalSequence {
                grain_id: stamp.grain_id.clone(),
                sequence: entry.sequence,
            });
        }
        journal.push(entry);
        Ok(())
    }

    pub fn read_journal(&self, grain_id: &GrainId) -> Vec<JournalEntry> {
        self.journals.get(grain_id).cloned().unwrap_or_default()
    }

    fn authorize(&self, stamp: &LogicalActorCommitStamp) -> Result<(), LogicalActorCommitError> {
        if self
            .ownership
            .authorizes_commit(&stamp.grain_id, stamp.node_id, stamp.epoch)
        {
            return Ok(());
        }

        Err(LogicalActorCommitError::Unauthorized {
            grain_id: stamp.grain_id.clone(),
            node_id: stamp.node_id,
            epoch: stamp.epoch,
            current: self.ownership.record_for(&stamp.grain_id),
        })
    }
}

impl LogicalActorPersistenceStore for FencedLogicalActorStore {
    type Error = LogicalActorCommitError;

    fn save_logical_snapshot(
        &mut self,
        stamp: &LogicalActorCommitStamp,
        snapshot: ActorSnapshot,
    ) -> Result<(), Self::Error> {
        self.save_snapshot(stamp, snapshot)
    }

    fn load_logical_snapshot(
        &self,
        grain_id: &GrainId,
    ) -> Result<Option<ActorSnapshot>, Self::Error> {
        Ok(self.load_snapshot(grain_id))
    }

    fn append_logical_journal(
        &mut self,
        stamp: &LogicalActorCommitStamp,
        entry: JournalEntry,
    ) -> Result<(), Self::Error> {
        self.append_journal(stamp, entry)
    }

    fn read_logical_journal(
        &self,
        grain_id: &GrainId,
    ) -> Result<Vec<JournalEntry>, Self::Error> {
        Ok(self.read_journal(grain_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::PersistedValue;

    fn handle(raw: u64) -> ActivationHandle {
        ActivationHandle::new(raw).unwrap()
    }

    fn epoch(raw: u64) -> ActivationEpoch {
        ActivationEpoch::new(raw).unwrap()
    }

    fn snapshot(actor_id: u64, sequence: u64, value: i64) -> ActorSnapshot {
        let mut snapshot = ActorSnapshot {
            actor_id,
            sequence,
            ..ActorSnapshot::default()
        };
        snapshot
            .state
            .insert("value".to_string(), PersistedValue::Int(value));
        snapshot
    }

    fn journal(sequence: u64) -> JournalEntry {
        JournalEntry {
            sequence,
            behavior_id: 0,
            payload: vec![PersistedValue::Int(sequence as i64)],
        }
    }

    #[test]
    fn stale_owner_cannot_commit_after_handoff() {
        let mut store = FencedLogicalActorStore::new();
        let grain = GrainId::new("Account", "42");
        store
            .grant_ownership(grain.clone(), NodeId(1), handle(10), epoch(1))
            .unwrap();

        let old = LogicalActorCommitStamp::new(grain.clone(), NodeId(1), epoch(1));
        store.save_snapshot(&old, snapshot(100, 1, 10)).unwrap();
        store.append_journal(&old, journal(1)).unwrap();

        store
            .grant_ownership(grain.clone(), NodeId(2), handle(20), epoch(2))
            .unwrap();

        assert!(matches!(
            store.save_snapshot(&old, snapshot(100, 2, 99)),
            Err(LogicalActorCommitError::Unauthorized { .. })
        ));
        assert!(matches!(
            store.append_journal(&old, journal(2)),
            Err(LogicalActorCommitError::Unauthorized { .. })
        ));

        let current = LogicalActorCommitStamp::new(grain.clone(), NodeId(2), epoch(2));
        store.save_snapshot(&current, snapshot(200, 2, 20)).unwrap();
        store.append_journal(&current, journal(2)).unwrap();

        assert_eq!(store.load_snapshot(&grain).unwrap().actor_id, 200);
        assert_eq!(store.read_journal(&grain).len(), 2);
    }

    #[test]
    fn fenced_actor_accepts_no_commits_until_newer_grant() {
        let mut store = FencedLogicalActorStore::new();
        let grain = GrainId::new("Account", "fenced");
        store
            .grant_ownership(grain.clone(), NodeId(1), handle(1), epoch(3))
            .unwrap();
        let stamp = LogicalActorCommitStamp::new(grain.clone(), NodeId(1), epoch(3));

        store.fence_ownership(grain.clone(), epoch(4)).unwrap();
        assert!(matches!(
            store.save_snapshot(&stamp, snapshot(1, 1, 1)),
            Err(LogicalActorCommitError::Unauthorized { .. })
        ));

        store
            .grant_ownership(grain.clone(), NodeId(2), handle(2), epoch(5))
            .unwrap();
        let replacement = LogicalActorCommitStamp::new(grain, NodeId(2), epoch(5));
        store
            .save_snapshot(&replacement, snapshot(2, 1, 2))
            .unwrap();
    }

    #[test]
    fn full_logical_identity_not_legacy_actor_id_selects_storage_key() {
        let mut store = FencedLogicalActorStore::new();
        let first = GrainId::new("User", "a");
        let second = GrainId::new("User", "b");

        store
            .grant_ownership(first.clone(), NodeId(1), handle(1), epoch(1))
            .unwrap();
        store
            .grant_ownership(second.clone(), NodeId(1), handle(2), epoch(1))
            .unwrap();

        let first_stamp = LogicalActorCommitStamp::new(first.clone(), NodeId(1), epoch(1));
        let second_stamp = LogicalActorCommitStamp::new(second.clone(), NodeId(1), epoch(1));

        // The legacy actor id payload is intentionally identical. The logical
        // store must still keep the durable entities completely separate.
        store
            .save_snapshot(&first_stamp, snapshot(777, 1, 11))
            .unwrap();
        store
            .save_snapshot(&second_stamp, snapshot(777, 1, 22))
            .unwrap();

        assert_eq!(
            store.load_snapshot(&first).unwrap().state.get("value"),
            Some(&PersistedValue::Int(11))
        );
        assert_eq!(
            store.load_snapshot(&second).unwrap().state.get("value"),
            Some(&PersistedValue::Int(22))
        );
    }

    #[test]
    fn journal_replay_is_idempotent_but_conflicting_sequence_fails_closed() {
        let mut store = FencedLogicalActorStore::new();
        let grain = GrainId::new("Account", "journal-idempotence");
        store
            .grant_ownership(grain.clone(), NodeId(1), handle(1), epoch(1))
            .unwrap();
        let stamp = LogicalActorCommitStamp::new(grain.clone(), NodeId(1), epoch(1));

        store.append_journal(&stamp, journal(1)).unwrap();
        store.append_journal(&stamp, journal(1)).unwrap();
        assert_eq!(store.read_journal(&grain).len(), 1);

        let conflicting = JournalEntry {
            sequence: 1,
            behavior_id: 7,
            payload: vec![PersistedValue::Int(999)],
        };
        assert_eq!(
            store.append_journal(&stamp, conflicting),
            Err(LogicalActorCommitError::ConflictingJournalSequence {
                grain_id: grain,
                sequence: 1,
            })
        );
    }

    #[test]
    fn snapshot_sequence_cannot_move_backwards() {
        let mut store = FencedLogicalActorStore::new();
        let grain = GrainId::new("Account", "sequence");
        store
            .grant_ownership(grain.clone(), NodeId(1), handle(1), epoch(1))
            .unwrap();
        let stamp = LogicalActorCommitStamp::new(grain.clone(), NodeId(1), epoch(1));

        store.save_snapshot(&stamp, snapshot(1, 10, 10)).unwrap();
        assert_eq!(
            store.save_snapshot(&stamp, snapshot(1, 9, 9)),
            Err(LogicalActorCommitError::StaleSnapshotSequence {
                grain_id: grain,
                current: 10,
                attempted: 9,
            })
        );
    }
}
