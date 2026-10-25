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

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use super::checkpoint::{self, CheckpointError};
use super::tablet::{
    transaction_digest, MemoryTablet, TabletDescriptor, TabletError, TabletMutation, TabletWrite,
    TransactionId, TransactionIdentity, TransactionReceipt,
};
use super::wal::{FileWal, WalError, WalReadOnlyReport};

const MAX_RECEIPT_GROUP_TRANSACTIONS: usize = 64;

/// Read-only structural consistency report for a checkpoint/WAL pair.
///
/// A passing result validates formats, checksums, metadata, chain ordering,
/// and checkpoint sequence bounds; it is NOT a verified online snapshot,
/// byte-for-byte WAL/checkpoint overlap proof, or a successful restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointWalReport {
    pub checkpoint_sequence: Option<u64>,
    pub durable_tail: u64,
    pub wal: WalReadOnlyReport,
}

#[derive(Debug)]
pub struct WalBackedTablet {
    tablet: MemoryTablet,
    wal: FileWal,
    checkpoint_path: PathBuf,
}

impl WalBackedTablet {
    /// Inspect the checkpoint and WAL without creating, truncating, or
    /// repairing either source file or acquiring a mutable writer lock.
    ///
    /// This is a structural diagnostic, not a consistent online backup.
    /// Run against quiescent files or a separately established immutable,
    /// atomic snapshot; simultaneous checkpoint publication/WAL reclamation
    /// can otherwise yield a transient inconsistent observation.
    ///
    /// For recovery proof, copy both files into a disposable directory,
    /// reopen that copy with `WalBackedTablet::open`, and compare the
    /// recovered state and receipts against an independent reference.
    pub fn verify_checkpoint_wal_read_only(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
    ) -> Result<CheckpointWalReport, WalBackedError> {
        let wal_path = wal_path.as_ref();
        // Never use FileWal::open on the source: opening may create an empty
        // WAL, acquire/create a lock, or truncate an incomplete crash tail.
        let wal = FileWal::verify_read_only(wal_path)?;

        if let Some(tablet_id) = wal.tablet_id {
            if tablet_id != descriptor.id() {
                return Err(WalError::TabletMismatch {
                    expected: descriptor.id(),
                    presented: tablet_id,
                }
                .into());
            }
        }
        if let Some(epoch) = wal.last_ownership_epoch {
            if descriptor.ownership_epoch() < epoch {
                return Err(WalError::StaleOwnershipEpoch {
                    durable: epoch,
                    presented: descriptor.ownership_epoch(),
                }
                .into());
            }
        }

        let checkpoint_path = checkpoint::checkpoint_path_for_wal(wal_path);
        let checkpoint_sequence = checkpoint::load_checkpoint(&checkpoint_path, descriptor)?
            .as_ref()
            .map(MemoryTablet::current_sequence);

        match checkpoint_sequence {
            Some(sequence) if sequence < wal.base_sequence => {
                return Err(WalError::CheckpointTooOld {
                    checkpoint: sequence,
                    wal_base: wal.base_sequence,
                }
                .into());
            }
            Some(sequence) if sequence > wal.last_sequence => {
                return Err(WalError::CheckpointAheadOfWal {
                    checkpoint: sequence,
                    wal_tail: wal.last_sequence,
                }
                .into());
            }
            None if wal.base_sequence != 0 => {
                return Err(WalError::CheckpointRequired {
                    base_sequence: wal.base_sequence,
                }
                .into());
            }
            _ => {}
        }

        Ok(CheckpointWalReport {
            checkpoint_sequence,
            durable_tail: wal.last_sequence,
            wal,
        })
    }

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

    pub fn prepare_write(
        &self,
        presented_epoch: u64,
        expected_previous_sequence: u64,
        mutations: Vec<TabletMutation>,
    ) -> Result<TabletWrite, TabletError> {
        self.tablet
            .prepare_write(presented_epoch, expected_previous_sequence, mutations)
    }

    /// Commit an identified transaction, or return its original receipt if
    /// the same ordered mutations already committed on this tablet.
    ///
    /// A conflicting reuse of the ID is rejected without appending to the WAL.
    /// The receipt is part of the same WAL record and checkpoint as the data,
    /// so it survives restart and log reclamation. A failed append may have
    /// committed; reopen the tablet and retry with the exact same ID/payload.
    ///
    /// FileWal excludes concurrent local writers to the same canonical WAL.
    /// This is not distributed lease fencing. IDs are scoped to one tablet.
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
        let receipt = self
            .tablet
            .transaction_receipt(id)
            .expect("committed transaction identity must be recorded atomically");
        debug_assert_eq!(receipt.sequence(), sequence);
        Ok(receipt)
    }

    /// Commit distinct identified transactions with one bounded WAL fsync.
    ///
    /// Existing matching receipts are returned without reappending. The whole
    /// request is prevalidated before any file mutation; new records are
    /// published to MVCC only after the shared durability barrier succeeds.
    ///
    /// A failed WAL write can leave a recoverable prefix, so the caller must
    /// reopen and retry with these *same* IDs and mutations. This is not an
    /// atomic all-or-nothing multi-transaction operation after a crash.
    pub fn commit_batch_idempotent(
        &mut self,
        entries: Vec<(TransactionId, Vec<TabletMutation>)>,
    ) -> Result<Vec<TransactionReceipt>, WalBackedError> {
        // Bound the *request*, not just new WAL records. A replay-only batch
        // must not bypass admission limits and allocate an unbounded dedup set.
        if entries.len() > MAX_RECEIPT_GROUP_TRANSACTIONS {
            return Err(WalBackedError::TooManyReceiptTransactions {
                requested: entries.len(),
                limit: MAX_RECEIPT_GROUP_TRANSACTIONS,
            });
        }
        let mut seen = HashSet::with_capacity(entries.len());
        let mut ids = Vec::with_capacity(entries.len());
        let mut pending = Vec::new();
        let descriptor = self.tablet.descriptor().clone();
        let mut next_sequence = self.tablet.current_sequence();

        for (id, mutations) in entries {
            if !seen.insert(id) {
                return Err(WalBackedError::DuplicateBatchId(id));
            }
            ids.push(id);
            let digest = transaction_digest(&mutations);
            if let Some(receipt) = self.tablet.transaction_receipt(id) {
                if receipt.digest() != digest {
                    return Err(WalBackedError::IdempotencyConflict(id));
                }
                continue;
            }
            let write = TabletWrite::prepare(
                &descriptor,
                descriptor.ownership_epoch(),
                next_sequence,
                next_sequence,
                mutations,
            )?
            .with_identity(TransactionIdentity { id, digest });
            next_sequence = write.sequence();
            pending.push(write);
        }

        if !pending.is_empty() {
            self.wal.append_batch(&pending)?;
            for write in pending {
                self.tablet.publish_validated(write);
            }
        }

        Ok(ids
            .into_iter()
            .map(|id| {
                self.tablet
                    .transaction_receipt(id)
                    .expect("durable group commit must publish every identified receipt")
            })
            .collect())
    }

    /// Durably commit one write before making it visible to readers.
    pub fn commit(&mut self, write: TabletWrite) -> Result<u64, WalBackedError> {
        self.tablet.validate_write(&write)?;
        self.wal.append_write(&write)?;
        Ok(self.tablet.publish_validated(write))
    }

    /// Durably commit a consecutive group of independent writes with one WAL
    /// synchronization boundary, then publish them to MVCC state in order.
    ///
    /// The complete group is validated before any WAL bytes are emitted. Once
    /// the WAL acknowledges durability, publication is infallible. An I/O
    /// error during the append can leave an ambiguous durable prefix, so the
    /// poisoned WAL contract requires reopen/recovery before retrying.
    pub fn commit_batch(&mut self, writes: Vec<TabletWrite>) -> Result<u64, WalBackedError> {
        if writes.is_empty() {
            return Ok(self.tablet.current_sequence());
        }

        let mut projected_sequence = self.tablet.current_sequence();
        for write in &writes {
            self.tablet
                .validate_write_at_sequence(write, projected_sequence)?;
            projected_sequence = write.sequence();
        }

        self.wal.append_batch(&writes)?;
        for write in writes {
            self.tablet.publish_validated(write);
        }
        Ok(projected_sequence)
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
    DuplicateBatchId(TransactionId),
    TooManyReceiptTransactions { requested: usize, limit: usize },
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
            Self::TooManyReceiptTransactions { requested, limit } => {
                write!(
                    f,
                    "receipt batch has {requested} transactions (limit {limit})"
                )
            }
            Self::DuplicateBatchId(id) => {
                write!(f, "transaction ID {id:?} appears twice in one group commit")
            }
            Self::IdempotencyConflict(id) => {
                write!(f, "transaction ID {id:?} reused with different mutations")
            }
        }
    }
}

impl std::error::Error for WalBackedError {}
