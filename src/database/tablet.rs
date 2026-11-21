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
    rows: BTreeMap<Vec<u8>, Vec<VersionedValue>>,
}

impl MemoryTablet {
    pub fn new(descriptor: TabletDescriptor) -> Self {
        Self {
            descriptor,
            current_sequence: 0,
            rows: BTreeMap::new(),
        }
    }

    pub fn descriptor(&self) -> &TabletDescriptor {
        &self.descriptor
    }

    pub fn current_sequence(&self) -> u64 {
        self.current_sequence
    }

    pub(crate) fn snapshot_state(&self) -> TabletSnapshotState {
        TabletSnapshotState {
            current_sequence: self.current_sequence,
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
        let mut rows = BTreeMap::new();
        for row in state.rows {
            if !descriptor.range.contains(&row.key) {
                return Err(TabletError::KeyOutsideTabletRange);
            }

            let mut previous = 0_u64;
            for version in &row.versions {
                if version.sequence == 0
                    || version.sequence > state.current_sequence
                    || version.sequence <= previous
                {
                    return Err(TabletError::InvalidSnapshotHistory);
                }
                previous = version.sequence;
            }
            if rows.insert(row.key, row.versions).is_some() {
                return Err(TabletError::InvalidSnapshotHistory);
            }
        }

        Ok(Self {
            descriptor,
            current_sequence: state.current_sequence,
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
        for mutation in mutations {
            let (key, value) = match mutation {
                TabletMutation::Put { key, value } => (key, Some(value)),
                TabletMutation::Delete { key } => (key, None),
            };
            let versions = self.rows.entry(key).or_default();

            // A commit has one snapshot sequence, even if its batch updates
            // the same key repeatedly. Preserve the last mutation's value
            // without creating duplicate MVCC versions that checkpoint
            // recovery would reject as invalid history.
            if let Some(last) = versions.last_mut() {
                if last.sequence == sequence {
                    last.value = value;
                    continue;
                }
            }
            versions.push(VersionedValue { sequence, value });
        }
        self.current_sequence = sequence;
    }

    /// Read one key at an already committed snapshot sequence.
    pub fn read_at(&self, key: &[u8], snapshot: u64) -> Result<Option<&[u8]>, TabletError> {
        if snapshot > self.current_sequence {
            return Err(TabletError::SnapshotAhead {
                committed: self.current_sequence,
                requested: snapshot,
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
