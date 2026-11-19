//! Point-key optimistic concurrency for one in-memory NuDB tablet.
//!
//! Two transactions prepared at the same snapshot may commit in order even if
//! the tablet sequence has changed, provided none of their read or write keys
//! has changed since that snapshot. No predicate/range validation is provided.
//! WAL ordering and durability remain the responsibility of WalBackedTablet.

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

    /// Revalidate a prepared point transaction and assign the next sequence.
    /// No state changes are made before a WAL writer durably accepts the result.
    pub(crate) fn rebase_optimistic_write(
        &self,
        proposed: OptimisticTabletWrite,
    ) -> Result<TabletWrite, TabletError> {
        let OptimisticTabletWrite {
            tablet_id,
            ownership_epoch,
            snapshot_sequence,
            read_keys,
            mutations,
        } = proposed;
        if tablet_id != self.descriptor().id() {
            return Err(TabletError::WrongTablet {
                expected: self.descriptor().id(),
                presented: tablet_id,
            });
        }
        validate_epoch(self.descriptor(), ownership_epoch)?;
        if snapshot_sequence > self.current_sequence() {
            return Err(TabletError::SnapshotAhead {
                committed: self.current_sequence(),
                requested: snapshot_sequence,
            });
        }
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
