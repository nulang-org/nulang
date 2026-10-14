//! Range-tablet ownership and write-fencing primitives.
//!
//! This module is deliberately storage-engine agnostic. It defines the
//! invariants a future NuDB storage engine can enforce before mapping accepted
//! writes onto WAL/MVCC/Raft machinery.

use std::collections::BTreeMap;
use std::fmt;

/// Stable identifier for one logical tablet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabletId(u64);

impl TabletId {
    /// Construct a non-zero tablet identifier.
    pub fn new(value: u64) -> Result<Self, TabletError> {
        if value == 0 {
            return Err(TabletError::InvalidTabletId);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Half-open byte-key interval: `[start, end)`.
///
/// `end = None` denotes positive infinity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRange {
    start: Vec<u8>,
    end: Option<Vec<u8>>,
}

impl KeyRange {
    pub fn new(start: Vec<u8>, end: Option<Vec<u8>>) -> Result<Self, TabletError> {
        if let Some(end_key) = end.as_deref() {
            if end_key <= start.as_slice() {
                return Err(TabletError::InvalidKeyRange);
            }
        }
        Ok(Self { start, end })
    }

    pub fn start(&self) -> &[u8] {
        &self.start
    }

    pub fn end(&self) -> Option<&[u8]> {
        self.end.as_deref()
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        key >= self.start.as_slice() && self.end.as_deref().map(|end| key < end).unwrap_or(true)
    }

    fn contains_interior_split(&self, key: &[u8]) -> bool {
        key > self.start.as_slice() && self.end.as_deref().map(|end| key < end).unwrap_or(true)
    }

    fn split_at(&self, split_key: &[u8]) -> Result<(Self, Self), TabletError> {
        if !self.contains_interior_split(split_key) {
            return Err(TabletError::SplitKeyOutsideInterior);
        }

        let left = Self::new(self.start.clone(), Some(split_key.to_vec()))?;
        let right = Self::new(split_key.to_vec(), self.end.clone())?;
        Ok((left, right))
    }
}

/// Current ownership metadata for one tablet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabletDescriptor {
    id: TabletId,
    range: KeyRange,
    ownership_epoch: u64,
}

impl TabletDescriptor {
    pub fn new(id: TabletId, range: KeyRange, ownership_epoch: u64) -> Result<Self, TabletError> {
        if ownership_epoch == 0 {
            return Err(TabletError::InvalidOwnershipEpoch);
        }
        Ok(Self {
            id,
            range,
            ownership_epoch,
        })
    }

    pub fn id(&self) -> TabletId {
        self.id
    }

    pub fn range(&self) -> &KeyRange {
        &self.range
    }

    pub fn ownership_epoch(&self) -> u64 {
        self.ownership_epoch
    }

    /// Plan a deterministic range split.
    ///
    /// Child ranges exactly cover the source range and share a fresh ownership
    /// epoch. One child may intentionally retain the source tablet id, which is
    /// useful for implementations that keep the left-hand lineage stable.
    pub fn plan_split(
        &self,
        split_key: &[u8],
        left_id: TabletId,
        right_id: TabletId,
        child_epoch: u64,
    ) -> Result<TabletSplitPlan, TabletError> {
        if left_id == right_id {
            return Err(TabletError::DuplicateChildTablet);
        }
        if child_epoch <= self.ownership_epoch {
            return Err(TabletError::EpochNotAdvanced {
                current: self.ownership_epoch,
                proposed: child_epoch,
            });
        }

        let (left_range, right_range) = self.range.split_at(split_key)?;
        let left = Self::new(left_id, left_range, child_epoch)?;
        let right = Self::new(right_id, right_range, child_epoch)?;

        Ok(TabletSplitPlan {
            source: self.clone(),
            split_key: split_key.to_vec(),
            left,
            right,
        })
    }
}

/// Pure metadata describing one source tablet split into two children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabletSplitPlan {
    pub source: TabletDescriptor,
    pub split_key: Vec<u8>,
    pub left: TabletDescriptor,
    pub right: TabletDescriptor,
}

/// One mutation staged for a tablet commit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TabletMutation {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

impl TabletMutation {
    pub fn key(&self) -> &[u8] {
        match self {
            Self::Put { key, .. } | Self::Delete { key } => key,
        }
    }
}

/// Immutable, prevalidated tablet write descriptor.
///
/// This is not a WAL record or consensus protocol. It is the admission
/// contract immediately above those layers: ownership epoch, predecessor
/// sequence, and key-range membership must all be valid before storage work is
/// scheduled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabletWrite {
    tablet_id: TabletId,
    ownership_epoch: u64,
    sequence: u64,
    expected_previous_sequence: u64,
    mutations: Vec<TabletMutation>,
}

impl TabletWrite {
    pub fn prepare(
        descriptor: &TabletDescriptor,
        presented_epoch: u64,
        expected_previous_sequence: u64,
        committed_sequence: u64,
        mutations: Vec<TabletMutation>,
    ) -> Result<Self, TabletError> {
        if presented_epoch < descriptor.ownership_epoch {
            return Err(TabletError::StaleEpoch {
                current: descriptor.ownership_epoch,
                presented: presented_epoch,
            });
        }
        if presented_epoch > descriptor.ownership_epoch {
            return Err(TabletError::UnknownEpoch {
                current: descriptor.ownership_epoch,
                presented: presented_epoch,
            });
        }
        if expected_previous_sequence != committed_sequence {
            return Err(TabletError::SequenceMismatch {
                committed: committed_sequence,
                expected_previous: expected_previous_sequence,
            });
        }
        if mutations
            .iter()
            .any(|mutation| !descriptor.range.contains(mutation.key()))
        {
            return Err(TabletError::KeyOutsideTabletRange);
        }

        let sequence = expected_previous_sequence
            .checked_add(1)
            .ok_or(TabletError::SequenceOverflow)?;

        Ok(Self {
            tablet_id: descriptor.id,
            ownership_epoch: descriptor.ownership_epoch,
            sequence,
            expected_previous_sequence,
            mutations,
        })
    }

    pub fn tablet_id(&self) -> TabletId {
        self.tablet_id
    }

    pub fn ownership_epoch(&self) -> u64 {
        self.ownership_epoch
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn expected_previous_sequence(&self) -> u64 {
        self.expected_previous_sequence
    }

    pub fn mutations(&self) -> &[TabletMutation] {
        &self.mutations
    }
}

/// One committed value version in the in-memory MVCC prototype.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct VersionedValue {
    pub(crate) sequence: u64,
    pub(crate) value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TabletSnapshotRow {
    pub(crate) key: Vec<u8>,
    pub(crate) versions: Vec<VersionedValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TabletSnapshotState {
    pub(crate) current_sequence: u64,
    // Defaults to zero when loading checkpoints written before MVCC retention.
    #[serde(default)]
    pub(crate) oldest_retained_snapshot: u64,
    pub(crate) rows: Vec<TabletSnapshotRow>,
}

/// Minimal single-node MVCC tablet used to prove transaction semantics before
/// introducing WAL and replication.
///
/// The structure is deliberately ordinary local computation rather than an
/// actor-per-key design. A future tablet actor can own this state while reads,
/// version lookup, and mutation application stay in the local hot path.
#[derive(Debug, Clone)]
pub struct MemoryTablet {
    descriptor: TabletDescriptor,
    current_sequence: u64,
    oldest_retained_snapshot: u64,
    rows: BTreeMap<Vec<u8>, Vec<VersionedValue>>,
}

impl MemoryTablet {
    pub fn new(descriptor: TabletDescriptor) -> Self {
        Self {
            descriptor,
            current_sequence: 0,
            oldest_retained_snapshot: 0,
            rows: BTreeMap::new(),
        }
    }

    pub fn descriptor(&self) -> &TabletDescriptor {
        &self.descriptor
    }

    pub fn current_sequence(&self) -> u64 {
        self.current_sequence
    }

    /// Earliest snapshot that this tablet still promises to serve.
    pub fn oldest_retained_snapshot(&self) -> u64 {
        self.oldest_retained_snapshot
    }

    pub(crate) fn snapshot_state(&self) -> TabletSnapshotState {
        TabletSnapshotState {
            current_sequence: self.current_sequence,
            oldest_retained_snapshot: self.oldest_retained_snapshot,
            rows: self
                .rows
                .iter()
                .map(|(key, versions)| TabletSnapshotRow {
                    key: key.clone(),
                    versions: versions.clone(),
                })
                .collect(),
        }
    }

    pub(crate) fn restore_snapshot(
        descriptor: TabletDescriptor,
        state: TabletSnapshotState,
    ) -> Result<Self, TabletError> {
        if state.oldest_retained_snapshot > state.current_sequence {
            return Err(TabletError::InvalidSnapshotHistory);
        }
        let mut rows = BTreeMap::new();
        for row in state.rows {
            if !descriptor.range.contains(&row.key) {
                return Err(TabletError::KeyOutsideTabletRange);
            }

            // Older checkpoints may contain multiple mutations of a key at
            // the same commit sequence. Coalesce these in original order so
            // the last mutation wins, as it did for snapshot reads.
            let mut previous = 0_u64;
            let mut versions: Vec<VersionedValue> = Vec::with_capacity(row.versions.len());
            for version in row.versions {
                if version.sequence == 0
                    || version.sequence > state.current_sequence
                    || version.sequence < previous
                {
                    return Err(TabletError::InvalidSnapshotHistory);
                }
                if version.sequence == previous {
                    *versions.last_mut().expect("a prior version exists") = version;
                } else {
                    previous = version.sequence;
                    versions.push(version);
                }
            }
            if rows.insert(row.key, versions).is_some() {
                return Err(TabletError::InvalidSnapshotHistory);
            }
        }

        Ok(Self {
            descriptor,
            current_sequence: state.current_sequence,
            oldest_retained_snapshot: state.oldest_retained_snapshot,
            rows,
        })
    }

    pub fn prepare_write(
        &self,
        presented_epoch: u64,
        expected_previous_sequence: u64,
        mutations: Vec<TabletMutation>,
    ) -> Result<TabletWrite, TabletError> {
        TabletWrite::prepare(
            &self.descriptor,
            presented_epoch,
            expected_previous_sequence,
            self.current_sequence,
            mutations,
        )
    }

    /// Validate a prepared write against the tablet's current ownership and
    /// committed tail without mutating state.
    pub(crate) fn validate_write(&self, write: &TabletWrite) -> Result<(), TabletError> {
        if write.tablet_id != self.descriptor.id {
            return Err(TabletError::WrongTablet {
                expected: self.descriptor.id,
                presented: write.tablet_id,
            });
        }
        if write.ownership_epoch < self.descriptor.ownership_epoch {
            return Err(TabletError::StaleEpoch {
                current: self.descriptor.ownership_epoch,
                presented: write.ownership_epoch,
            });
        }
        if write.ownership_epoch > self.descriptor.ownership_epoch {
            return Err(TabletError::UnknownEpoch {
                current: self.descriptor.ownership_epoch,
                presented: write.ownership_epoch,
            });
        }
        if write.expected_previous_sequence != self.current_sequence {
            return Err(TabletError::SequenceMismatch {
                committed: self.current_sequence,
                expected_previous: write.expected_previous_sequence,
            });
        }
        if write
            .mutations
            .iter()
            .any(|mutation| !self.descriptor.range.contains(mutation.key()))
        {
            return Err(TabletError::KeyOutsideTabletRange);
        }

        let next_sequence = self
            .current_sequence
            .checked_add(1)
            .ok_or(TabletError::SequenceOverflow)?;
        if write.sequence != next_sequence {
            return Err(TabletError::SequenceMismatch {
                committed: self.current_sequence,
                expected_previous: write.expected_previous_sequence,
            });
        }

        Ok(())
    }

    /// Atomically apply one prevalidated write to the in-memory MVCC state.
    ///
    /// All ownership, predecessor, and key-range checks happen before the
    /// first row version is appended, so a rejected batch leaves the tablet
    /// unchanged.
    pub fn commit(&mut self, write: TabletWrite) -> Result<u64, TabletError> {
        self.validate_write(&write)?;
        Ok(self.publish_validated(write))
    }

    /// Publish a write after `validate_write` has succeeded.
    ///
    /// This operation is intentionally infallible so a WAL-backed coordinator
    /// can perform all fallible validation before fsync, then publish the
    /// already-durable write without creating an ambiguous commit result.
    pub(crate) fn publish_validated(&mut self, write: TabletWrite) -> u64 {
        let sequence = write.sequence;
        self.apply_mutations(sequence, write.mutations);
        sequence
    }

    /// Replay one already checksummed/validated WAL record into MVCC state.
    ///
    /// WAL replay intentionally does not re-check historical ownership epochs:
    /// an older owner epoch is valid history. It does re-check sequence order
    /// and current tablet range before mutating state.
    pub(crate) fn replay_committed(
        &mut self,
        sequence: u64,
        expected_previous_sequence: u64,
        mutations: Vec<TabletMutation>,
    ) -> Result<(), TabletError> {
        if expected_previous_sequence != self.current_sequence {
            return Err(TabletError::SequenceMismatch {
                committed: self.current_sequence,
                expected_previous: expected_previous_sequence,
            });
        }
        let expected_sequence = self
            .current_sequence
            .checked_add(1)
            .ok_or(TabletError::SequenceOverflow)?;
        if sequence != expected_sequence {
            return Err(TabletError::RecoveredSequenceMismatch {
                expected: expected_sequence,
                presented: sequence,
            });
        }
        if mutations
            .iter()
            .any(|mutation| !self.descriptor.range.contains(mutation.key()))
        {
            return Err(TabletError::KeyOutsideTabletRange);
        }

        self.apply_mutations(sequence, mutations);
        Ok(())
    }

    fn apply_mutations(&mut self, sequence: u64, mutations: Vec<TabletMutation>) {
        // A transaction exposes only its final mutation for each key. This
        // also normalizes old WAL records that contain repeated keys, keeping
        // checkpoints strictly increasing by sequence for every row.
        let mut final_values = BTreeMap::new();
        for mutation in mutations {
            match mutation {
                TabletMutation::Put { key, value } => {
                    final_values.insert(key, Some(value));
                }
                TabletMutation::Delete { key } => {
                    final_values.insert(key, None);
                }
            }
        }
        for (key, value) in final_values {
            self.rows
                .entry(key)
                .or_default()
                .push(VersionedValue { sequence, value });
        }
        self.current_sequence = sequence;
    }

    /// Discard superseded versions before an explicit, caller-approved snapshot
    /// floor. There is no snapshot registry yet: callers MUST ensure that no
    /// active reader, backup, or other consumer needs an older snapshot.
    pub fn prune_history_before(&mut self, floor: u64) -> Result<(), TabletError> {
        if floor < self.oldest_retained_snapshot {
            return Err(TabletError::RetentionFloorRegression {
                requested: floor,
                oldest_retained: self.oldest_retained_snapshot,
            });
        }
        if floor > self.current_sequence {
            return Err(TabletError::SnapshotAhead {
                committed: self.current_sequence,
                requested: floor,
            });
        }

        for versions in self.rows.values_mut() {
            let boundary = versions.partition_point(|version| version.sequence <= floor);
            if boundary != 0 {
                // Keep one anchor value visible at the floor. A tombstone
                // needs no anchor: absence is the default for earlier reads.
                let discard = if versions[boundary - 1].value.is_some() {
                    boundary - 1
                } else {
                    boundary
                };
                versions.drain(..discard);
            }
        }
        self.rows.retain(|_, versions| !versions.is_empty());
        self.oldest_retained_snapshot = floor;
        Ok(())
    }

    /// Read one key at an already committed snapshot sequence.
    pub fn read_at(&self, key: &[u8], snapshot: u64) -> Result<Option<&[u8]>, TabletError> {
        if snapshot > self.current_sequence {
            return Err(TabletError::SnapshotAhead {
                committed: self.current_sequence,
                requested: snapshot,
            });
        }
        if snapshot < self.oldest_retained_snapshot {
            return Err(TabletError::SnapshotExpired {
                requested: snapshot,
                oldest_retained: self.oldest_retained_snapshot,
            });
        }
        if !self.descriptor.range.contains(key) {
            return Err(TabletError::KeyOutsideTabletRange);
        }

        Ok(self.rows.get(key).and_then(|versions| {
            versions
                .iter()
                .rev()
                .find(|version| version.sequence <= snapshot)
                .and_then(|version| version.value.as_deref())
        }))
    }

    /// Read the newest committed value. Out-of-range keys route as absent;
    /// callers that need a routing error can use `read_at`.
    pub fn read_latest(&self, key: &[u8]) -> Option<&[u8]> {
        self.read_at(key, self.current_sequence).ok().flatten()
    }
}

/// Invariant failures detected before a tablet operation reaches storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabletError {
    InvalidTabletId,
    InvalidKeyRange,
    InvalidOwnershipEpoch,
    SplitKeyOutsideInterior,
    WrongTablet {
        expected: TabletId,
        presented: TabletId,
    },
    DuplicateChildTablet,
    EpochNotAdvanced {
        current: u64,
        proposed: u64,
    },
    StaleEpoch {
        current: u64,
        presented: u64,
    },
    UnknownEpoch {
        current: u64,
        presented: u64,
    },
    SequenceMismatch {
        committed: u64,
        expected_previous: u64,
    },
    SequenceOverflow,
    RecoveredSequenceMismatch {
        expected: u64,
        presented: u64,
    },
    SnapshotAhead {
        committed: u64,
        requested: u64,
    },
    SnapshotExpired {
        requested: u64,
        oldest_retained: u64,
    },
    RetentionFloorRegression {
        requested: u64,
        oldest_retained: u64,
    },
    InvalidSnapshotHistory,
    KeyOutsideTabletRange,
}

impl fmt::Display for TabletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTabletId => f.write_str("tablet id must be non-zero"),
            Self::InvalidKeyRange => f.write_str("tablet key range must have start < end"),
            Self::InvalidOwnershipEpoch => f.write_str("tablet ownership epoch must be non-zero"),
            Self::SplitKeyOutsideInterior => {
                f.write_str("tablet split key must be strictly inside the source range")
            }
            Self::WrongTablet {
                expected,
                presented,
            } => write!(
                f,
                "tablet write targets tablet {}; expected {}",
                presented.get(),
                expected.get()
            ),
            Self::DuplicateChildTablet => f.write_str("tablet split children must have distinct ids"),
            Self::EpochNotAdvanced { current, proposed } => write!(
                f,
                "tablet split epoch {proposed} must be newer than current epoch {current}"
            ),
            Self::StaleEpoch { current, presented } => write!(
                f,
                "stale tablet ownership epoch {presented}; current epoch is {current}"
            ),
            Self::UnknownEpoch { current, presented } => write!(
                f,
                "unrecognized future tablet ownership epoch {presented}; current epoch is {current}"
            ),
            Self::SequenceMismatch {
                committed,
                expected_previous,
            } => write!(
                f,
                "tablet predecessor {expected_previous} does not match committed sequence {committed}"
            ),
            Self::SequenceOverflow => f.write_str("tablet sequence overflow"),
            Self::RecoveredSequenceMismatch {
                expected,
                presented,
            } => write!(
                f,
                "recovered tablet sequence {presented} does not match expected sequence {expected}"
            ),
            Self::SnapshotAhead {
                committed,
                requested,
            } => write!(
                f,
                "snapshot {requested} is ahead of committed tablet sequence {committed}"
            ),
            Self::SnapshotExpired {
                requested,
                oldest_retained,
            } => write!(
                f,
                "snapshot {requested} was reclaimed; oldest retained snapshot is {oldest_retained}"
            ),
            Self::RetentionFloorRegression {
                requested,
                oldest_retained,
            } => write!(
                f,
                "retention floor {requested} is below current floor {oldest_retained}"
            ),
            Self::InvalidSnapshotHistory => {
                f.write_str("tablet snapshot contains invalid MVCC version history")
            }
            Self::KeyOutsideTabletRange => {
                f.write_str("tablet mutation key falls outside the owned key range")
            }
        }
    }
}

impl std::error::Error for TabletError {}

#[cfg(test)]
mod mvcc_retention_tests {
    use super::*;

    #[test]
    fn pruning_discards_old_values_and_obsolete_tombstones() {
        let descriptor = TabletDescriptor::new(
            TabletId::new(91).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            1,
        )
        .unwrap();
        let mut tablet = MemoryTablet::new(descriptor);
        let batches = [
            vec![
                TabletMutation::Put { key: b"k".to_vec(), value: b"v1".to_vec() },
                TabletMutation::Put { key: b"gone".to_vec(), value: b"x".to_vec() },
            ],
            vec![
                TabletMutation::Put { key: b"k".to_vec(), value: b"v2".to_vec() },
                TabletMutation::Delete { key: b"gone".to_vec() },
            ],
            vec![TabletMutation::Put { key: b"k".to_vec(), value: b"v3".to_vec() }],
        ];
        for batch in batches {
            let previous = tablet.current_sequence();
            let write = tablet.prepare_write(1, previous, batch).unwrap();
            tablet.commit(write).unwrap();
        }

        assert_eq!(tablet.rows.get(b"k".as_slice()).unwrap().len(), 3);
        assert_eq!(tablet.rows.get(b"gone".as_slice()).unwrap().len(), 2);
        tablet.prune_history_before(2).unwrap();

        assert_eq!(tablet.rows.get(b"k".as_slice()).unwrap().len(), 2);
        assert!(!tablet.rows.contains_key(b"gone".as_slice()));
        assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
        assert_eq!(tablet.read_at(b"k", 3).unwrap(), Some(&b"v3"[..]));
        assert_eq!(
            tablet.read_at(b"k", 1),
            Err(TabletError::SnapshotExpired { requested: 1, oldest_retained: 2 })
        );
    }
}
