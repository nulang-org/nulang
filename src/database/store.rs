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

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use super::checkpoint::{self, CheckpointError};
use super::optimistic::{OptimisticTabletWrite, PointTransaction};
use super::tablet::{
    transaction_digest, MemoryTablet, TabletDescriptor, TabletError, TabletMutation, TabletWrite,
    TransactionId, TransactionIdentity, TransactionReceipt,
};
use super::wal::{
    group_input_bytes, FileWal, WalError, MAX_GROUP_COMMIT_BYTES, MAX_GROUP_COMMIT_RECORDS,
};

/// In-process snapshot retention guard. Valid only for the lifetime of
/// the tablet handle that issued it; not a durable lease or a backup pin.
#[derive(Debug, Clone)]
pub struct SnapshotPin {
    sequence: u64,
    _lease: Arc<()>,
}

impl SnapshotPin {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

#[derive(Debug)]
pub struct WalBackedTablet {
    tablet: MemoryTablet,
    wal: FileWal,
    checkpoint_path: PathBuf,
    snapshot_pins: BTreeMap<u64, Weak<()>>,
    recovery_required: bool,
}

impl WalBackedTablet {
    /// Open a tablet and reconstruct its MVCC state from the valid WAL prefix.
    pub fn open(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
    ) -> Result<Self, WalBackedError> {
        let wal_path = wal_path.as_ref();
        let wal = FileWal::open(wal_path)?;
        let checkpoint_path = checkpoint::checkpoint_path_for_wal(wal.path());
        let mut tablet = match checkpoint::load_checkpoint(&checkpoint_path, descriptor.clone())? {
            Some(tablet) => tablet,
            None => wal.recover_memory_tablet(descriptor)?,
        };
        wal.replay_after_checkpoint(&mut tablet)?;
        Ok(Self {
            tablet,
            wal,
            checkpoint_path,
            snapshot_pins: BTreeMap::new(),
            recovery_required: false,
        })
    }

    /// Explicitly recover a live in-memory tablet from a sealed SST prefix
    /// plus the complete existing WAL. Verify every overlapping mutation and
    /// receipt before replaying the tail. Refuses existing checkpoints and
    /// compacted WALs: their retention or lineage cannot yet be reconciled
    /// with the seal. Does not enable SST-based WAL reclamation.
    #[cfg(unix)]
    pub fn open_from_sealed_sst(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
        manifest_dir: impl AsRef<Path>,
    ) -> std::io::Result<Self> {
        use std::io;
        let invalid = |reason: &'static str| io::Error::new(io::ErrorKind::InvalidData, reason);
        let wal = FileWal::open_existing(wal_path.as_ref())
            .map_err(|error| io::Error::other(error.to_string()))?;
        let checkpoint_path = checkpoint::checkpoint_path_for_wal(wal.path());
        match std::fs::symlink_metadata(&checkpoint_path) {
            Ok(_) => return Err(invalid("existing checkpoint requires normal recovery")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let manifest = super::sst_manifest::ManifestStore::open(
            manifest_dir,
            descriptor.id().get(),
            descriptor.ownership_epoch(),
        )?;
        if manifest.current().recovery_descriptor() != Some(&descriptor) {
            return Err(invalid("sealed recovery descriptor mismatch"));
        }
        super::wal_sst_shadow::verify_prefix(&wal, &descriptor, &manifest)?;
        let mut tablet = manifest.restore_memory_tablet()?;
        wal.replay_after_checkpoint(&mut tablet)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        Ok(Self {
            tablet,
            wal,
            checkpoint_path,
            snapshot_pins: BTreeMap::new(),
            recovery_required: false,
        })
    }

    pub fn descriptor(&self) -> &TabletDescriptor {
        self.tablet.descriptor()
    }

    pub fn current_sequence(&self) -> u64 {
        self.tablet.current_sequence()
    }

    pub fn oldest_retained_snapshot(&self) -> u64 {
        self.tablet.oldest_retained_snapshot()
    }

    /// Greatest floor allowed by currently live in-process snapshot guards.
    pub fn safe_retention_floor(&self) -> u64 {
        self.snapshot_pins
            .iter()
            .find_map(|(&sequence, pin)| (pin.strong_count() > 0).then_some(sequence))
            .unwrap_or_else(|| self.current_sequence())
    }

    /// Hold the returned guard for as long as the snapshot is needed.
    /// Dropping the last guard allows a later explicit reclamation.
    pub fn pin_snapshot(&mut self, snapshot: u64) -> Result<SnapshotPin, TabletError> {
        if self.recovery_required {
            return Err(TabletError::RecoveryRequired);
        }
        self.tablet.validate_snapshot(snapshot)?;
        self.snapshot_pins.retain(|_, pin| pin.strong_count() > 0);
        let lease = self
            .snapshot_pins
            .get(&snapshot)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(()));
        self.snapshot_pins.insert(snapshot, Arc::downgrade(&lease));
        Ok(SnapshotPin {
            sequence: snapshot,
            _lease: lease,
        })
    }

    pub fn prepare_write(
        &self,
        presented_epoch: u64,
        expected_previous_sequence: u64,
        mutations: Vec<TabletMutation>,
    ) -> Result<TabletWrite, TabletError> {
        if self.recovery_required {
            return Err(TabletError::RecoveryRequired);
        }
        self.tablet
            .prepare_write(presented_epoch, expected_previous_sequence, mutations)
    }

    /// Prepare snapshot-scoped point-key writes without reserving the next
    /// WAL sequence. A second independent writer can prepare at the same
    /// snapshot and commit afterwards if the read/write sets do not conflict.
    ///
    /// This API does not validate predicate/range reads or multi-tablet state.
    pub fn prepare_optimistic_write(
        &self,
        ownership_epoch: u64,
        snapshot_sequence: u64,
        read_keys: Vec<Vec<u8>>,
        mutations: Vec<TabletMutation>,
    ) -> Result<OptimisticTabletWrite, TabletError> {
        if self.recovery_required || self.wal.is_poisoned() {
            return Err(TabletError::RecoveryRequired);
        }
        self.tablet.prepare_optimistic_write(
            ownership_epoch,
            snapshot_sequence,
            read_keys,
            mutations,
        )
    }

    /// Start an automatically tracked point-key transaction at the current
    /// snapshot. Explicitly pin it if retention may advance. Reads through
    /// `read_point` automatically enter the transaction's read set.
    pub fn begin_point_transaction(&self) -> PointTransaction {
        self.tablet.begin_point_transaction()
    }

    /// Read one key through a tracked transaction at its original snapshot.
    pub fn read_point(
        &self,
        tx: &mut PointTransaction,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, TabletError> {
        if self.recovery_required || self.wal.is_poisoned() {
            return Err(TabletError::RecoveryRequired);
        }
        tx.read(&self.tablet, key)
    }

    /// Commit a tracked point transaction with the same WAL receipts and
    /// point-key conflict checks as the low-level optimistic API.
    pub fn commit_point_transaction_idempotent(
        &mut self,
        id: TransactionId,
        tx: PointTransaction,
    ) -> Result<TransactionReceipt, WalBackedError> {
        self.require_recovered()?;
        if self.wal.is_poisoned() {
            return Err(WalError::Poisoned.into());
        }
        let prepared = tx.into_prepared(&self.tablet)?;
        self.commit_optimistic_idempotent(id, prepared)
    }

    /// Durably commit an identified point-key optimistic transaction.
    ///
    /// Existing matching receipts win over stale snapshot conflicts, enabling
    /// an ambiguous WAL retry after restart using the original ID and writes.
    /// Receipts identify the ordered mutation payload (as in commit_idempotent).
    /// This is a single-tablet, single-owner API, not a multi-node transaction.
    pub fn commit_optimistic_idempotent(
        &mut self,
        id: TransactionId,
        proposed: OptimisticTabletWrite,
    ) -> Result<TransactionReceipt, WalBackedError> {
        // Even a replay-only receipt lookup must reject wrong tablet, stale
        // ownership and future snapshots. Only *data conflicts* are bypassed
        // when an identical durable transaction is retried after newer writes.
        self.require_recovered()?;
        if self.wal.is_poisoned() {
            return Err(WalError::Poisoned.into());
        }
        self.tablet.validate_optimistic_identity(&proposed)?;
        let digest = transaction_digest(proposed.mutations());
        if let Some(receipt) = self.tablet.transaction_receipt(id) {
            if receipt.digest() != digest {
                return Err(WalBackedError::IdempotencyConflict(id));
            }
            return Ok(receipt);
        }
        let write = self
            .tablet
            .rebase_optimistic_write(proposed)?
            .with_identity(TransactionIdentity { id, digest });
        let sequence = self.commit(write)?;
        let receipt = self
            .tablet
            .transaction_receipt(id)
            .expect("durable optimistic commit must publish its receipt");
        debug_assert_eq!(receipt.sequence(), sequence);
        Ok(receipt)
    }

    /// Commit once per tablet-scoped caller ID and exact ordered payload.
    /// After ambiguous I/O, reopen and retry with the original ID and payload.
    /// Receipts are never evicted by MVCC history reclamation.
    pub fn commit_idempotent(
        &mut self,
        id: TransactionId,
        mutations: Vec<TabletMutation>,
    ) -> Result<TransactionReceipt, WalBackedError> {
        self.require_recovered()?;
        if self.wal.is_poisoned() {
            return Err(WalError::Poisoned.into());
        }
        let digest = transaction_digest(&mutations);
        if let Some(receipt) = self.tablet.transaction_receipt(id) {
            if receipt.digest() != digest {
                return Err(WalBackedError::IdempotencyConflict(id));
            }
            return Ok(receipt);
        }
        let write = self
            .tablet
            .prepare_write(
                self.tablet.descriptor().ownership_epoch(),
                self.current_sequence(),
                mutations,
            )?
            .with_identity(TransactionIdentity { id, digest });
        let sequence = self.commit(write)?;
        let receipt = self
            .tablet
            .transaction_receipt(id)
            .expect("validated commit publishes receipt");
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
        self.require_recovered()?;
        if self.wal.is_poisoned() {
            return Err(WalError::Poisoned.into());
        }
        // Bound the *request*, not just new WAL records. A replay-only batch
        // must not bypass admission limits and allocate an unbounded dedup set.
        if entries.len() > MAX_GROUP_COMMIT_RECORDS {
            return Err(WalBackedError::Wal(WalError::BatchTooLarge {
                records: entries.len(),
                bytes: 0,
            }));
        }
        // Retry-only requests are bounded too, before hashing large payloads.
        let raw_bytes =
            group_input_bytes(entries.iter().map(|(_, mutations)| mutations.as_slice()));
        if raw_bytes > MAX_GROUP_COMMIT_BYTES {
            return Err(WalError::BatchTooLarge {
                records: entries.len(),
                bytes: raw_bytes,
            }
            .into());
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
        self.require_recovered()?;
        self.tablet.validate_write(&write)?;
        self.wal.append_write(&write)?;
        Ok(self.tablet.publish_validated(write))
    }

    pub fn read_at(&self, key: &[u8], snapshot: u64) -> Result<Option<&[u8]>, TabletError> {
        if self.recovery_required {
            return Err(TabletError::RecoveryRequired);
        }
        self.tablet.read_at(key, snapshot)
    }

    /// Latest committed in-memory value; unaffected by history pruning.
    pub fn read_latest(&self, key: &[u8]) -> Option<&[u8]> {
        self.tablet.read_latest(key)
    }

    /// Materialize a bounded, verified shadow interval from the complete WAL.
    /// This never changes the WAL, checkpoint, receipts, or visible tablet state.
    #[cfg(unix)]
    pub fn flush_sst_shadow(
        &self,
        manifest: &mut super::sst_manifest::ManifestStore,
        max_records: usize,
    ) -> std::io::Result<Option<super::sst_manifest::TableEntry>> {
        if self.recovery_required
            || self.wal.is_poisoned()
            || self.oldest_retained_snapshot() != 0
            || self.wal.base_sequence() != 0
            || self.wal.last_sequence() != self.current_sequence()
        {
            return Err(std::io::Error::other(
                "shadow flush requires recovered complete unpruned WAL",
            ));
        }
        super::wal_sst_shadow::flush(&self.wal, self.descriptor(), manifest, max_records)
    }

    /// Seal an independently readable snapshot of the complete, verified WAL
    /// prefix. This does not enable live disk-backed writes or WAL reclamation.
    #[cfg(unix)]
    pub fn seal_sst_recovery(
        &self,
        manifest: &mut super::sst_manifest::ManifestStore,
    ) -> std::io::Result<()> {
        if manifest.current().durable_sequence() != self.current_sequence() {
            return Err(std::io::Error::other(
                "shadow manifest must cover entire committed WAL",
            ));
        }
        // No pending records: the shadow path verifies every referenced table
        // and rejects incomplete/pruned/poisoned history without publishing.
        if self.flush_sst_shadow(manifest, 1)?.is_some() {
            return Err(std::io::Error::other("unexpected shadow publication"));
        }
        let meta = super::sst_recovery_meta::RecoveryMeta::new(
            self.descriptor().clone(),
            self.oldest_retained_snapshot(),
            self.current_sequence(),
            self.tablet.receipts(),
        )?;
        manifest.seal(meta)
    }

    /// Atomically publish a checkpoint without reclaiming the WAL.
    ///
    /// This is a valid crash state and is intentionally public so operators can
    /// separate checkpoint publication from later space reclamation.
    pub fn publish_checkpoint(&self) -> Result<(), WalBackedError> {
        self.require_recovered()?;
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

    /// Explicitly advance the durable retention floor. Local pins are checked;
    /// callers must separately coordinate backups and external consumers.
    /// Checkpoint the candidate before publishing it or reclaiming the WAL.
    pub fn reclaim_history_before(&mut self, floor: u64) -> Result<(), WalBackedError> {
        self.require_recovered()?;
        if self.wal.is_poisoned() {
            return Err(WalError::Poisoned.into());
        }
        self.tablet
            .validate_snapshot(floor)
            .map_err(|error| match error {
                TabletError::SnapshotExpired {
                    requested,
                    oldest_retained,
                } => TabletError::RetentionFloorRegression {
                    requested,
                    oldest_retained,
                },
                other => other,
            })?;
        let safe = self.safe_retention_floor();
        if floor > safe {
            return Err(TabletError::SnapshotPinned {
                requested: floor,
                oldest_pinned: safe,
            }
            .into());
        }
        if floor == self.oldest_retained_snapshot() {
            return Ok(());
        }
        let mut candidate = self.tablet.clone();
        candidate.prune_history_before(floor)?;
        // Once publication begins, an error may mean the new floor is already
        // on disk. Never allow this handle to overwrite it with older history.
        if let Err(error) = checkpoint::write_checkpoint(&self.checkpoint_path, &candidate) {
            self.recovery_required = true;
            return Err(error.into());
        }
        self.tablet = candidate;
        if let Err(error) = self.wal.reclaim_through(self.tablet.current_sequence()) {
            self.recovery_required = true;
            return Err(error.into());
        }
        Ok(())
    }

    fn require_recovered(&self) -> Result<(), WalBackedError> {
        if self.recovery_required {
            return Err(TabletError::RecoveryRequired.into());
        }
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
