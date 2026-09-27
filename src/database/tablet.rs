//! Range-tablet ownership and write-fencing primitives.
//!
//! This module is deliberately storage-engine agnostic. It defines the
//! invariants a future NuDB storage engine can enforce before mapping accepted
//! writes onto WAL/MVCC/Raft machinery.

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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Invariant failures detected before a tablet operation reaches storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabletError {
    InvalidTabletId,
    InvalidKeyRange,
    InvalidOwnershipEpoch,
    SplitKeyOutsideInterior,
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
            Self::KeyOutsideTabletRange => {
                f.write_str("tablet mutation key falls outside the owned key range")
            }
        }
    }
}

impl std::error::Error for TabletError {}
