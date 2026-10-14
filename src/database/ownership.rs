//! Deterministic NuDB tablet-ownership log state machine.
//!
//! This module **only applies entries already committed by an external
//! consensus implementation**. It has no Raft transport, voting, log storage,
//! quorum verification, lease clock, or cryptographic authorization. Calling
//! `apply_committed` with a locally proposed/unverified entry is unsafe.
//!
//! A real control plane must durably replicate the log and replay committed
//! entries in order before distributing grants. Data-plane WAL fencing and
//! replicated data durability remain separate invariants.

use std::collections::BTreeMap;
use std::fmt;

use super::tablet::TabletId;

/// Nonzero identity of a NuDB storage node; never reuse for a different node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(u64);

impl NodeId {
    pub fn new(value: u64) -> Result<Self, OwnershipError> {
        if value == 0 {
            return Err(OwnershipError::InvalidNodeId);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Command selected by the external consensus control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipCommand {
    Bootstrap {
        tablet: TabletId,
        owner: NodeId,
        epoch: u64,
    },
    Transfer {
        tablet: TabletId,
        expected_owner: NodeId,
        expected_epoch: u64,
        next_owner: NodeId,
        next_epoch: u64,
    },
}

impl OwnershipCommand {
    fn tablet(&self) -> TabletId {
        match *self {
            Self::Bootstrap { tablet, .. } | Self::Transfer { tablet, .. } => tablet,
        }
    }
}

/// A finalized, ordered log entry supplied by a trusted consensus adapter.
///
/// Merely constructing this value does NOT constitute quorum commitment.
/// This type exists to isolate the deterministic *apply* boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedOwnershipEntry {
    pub index: u64,
    pub term: u64,
    pub command: OwnershipCommand,
}

/// A materialized grant from the last applied committed log entry.
///
/// An RPC should validate owner and epoch against current committed metadata;
/// holding an old grant alone cannot authorize an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnershipGrant {
    pub tablet: TabletId,
    pub owner: NodeId,
    pub epoch: u64,
    pub committed_index: u64,
    pub committed_term: u64,
}

/// Pure state machine for a replayable external consensus ownership log.
///
/// This is an in-memory materialization of authoritative committed metadata.
/// Its durability comes only from a separately implemented consensus log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OwnershipStateMachine {
    applied_index: u64,
    applied_term: u64,
    last_entry: Option<CommittedOwnershipEntry>,
    owners: BTreeMap<TabletId, OwnershipGrant>,
}

impl OwnershipStateMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn applied_index(&self) -> u64 {
        self.applied_index
    }

    pub fn applied_term(&self) -> u64 {
        self.applied_term
    }

    pub fn grant(&self, tablet: TabletId) -> Option<OwnershipGrant> {
        self.owners.get(&tablet).copied()
    }

    /// Apply the next *already committed* entry atomically to the state
    /// machine. Invalid entries do not consume an index or mutate any grant.
    ///
    /// The exact latest entry is idempotent to support replay after a lost
    /// apply acknowledgement. Earlier indexes and same-index conflicts fail.
    pub fn apply_committed(
        &mut self,
        entry: CommittedOwnershipEntry,
    ) -> Result<OwnershipGrant, OwnershipError> {
        if entry.index == self.applied_index && self.applied_index != 0 {
            if self.last_entry.as_ref() == Some(&entry) {
                return self.grant(entry.command.tablet()).ok_or(
                    OwnershipError::UnknownTablet {
                        tablet: entry.command.tablet(),
                    },
                );
            }
            return Err(OwnershipError::ConflictingCommittedEntry {
                index: entry.index,
            });
        }
        let expected = self
            .applied_index
            .checked_add(1)
            .ok_or(OwnershipError::InvalidLogPosition)?;
        if entry.index < expected {
            return Err(OwnershipError::OldLogIndex {
                applied: self.applied_index,
                presented: entry.index,
            });
        }
        if entry.index > expected {
            return Err(OwnershipError::LogGap {
                expected,
                presented: entry.index,
            });
        }
        if entry.term == 0 {
            return Err(OwnershipError::InvalidTerm);
        }
        if entry.term < self.applied_term {
            return Err(OwnershipError::TermRegression {
                committed: self.applied_term,
                presented: entry.term,
            });
        }

        // Fully validate against the last committed state before mutation.
        let grant = match entry.command {
            OwnershipCommand::Bootstrap {
                tablet,
                owner,
                epoch,
            } => {
                if epoch == 0 {
                    return Err(OwnershipError::InvalidEpoch);
                }
                if self.owners.contains_key(&tablet) {
                    return Err(OwnershipError::AlreadyBootstrapped { tablet });
                }
                OwnershipGrant {
                    tablet,
                    owner,
                    epoch,
                    committed_index: entry.index,
                    committed_term: entry.term,
                }
            }
            OwnershipCommand::Transfer {
                tablet,
                expected_owner,
                expected_epoch,
                next_owner,
                next_epoch,
            } => {
                let current = self
                    .grant(tablet)
                    .ok_or(OwnershipError::UnknownTablet { tablet })?;
                if expected_epoch < current.epoch {
                    return Err(OwnershipError::StaleEpoch {
                        committed: current.epoch,
                        presented: expected_epoch,
                    });
                }
                if expected_epoch > current.epoch {
                    return Err(OwnershipError::FutureEpoch {
                        committed: current.epoch,
                        presented: expected_epoch,
                    });
                }
                if expected_owner != current.owner {
                    return Err(OwnershipError::WrongOwner {
                        committed: current.owner,
                        presented: expected_owner,
                    });
                }
                if next_epoch <= current.epoch {
                    return Err(OwnershipError::EpochNotAdvanced {
                        committed: current.epoch,
                        proposed: next_epoch,
                    });
                }
                OwnershipGrant {
                    tablet,
                    owner: next_owner,
                    epoch: next_epoch,
                    committed_index: entry.index,
                    committed_term: entry.term,
                }
            }
        };

        self.owners.insert(grant.tablet, grant);
        self.applied_index = entry.index;
        self.applied_term = entry.term;
        self.last_entry = Some(entry);
        Ok(grant)
    }

    /// Check a caller's current write-fencing token, *including* owner.
    /// This check belongs before RPC dispatch and is not a substitute for
    /// checking the tablet's own durable epoch at the WAL commit boundary.
    pub fn authorize_writer(
        &self,
        tablet: TabletId,
        presented_owner: NodeId,
        presented_epoch: u64,
    ) -> Result<(), OwnershipError> {
        let grant = self
            .grant(tablet)
            .ok_or(OwnershipError::UnknownTablet { tablet })?;
        if presented_epoch < grant.epoch {
            return Err(OwnershipError::StaleEpoch {
                committed: grant.epoch,
                presented: presented_epoch,
            });
        }
        if presented_epoch > grant.epoch {
            return Err(OwnershipError::FutureEpoch {
                committed: grant.epoch,
                presented: presented_epoch,
            });
        }
        if presented_owner != grant.owner {
            return Err(OwnershipError::WrongOwner {
                committed: grant.owner,
                presented: presented_owner,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipError {
    InvalidNodeId,
    InvalidEpoch,
    InvalidTerm,
    InvalidLogPosition,
    OldLogIndex { applied: u64, presented: u64 },
    LogGap { expected: u64, presented: u64 },
    TermRegression { committed: u64, presented: u64 },
    ConflictingCommittedEntry { index: u64 },
    UnknownTablet { tablet: TabletId },
    AlreadyBootstrapped { tablet: TabletId },
    StaleEpoch { committed: u64, presented: u64 },
    FutureEpoch { committed: u64, presented: u64 },
    WrongOwner { committed: NodeId, presented: NodeId },
    EpochNotAdvanced { committed: u64, proposed: u64 },
}

impl fmt::Display for OwnershipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for OwnershipError {}
