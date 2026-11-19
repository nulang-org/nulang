//! Point-key optimistic concurrency for one in-memory NuDB tablet.
//!
//! Two transactions prepared at the same snapshot may commit in order even if
//! the tablet sequence has changed, provided none of their read or write keys
//! has changed since that snapshot. No predicate/range validation is provided.
//! WAL ordering and durability remain the responsibility of WalBackedTablet.

use std::collections::BTreeSet;

use super::tablet::{
    MemoryTablet, TabletDescriptor, TabletError, TabletId, TabletMutation, TabletWrite,
};

/// Prevent an unbounded caller-provided point read set from dominating admission.
pub const MAX_POINT_READ_KEYS: usize = 4096;

/// Prepared snapshot-scoped point-key transaction. This is not a WAL record.
///
/// Commit assigns the final tablet sequence only after validating point-key
/// conflicts. This does not provide phantom detection, multi-tablet atomicity,
/// or distributed serializable transactions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimisticTabletWrite {
    tablet_id: TabletId,
    ownership_epoch: u64,
    snapshot_sequence: u64,
    read_keys: Vec<Vec<u8>>,
    mutations: Vec<TabletMutation>,
}

impl OptimisticTabletWrite {
    pub fn mutations(&self) -> &[TabletMutation] {
        &self.mutations
    }
}

/// Snapshot-scoped point transaction that automatically records every
/// database key read by `read`, including reads returning no value.
///
/// Mutable writes are staged locally and are visible to later reads in the
/// same transaction; the writes are also conflict-checked at commit. Reads
/// return owned bytes so callers cannot keep a mutable tablet borrow alive.
/// This is not a range-query or phantom-safe transaction, and it does not
/// currently acquire a MVCC GC snapshot pin.
#[derive(Debug)]
pub struct PointTransaction {
    descriptor: TabletDescriptor,
    snapshot_sequence: u64,
    read_keys: BTreeSet<Vec<u8>>,
    mutations: Vec<TabletMutation>,
}

impl PointTransaction {
    pub fn snapshot_sequence(&self) -> u64 {
        self.snapshot_sequence
    }

    fn validate_source(&self, tablet: &MemoryTablet) -> Result<(), TabletError> {
        if self.descriptor.id() != tablet.descriptor().id() {
            return Err(TabletError::WrongTablet {
                expected: tablet.descriptor().id(),
                presented: self.descriptor.id(),
            });
        }
        validate_epoch(tablet.descriptor(), self.descriptor.ownership_epoch())?;
        if self.snapshot_sequence > tablet.current_sequence() {
            return Err(TabletError::SnapshotAhead {
                committed: tablet.current_sequence(),
                requested: self.snapshot_sequence,
            });
        }
        Ok(())
    }

    /// Read at the transaction's original committed snapshot, recording the
    /// key so later writes by another transaction cause commit rejection.
    /// Staged writes take precedence and already belong to the write set.
    pub fn read(
        &mut self,
        tablet: &MemoryTablet,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, TabletError> {
        self.validate_source(tablet)?;
        if !self.descriptor.range().contains(key) {
            return Err(TabletError::KeyOutsideTabletRange);
        }
        if let Some(mutation) = self.mutations.iter().rev().find(|m| m.key() == key) {
            return Ok(match mutation {
                TabletMutation::Put { value, .. } => Some(value.clone()),
                TabletMutation::Delete { .. } => None,
            });
        }
        if !self.read_keys.contains(key) && self.read_keys.len() >= MAX_POINT_READ_KEYS {
            return Err(TabletError::TooManyOptimisticReadKeys {
                requested: self.read_keys.len() + 1,
                limit: MAX_POINT_READ_KEYS,
            });
        }
        let result = tablet.read_at(key, self.snapshot_sequence)?.map(Vec::from);
        self.read_keys.insert(key.to_vec());
        Ok(result)
    }

    /// Stage one write. The final mutation to a key wins within this
    /// transaction; writes remain uncommitted until durable WAL admission.
    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), TabletError> {
        if !self.descriptor.range().contains(&key) {
            return Err(TabletError::KeyOutsideTabletRange);
        }
        self.mutations.push(TabletMutation::Put { key, value });
        Ok(())
    }

    pub fn delete(&mut self, key: Vec<u8>) -> Result<(), TabletError> {
        if !self.descriptor.range().contains(&key) {
            return Err(TabletError::KeyOutsideTabletRange);
        }
        self.mutations.push(TabletMutation::Delete { key });
        Ok(())
    }

    /// Admission does not reserve a tablet sequence; callers can prepare
    /// independently and let the ordered WAL commit path detect conflicts.
    pub(crate) fn into_prepared(
        self,
        tablet: &MemoryTablet,
    ) -> Result<OptimisticTabletWrite, TabletError> {
        self.validate_source(tablet)?;
        tablet.prepare_optimistic_write(
            self.descriptor.ownership_epoch(),
            self.snapshot_sequence,
            self.read_keys.into_iter().collect(),
            self.mutations,
        )
    }
}

fn validate_epoch(descriptor: &TabletDescriptor, presented: u64) -> Result<(), TabletError> {
    let current = descriptor.ownership_epoch();
    if presented < current {
        return Err(TabletError::StaleEpoch { current, presented });
    }
    if presented > current {
        return Err(TabletError::UnknownEpoch { current, presented });
    }
    Ok(())
}

impl MemoryTablet {
    /// Begin a point-key transaction at the latest committed snapshot.
    pub fn begin_point_transaction(&self) -> PointTransaction {
        PointTransaction {
            descriptor: self.descriptor().clone(),
            snapshot_sequence: self.current_sequence(),
            read_keys: BTreeSet::new(),
            mutations: Vec::new(),
        }
    }

    /// Prepare against a committed snapshot without reserving a global tablet
    /// sequence. A later writer may commit before this transaction does.
    pub fn prepare_optimistic_write(
        &self,
        ownership_epoch: u64,
        snapshot_sequence: u64,
        mut read_keys: Vec<Vec<u8>>,
        mutations: Vec<TabletMutation>,
    ) -> Result<OptimisticTabletWrite, TabletError> {
        validate_epoch(self.descriptor(), ownership_epoch)?;
        if snapshot_sequence > self.current_sequence() {
            return Err(TabletError::SnapshotAhead {
                committed: self.current_sequence(),
                requested: snapshot_sequence,
            });
        }
        if read_keys.len() > MAX_POINT_READ_KEYS {
            return Err(TabletError::TooManyOptimisticReadKeys {
                requested: read_keys.len(),
                limit: MAX_POINT_READ_KEYS,
            });
        }
        let range = self.descriptor().range();
        if read_keys.iter().any(|key| !range.contains(key))
            || mutations
                .iter()
                .any(|mutation| !range.contains(mutation.key()))
        {
            return Err(TabletError::KeyOutsideTabletRange);
        }
        read_keys.sort_unstable();
        read_keys.dedup();
        Ok(OptimisticTabletWrite {
            tablet_id: self.descriptor().id(),
            ownership_epoch,
            snapshot_sequence,
            read_keys,
            mutations,
        })
    }

    /// Check routing/ownership/snapshot admission independently of read/write
    /// conflict validation. A durable idempotent retry must recheck these
    /// invariants even if its transaction ID already has a matching receipt.
    pub(crate) fn validate_optimistic_identity(
        &self,
        proposed: &OptimisticTabletWrite,
    ) -> Result<(), TabletError> {
        if proposed.tablet_id != self.descriptor().id() {
            return Err(TabletError::WrongTablet {
                expected: self.descriptor().id(),
                presented: proposed.tablet_id,
            });
        }
        validate_epoch(self.descriptor(), proposed.ownership_epoch)?;
        if proposed.snapshot_sequence > self.current_sequence() {
            return Err(TabletError::SnapshotAhead {
                committed: self.current_sequence(),
                requested: proposed.snapshot_sequence,
            });
        }
        let range = self.descriptor().range();
        if proposed.read_keys.iter().any(|key| !range.contains(key))
            || proposed
                .mutations
                .iter()
                .any(|mutation| !range.contains(mutation.key()))
        {
            return Err(TabletError::KeyOutsideTabletRange);
        }
        Ok(())
    }

    /// Revalidate a prepared point transaction and assign the next sequence.
    /// No state changes are made before a WAL writer durably accepts the result.
    pub(crate) fn rebase_optimistic_write(
        &self,
        proposed: OptimisticTabletWrite,
    ) -> Result<TabletWrite, TabletError> {
        self.validate_optimistic_identity(&proposed)?;
        let OptimisticTabletWrite {
            tablet_id: _,
            ownership_epoch,
            snapshot_sequence,
            read_keys,
            mutations,
        } = proposed;
        let range = self.descriptor().range();
        for key in read_keys
            .iter()
            .map(Vec::as_slice)
            .chain(mutations.iter().map(TabletMutation::key))
        {
            if !range.contains(key) {
                return Err(TabletError::KeyOutsideTabletRange);
            }
            if let Some(committed) = self.latest_key_write_sequence(key) {
                if committed > snapshot_sequence {
                    return Err(TabletError::OptimisticConflict {
                        key: key.to_vec(),
                        snapshot: snapshot_sequence,
                        committed,
                    });
                }
            }
        }
        TabletWrite::prepare(
            self.descriptor(),
            ownership_epoch,
            self.current_sequence(),
            self.current_sequence(),
            mutations,
        )
    }

    /// In-memory prototype; the WAL-backed equivalent provides durability.
    pub fn commit_optimistic_write(
        &mut self,
        proposed: OptimisticTabletWrite,
    ) -> Result<u64, TabletError> {
        let write = self.rebase_optimistic_write(proposed)?;
        self.commit(write)
    }
}
