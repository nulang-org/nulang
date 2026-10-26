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
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::backup::{self, BackupError, BackupReceipt};
use super::checkpoint::{self, CheckpointError};
use super::retention_store::{DurableRetentionRegistry, RegistryError};
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
    /// Capture a consistent single-tablet local backup from the live owner.
    ///
    /// The owned WAL's exclusive cooperative single-host writer lock remains
    /// held and this shared borrow prevents in-process mutations during the
    /// entire copy, staging, disposable restore, and durable publication.
    /// The destination must not exist; a failed capture leaves no valid
    /// COMPLETE.json marker. Never use this on untrusted/network filesystems.
    ///
    /// This does not implement remote backups, consensus, or cross-tablet
    /// transactionally consistent snapshots.
    pub fn capture_consistent_backup(
        &self,
        destination: impl AsRef<Path>,
    ) -> Result<BackupReceipt, BackupError> {
        if self.wal.is_poisoned() || self.wal.last_sequence() != self.tablet.current_sequence() {
            return Err(BackupError::UnstableSource);
        }
        let descriptor = self.tablet.descriptor().clone();
        let source_report =
            Self::verify_checkpoint_wal_read_only(descriptor.clone(), self.wal.path())?;
        if source_report.durable_tail != self.tablet.current_sequence() {
            return Err(BackupError::UnstableSource);
        }
        backup::capture_locked(
            self.wal.path(),
            &self.checkpoint_path,
            descriptor.clone(),
            source_report,
            destination.as_ref(),
            |proof_wal| {
                // Only reopen a separate disposable copy. Recovery may create
                // sidecars or repair tails there; it must never touch source.
                let restored = Self::open(descriptor, proof_wal)?;
                if !self.tablet.same_committed_state(&restored.tablet) {
                    return Err(BackupError::RestoreMismatch);
                }
                Ok(())
            },
        )
    }

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
        self.commit_idempotent_internal(id, mutations, None)
    }

    /// Opt-in WAL budget for a new identified transaction. Already durable
    /// matching receipts are returned without appending, even at a zero budget.
    pub fn commit_idempotent_with_wal_budget(
        &mut self,
        id: TransactionId,
        mutations: Vec<TabletMutation>,
        max_wal_bytes: u64,
    ) -> Result<TransactionReceipt, WalBackedError> {
        self.commit_idempotent_internal(id, mutations, Some(max_wal_bytes))
    }

    fn commit_idempotent_internal(
        &mut self,
        id: TransactionId,
        mutations: Vec<TabletMutation>,
        max_wal_bytes: Option<u64>,
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
        let sequence = match max_wal_bytes {
            Some(max) => self.commit_with_wal_budget(write, max)?,
            None => self.commit(write)?,
        };
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
        self.commit_batch_idempotent_internal(entries, None)
    }

    /// Opt-in WAL limit for every new record in an identified group. The
    /// complete pending subset is admitted atomically before the first byte
    /// is written; existing matching receipts cost zero additional WAL bytes.
    pub fn commit_batch_idempotent_with_wal_budget(
        &mut self,
        entries: Vec<(TransactionId, Vec<TabletMutation>)>,
        max_wal_bytes: u64,
    ) -> Result<Vec<TransactionReceipt>, WalBackedError> {
        self.commit_batch_idempotent_internal(entries, Some(max_wal_bytes))
    }

    fn commit_batch_idempotent_internal(
        &mut self,
        entries: Vec<(TransactionId, Vec<TabletMutation>)>,
        max_wal_bytes: Option<u64>,
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
            match max_wal_bytes {
                Some(max) => self.wal.append_batch_with_budget(&pending, max)?,
                None => self.wal.append_batch(&pending)?,
            }
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

    /// Opt-in per-commit WAL physical-byte budget. Deny oversized append
    /// before the first write or MVCC publication. This does not compact
    /// retained history or enforce a global quota across other commit paths.
    pub fn commit_with_wal_budget(
        &mut self,
        write: TabletWrite,
        max_wal_bytes: u64,
    ) -> Result<u64, WalBackedError> {
        self.tablet.validate_write(&write)?;
        self.wal.append_write_with_budget(&write, max_wal_bytes)?;
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
        self.commit_batch_internal(writes, None)
    }

    /// Opt-in WAL limit for the entire prepared group (never partial
    /// admission). No checkpoint or retention cursor mutation is attempted.
    pub fn commit_batch_with_wal_budget(
        &mut self,
        writes: Vec<TabletWrite>,
        max_wal_bytes: u64,
    ) -> Result<u64, WalBackedError> {
        self.commit_batch_internal(writes, Some(max_wal_bytes))
    }

    fn commit_batch_internal(
        &mut self,
        writes: Vec<TabletWrite>,
        max_wal_bytes: Option<u64>,
    ) -> Result<u64, WalBackedError> {
        if writes.is_empty() {
            return Ok(self.tablet.current_sequence());
        }

        let mut projected_sequence = self.tablet.current_sequence();
        for write in &writes {
            self.tablet
                .validate_write_at_sequence(write, projected_sequence)?;
            projected_sequence = write.sequence();
        }

        match max_wal_bytes {
            Some(max) => self.wal.append_batch_with_budget(&writes, max)?,
            None => self.wal.append_batch(&writes)?,
        }
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

    /// Adopt a WAL that was *already* fully reclaimed before retention was
    /// enabled. The WAL owner lock must remain held and this immutable borrow
    /// prevents same-process commits during proof. Audit both files read-only,
    /// verify exact committed MVCC/receipt equality, then durably initialize
    /// an Unknown-coverage registry at wal.base_sequence + 1.
    ///
    /// This does not discover or certify detached consumers, and cannot
    /// recover pre-compaction WAL records. Any existing registry is refused:
    /// operator reconciliation of its pins is a separate manual process.
    pub fn bootstrap_retention_after_verified_compaction(
        &self,
    ) -> Result<DurableRetentionRegistry, WalBackedError> {
        if self.wal.is_poisoned() || self.wal.last_sequence() != self.tablet.current_sequence() {
            return Err(WalBackedError::UnverifiedCheckpoint);
        }
        let report = Self::verify_checkpoint_wal_read_only(
            self.tablet.descriptor().clone(),
            self.wal.path(),
        )?;
        let base = report.wal.base_sequence;
        if base == 0
            || base == u64::MAX
            || report.checkpoint_sequence.is_none()
            || report.durable_tail != self.tablet.current_sequence()
        {
            return Err(WalBackedError::UnverifiedCheckpoint);
        }
        let loaded =
            checkpoint::load_checkpoint(&self.checkpoint_path, self.tablet.descriptor().clone())?
                .ok_or(WalBackedError::UnverifiedCheckpoint)?;
        // A valid checksum/sequence by itself cannot prove the checkpoint
        // represents the exact data and committed transaction receipts.
        let mut reconstructed = loaded.clone();
        // This WAL may have records after its compacted base; verify their
        // logical replay as well against the live state.
        // We already own the live WAL, so use its retained WAL chain.
        self.wal.replay_after_checkpoint(&mut reconstructed)?;
        if !reconstructed.same_committed_state(&self.tablet) {
            return Err(WalBackedError::UnverifiedCheckpoint);
        }
        DurableRetentionRegistry::bootstrap_compacted(
            self.retention_registry_path(),
            self.tablet.descriptor().id().get(),
            self.tablet.descriptor().ownership_epoch(),
            base,
        )
        .map_err(Into::into)
    }

    /// Canonical registry path for opt-in, cooperative, single-host retention.
    /// The canonical WAL parent is used to prevent equivalent relative paths
    /// from silently producing independent retention owners.
    pub fn retention_registry_path(&self) -> PathBuf {
        let mut name = self.wal.path().as_os_str().to_os_string();
        name.push(".pins");
        PathBuf::from(name)
    }

    /// Reject the legacy full-tail checkpoint reclaim once a retention
    /// registry was initialized. Requiring the complete managed protocol
    /// prevents accidental bypass by callers of the historical API.
    ///
    /// This is a cooperative trusted-directory boundary, not a distributed
    /// enrollment fence against concurrent external path manipulation.
    fn reject_legacy_retention_bypass(&self) -> Result<(), WalBackedError> {
        let base = self.retention_registry_path();
        for extension in ["", ".lock", ".pending", ".stage"] {
            let mut name = base.as_os_str().to_os_string();
            name.push(extension);
            match fs::symlink_metadata(PathBuf::from(name)) {
                Ok(_) => return Err(WalBackedError::RetentionManaged),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(WalBackedError::Io(error.to_string())),
            }
        }
        Ok(())
    }

    /// Authorize only a full-tail reclaim with the exact local tablet
    /// registry held exclusively by the caller. No physical files change.
    fn preflight_retention_reclaim(
        &self,
        registry: &DurableRetentionRegistry,
    ) -> Result<u64, WalBackedError> {
        if !registry.matches_owner(
            &self.retention_registry_path(),
            self.tablet.descriptor().id().get(),
            self.tablet.descriptor().ownership_epoch(),
        ) {
            return Err(WalBackedError::RetentionMismatch);
        }
        let tail = self.tablet.current_sequence();
        if self.wal.is_poisoned() || self.wal.last_sequence() != tail {
            return Err(WalBackedError::UnverifiedCheckpoint);
        }
        // Existing compacted logs cannot be retroactively covered by a
        // freshly initialized registry with floor 1. That historical gap
        // requires explicit operator reconciliation before managed reclaim.
        if self.wal.base_sequence() != 0
            && registry.minimum_wal_cursor() <= self.wal.base_sequence()
        {
            return Err(WalBackedError::RetentionBootstrapRequired);
        }
        if tail == u64::MAX {
            return Err(WalBackedError::Retention(
                RegistryError::InvalidReclaimBound.to_string(),
            ));
        }
        let allowed = registry
            .policy()
            .wal_reclaim_bound(Some(tail), tail)
            .map_err(|error| {
                WalBackedError::Retention(
                    RegistryError::ReclaimBlocked(format!("{error:?}")).to_string(),
                )
            })?;
        if allowed != tail {
            return Err(WalBackedError::Retention(
                RegistryError::ReclaimBlocked(format!(
                    "WAL consumer still needs sequence {}",
                    allowed.saturating_add(1)
                ))
                .to_string(),
            ));
        }
        Ok(tail)
    }

    /// Verify an already-published, exact, *logically identical* checkpoint
    /// and WAL under the live writer lock, then durably reserve the retention
    /// watermark BEFORE rewriting the WAL. No partial-prefix cleanup exists.
    ///
    /// This is opt-in, local, and cooperative: source enrollment/completion
    /// must be certified by the external consumer owner, and no distributed
    /// fencing is implied. On uncertainty, do NOT reset/delete the registry.
    pub fn reclaim_verified_checkpoint_with_retention(
        &mut self,
        registry: &mut DurableRetentionRegistry,
    ) -> Result<(), WalBackedError> {
        let tail = self.preflight_retention_reclaim(registry)?;
        let report = Self::verify_checkpoint_wal_read_only(
            self.tablet.descriptor().clone(),
            self.wal.path(),
        )?;
        if report.checkpoint_sequence != Some(tail)
            || report.durable_tail != tail
            || report.wal.base_sequence != self.wal.base_sequence()
        {
            return Err(WalBackedError::UnverifiedCheckpoint);
        }
        let restored =
            checkpoint::load_checkpoint(&self.checkpoint_path, self.tablet.descriptor().clone())?
                .ok_or(WalBackedError::UnverifiedCheckpoint)?;
        if !restored.same_committed_state(&self.tablet) {
            return Err(WalBackedError::UnverifiedCheckpoint);
        }
        // Crash after reservation but before actual reclaim is conservative:
        // full WAL remains, but no new consumer can join below the floor.
        registry.reserve_reclaim_through(tail, tail)?;
        self.wal.reclaim_through_managed(
            tail,
            registry,
            self.tablet.descriptor().id().get(),
            self.tablet.descriptor().ownership_epoch(),
        )?;
        Ok(())
    }

    /// Publish checkpoint and perform the independently verified guarded
    /// reclaim, but only after fail-closed retention admission succeeds.
    pub fn checkpoint_with_retention(
        &mut self,
        registry: &mut DurableRetentionRegistry,
    ) -> Result<(), WalBackedError> {
        self.preflight_retention_reclaim(registry)?;
        self.publish_checkpoint()?;
        self.reclaim_verified_checkpoint_with_retention(registry)
    }

    /// Legacy full-tail checkpoint: valid only when retention is not enabled.
    /// Existing unregistered applications preserve the historical behavior;
    /// once a pin registry/sidecar exists, use checkpoint_with_retention.
    pub fn checkpoint(&mut self) -> Result<(), WalBackedError> {
        self.reject_legacy_retention_bypass()?;
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
    Retention(String),
    RetentionManaged,
    RetentionMismatch,
    RetentionBootstrapRequired,
    UnverifiedCheckpoint,
    Io(String),
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
impl From<RegistryError> for WalBackedError {
    fn from(error: RegistryError) -> Self {
        Self::Retention(error.to_string())
    }
}

impl fmt::Display for WalBackedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tablet(error) => write!(f, "tablet commit rejected: {error}"),
            Self::Wal(error) => write!(f, "tablet WAL failure: {error}"),
            Self::Checkpoint(error) => write!(f, "tablet checkpoint failure: {error}"),
            Self::Retention(error) => write!(f, "tablet retention admission failed: {error}"),
            Self::RetentionManaged => {
                f.write_str("retention-managed WAL cannot use the legacy checkpoint reclaimer")
            }
            Self::RetentionMismatch => f.write_str(
                "retention registry is not the exclusive owner for this WAL/tablet epoch",
            ),
            Self::RetentionBootstrapRequired => f.write_str(
                "existing reclaimed WAL history requires retention floor reconciliation",
            ),
            Self::UnverifiedCheckpoint => f.write_str(
                "checkpoint and WAL are not a proven exact copy of live committed history",
            ),
            Self::Io(error) => write!(f, "retention guard I/O error: {error}"),
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
