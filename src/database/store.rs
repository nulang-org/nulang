//! WAL-backed single-node tablet coordinator.
//!
//! This type enforces the first NuDB durability ordering contract:
//!
//! ```text
//! validate write
//!     -> append + sync WAL
//!     -> publish MVCC state
//! ```
//!
//! No fallible validation is performed after the WAL acknowledges durability.

use std::fmt;
use std::path::{Path, PathBuf};

use super::checkpoint::{self, CheckpointError};
use super::ownership::{NodeId, OwnershipError, OwnershipStateMachine};
use super::tablet::{
    transaction_digest, MemoryTablet, TabletDescriptor, TabletError, TabletMutation, TabletWrite,
    TransactionId, TransactionIdentity, TransactionReceipt,
};
use super::wal::{FileWal, WalError};

#[derive(Debug)]
pub struct WalBackedTablet {
    tablet: MemoryTablet,
    wal: FileWal,
    checkpoint_path: PathBuf,
}

impl WalBackedTablet {
    /// Open a tablet and reconstruct its MVCC state from the valid WAL prefix.
    pub fn open(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
    ) -> Result<Self, WalBackedError> {
        let wal_path = wal_path.as_ref();
        let checkpoint_path = checkpoint::checkpoint_path_for_wal(wal_path);
        let wal = FileWal::open(wal_path)?;
        let mut tablet = match checkpoint::load_checkpoint(&checkpoint_path, descriptor.clone())? {
            Some(tablet) => tablet,
            None => wal.recover_memory_tablet(descriptor)?,
        };
        wal.replay_after_checkpoint(&mut tablet)?;
        Ok(Self {
            tablet,
            wal,
            checkpoint_path,
        })
    }

    pub fn descriptor(&self) -> &TabletDescriptor {
        self.tablet.descriptor()
    }

    pub fn current_sequence(&self) -> u64 {
        self.tablet.current_sequence()
    }

    /// Commit only for the node+epoch selected by an already-applied
    /// consensus ownership log. The data WAL *also* checks its local durable
    /// epoch. Callers must supply a state machine built from verified committed
    /// entries, never uncommitted proposals.
    ///
    /// This protects the local RPC dispatch boundary; it does not replicate
    /// data writes or make the control-plane and data-plane logs atomic.
    pub fn commit_with_ownership(
        &mut self,
        authority: &OwnershipStateMachine,
        node: NodeId,
        epoch: u64,
        id: TransactionId,
        mutations: Vec<TabletMutation>,
    ) -> Result<TransactionReceipt, WalBackedError> {
        authority.authorize_writer(self.tablet.descriptor().id(), node, epoch)?;
        self.commit_idempotent_at_epoch(epoch, id, mutations)
    }

    /// Install an already committed leadership handoff into the local WAL.
    /// A newer authoritative epoch must be durably fenced locally before
    /// this tablet accepts writes from the new owner.
    ///
    /// Errors after the WAL append begins can be ambiguous; close/reopen
    /// with the intended descriptor epoch and reconcile from the WAL.
    pub fn install_committed_ownership(
        &mut self,
        authority: &OwnershipStateMachine,
        node: NodeId,
    ) -> Result<u64, WalBackedError> {
        let tablet_id = self.tablet.descriptor().id();
        let grant = authority
            .grant(tablet_id)
            .ok_or(OwnershipError::UnknownTablet { tablet: tablet_id })?;
        authority.authorize_writer(tablet_id, node, grant.epoch)?;
        let current = self.tablet.descriptor().ownership_epoch();
        if grant.epoch < current {
            return Err(TabletError::StaleEpoch {
                current,
                presented: grant.epoch,
            }
            .into());
        }
        if grant.epoch == current {
            return Ok(self.tablet.current_sequence());
        }
        self.advance_ownership_epoch(current, grant.epoch)
    }

    /// Append an explicit durable ownership epoch barrier. A barrier consumes
    /// one committed sequence but changes no user rows or transaction receipts.
    /// This is local fencing only: the caller must obtain its new epoch from a
    /// trusted coordinator before invoking it. This API does not establish
    /// distributed leader authority or invalidate an existing remote writer.
    ///
    /// An I/O error may leave an ambiguous durable barrier. Drop the poisoned
    /// handle, reopen with the intended epoch, and reconcile its sequence.
    pub fn advance_ownership_epoch(
        &mut self,
        expected_epoch: u64,
        new_epoch: u64,
    ) -> Result<u64, WalBackedError> {
        let current = self.tablet.descriptor().ownership_epoch();
        if expected_epoch < current {
            return Err(TabletError::StaleEpoch {
                current,
                presented: expected_epoch,
            }
            .into());
        }
        if expected_epoch > current {
            return Err(TabletError::UnknownEpoch {
                current,
                presented: expected_epoch,
            }
            .into());
        }
        let write = TabletWrite::prepare_epoch_fence(
            self.tablet.descriptor(),
            new_epoch,
            self.tablet.current_sequence(),
        )?;
        let sequence = write.sequence();
        self.wal.append_write(&write)?;
        self.tablet.publish_epoch_fence(new_epoch, sequence);
        Ok(sequence)
    }

    pub fn prepare_write(
        &self,
        presented_epoch: u64,
        expected_previous_sequence: u64,
        mutations: Vec<TabletMutation>,
    ) -> Result<TabletWrite, TabletError> {
        self.tablet
            .prepare_write(presented_epoch, expected_previous_sequence, mutations)
    }

    /// Commit an identified transaction only if the caller holds the current
    /// tablet ownership epoch. Remote APIs must use this path instead of
    /// commit_idempotent to reject requests carrying stale fencing tokens.
    pub fn commit_idempotent_at_epoch(
        &mut self,
        presented_epoch: u64,
        id: TransactionId,
        mutations: Vec<TabletMutation>,
    ) -> Result<TransactionReceipt, WalBackedError> {
        let current = self.tablet.descriptor().ownership_epoch();
        if presented_epoch < current {
            return Err(TabletError::StaleEpoch {
                current,
                presented: presented_epoch,
            }
            .into());
        }
        if presented_epoch > current {
            return Err(TabletError::UnknownEpoch {
                current,
                presented: presented_epoch,
            }
            .into());
        }
        self.commit_idempotent(id, mutations)
    }

    /// Commit an identified transaction, or return its original receipt if
    /// the same ordered mutations already committed on this tablet.
    ///
    /// A conflicting reuse of the ID is rejected without appending to the WAL.
    /// The receipt is part of the same WAL record and checkpoint as the data,
    /// so it survives restart and log reclamation. A failed append may have
    /// committed; reopen the tablet and retry with the exact same ID/payload.
    ///
    /// Only one writer may hold the local WAL lock at a time. Identifiers are
    /// scoped to one tablet. This local convenience method has no client epoch
    /// argument and must not be exposed directly as a remote write endpoint;
    /// use commit_idempotent_at_epoch for caller-presented fencing tokens.
    pub fn commit_idempotent(
        &mut self,
        id: TransactionId,
        mutations: Vec<TabletMutation>,
    ) -> Result<TransactionReceipt, WalBackedError> {
        let digest = transaction_digest(&mutations);
        if let Some(receipt) = self.tablet.transaction_receipt(id) {
            if receipt.digest() != digest {
                return Err(WalBackedError::IdempotencyConflict(id));
            }
            return Ok(receipt);
        }

        let write = self.tablet.prepare_write(
            self.tablet.descriptor().ownership_epoch(),
            self.tablet.current_sequence(),
            mutations,
        )?;
        let write = write.with_identity(TransactionIdentity { id, digest });
        let sequence = self.commit(write)?;
        // This lookup is infallible after a successful durable commit.
        let receipt = self.tablet.transaction_receipt(id).expect(
            "committed transaction identity must be recorded atomically",
        );
        debug_assert_eq!(receipt.sequence(), sequence);
        Ok(receipt)
    }

    /// Durably commit one write before making it visible to readers.
    pub fn commit(&mut self, write: TabletWrite) -> Result<u64, WalBackedError> {
        self.tablet.validate_write(&write)?;
        self.wal.append_write(&write)?;
        Ok(self.tablet.publish_validated(write))
    }

    pub fn read_at(&self, key: &[u8], snapshot: u64) -> Result<Option<&[u8]>, TabletError> {
        self.tablet.read_at(key, snapshot)
    }

    pub fn read_latest(&self, key: &[u8]) -> Option<&[u8]> {
        self.tablet.read_latest(key)
    }

    /// Atomically publish a checkpoint without reclaiming the WAL.
    ///
    /// This is a valid crash state and is intentionally public so operators can
    /// separate checkpoint publication from later space reclamation.
    pub fn publish_checkpoint(&self) -> Result<(), WalBackedError> {
        checkpoint::write_checkpoint(&self.checkpoint_path, &self.tablet)?;
        Ok(())
    }

    /// Publish a durable checkpoint, then reclaim the WAL through that exact
    /// committed sequence. The ordering is safety-critical: if reclamation
    /// fails, the already-published checkpoint plus the full WAL remain a valid
    /// recovery state.
    pub fn checkpoint(&mut self) -> Result<(), WalBackedError> {
        self.publish_checkpoint()?;
        self.wal.reclaim_through(self.tablet.current_sequence())?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalBackedError {
    Tablet(TabletError),
    Wal(WalError),
    Checkpoint(CheckpointError),
    IdempotencyConflict(TransactionId),
    Ownership(OwnershipError),
}

impl From<TabletError> for WalBackedError {
    fn from(error: TabletError) -> Self {
        Self::Tablet(error)
    }
}

impl From<WalError> for WalBackedError {
    fn from(error: WalError) -> Self {
        Self::Wal(error)
    }
}

impl From<OwnershipError> for WalBackedError {
    fn from(error: OwnershipError) -> Self {
        Self::Ownership(error)
    }
}

impl From<CheckpointError> for WalBackedError {
    fn from(error: CheckpointError) -> Self {
        Self::Checkpoint(error)
    }
}

impl fmt::Display for WalBackedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tablet(error) => write!(f, "tablet commit rejected: {error}"),
            Self::Wal(error) => write!(f, "tablet WAL failure: {error}"),
            Self::Checkpoint(error) => write!(f, "tablet checkpoint failure: {error}"),
            Self::IdempotencyConflict(id) => {
                write!(f, "transaction ID {id:?} reused with different mutations")
            },
            Self::Ownership(error) => write!(f, "tablet ownership rejected: {error}"),
        }
    }
}

impl std::error::Error for WalBackedError {}
