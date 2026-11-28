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
        let tablet = match checkpoint::load_checkpoint(&checkpoint_path, descriptor.clone())? {
            Some(mut tablet) => {
                wal.replay_after_checkpoint(&mut tablet)?;
                tablet
            }
            None => wal.recover_memory_tablet(descriptor)?,
        };
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
    /// unbound compacted WALs. A version-four header must bind exactly to
    /// the sealed manifest before tail repair. Only explicit sealed cutover
    /// emits this header; legacy checkpoint/reclamation is disabled for it.
    #[cfg(unix)]
    pub fn open_from_sealed_sst(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
        manifest_dir: impl AsRef<Path>,
    ) -> std::io::Result<Self> {
        let manifest = super::sst_manifest::ManifestStore::open(
            manifest_dir,
            descriptor.id().get(),
            descriptor.ownership_epoch(),
        )?;
        Self::open_with_sealed_manifest(descriptor, wal_path.as_ref(), &manifest, None)
    }

    #[cfg(unix)]
    pub(super) fn open_with_sealed_manifest(
        descriptor: TabletDescriptor,
        wal_path: &Path,
        manifest: &super::sst_manifest::ManifestStore,
        registry: Option<[u8; 32]>,
    ) -> std::io::Result<Self> {
        use std::io;
        let invalid = |reason: &'static str| io::Error::new(io::ErrorKind::InvalidData, reason);
        // Both locks are nonblocking. Resolve the expected seal before opening
        // the WAL so a mismatched bound header cannot trigger tail repair.
        if manifest.current().recovery_descriptor() != Some(&descriptor) {
            return Err(invalid("sealed recovery descriptor mismatch"));
        }
        let binding = manifest.wal_binding()?;
        let wal = if let Some(registry) = registry {
            FileWal::open_existing_bound(wal_path, binding, registry)
        } else {
            FileWal::open_existing(wal_path, binding)
        }
        .map_err(io::Error::other)?;
        let checkpoint_path = checkpoint::checkpoint_path_for_wal(wal.path());
        match std::fs::symlink_metadata(&checkpoint_path) {
            Ok(_) => return Err(invalid("existing checkpoint requires normal recovery")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if !wal.is_seal_bound() {
            if manifest.current().recovery_floor() != Some(0) {
                return Err(invalid(
                    "nonzero seal floor is pending WAL4 cutover; reopen complete WAL normally",
                ));
            }
            super::wal_sst_shadow::verify_prefix(&wal, &descriptor, manifest)?;
        }
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
        self.seal_sst_recovery_at_floor(manifest, 0)
    }

    /// Prepare an immutable seal with a requested logical retention floor.
    /// This changes only the sealed snapshot, not the live tablet. The floor
    /// becomes authoritative for live recovery only after explicit WAL4 cutover.
    /// External consumers and backups must be coordinated by the caller.
    #[cfg(unix)]
    pub fn seal_sst_recovery_at_floor(
        &self,
        manifest: &mut super::sst_manifest::ManifestStore,
        floor: u64,
    ) -> std::io::Result<()> {
        self.tablet
            .validate_snapshot(floor)
            .map_err(std::io::Error::other)?;
        if floor > self.safe_retention_floor() {
            return Err(std::io::Error::other(
                "seal retention floor exceeds live snapshot pin",
            ));
        }
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
            floor,
            self.current_sequence(),
            self.tablet.receipts(),
        )?;
        manifest.seal(meta)
    }

    /// Explicit, one-time cutover from a complete WAL3 to a sealed WAL4.
    /// The seal must cover the entire committed history, including receipts.
    /// Keep the immutable manifest and all its tables for the life of this
    /// WAL. After cutover, reopen only with open_from_sealed_sst. A failed
    /// replacement requires reopening; no legacy checkpoint or retention
    /// reclamation is supported for bound WALs. Trusted local Unix storage.
    #[cfg(unix)]
    pub fn reclaim_to_sealed_sst(
        &mut self,
        manifest: &super::sst_manifest::ManifestStore,
    ) -> std::io::Result<()> {
        use std::io;
        let invalid = |reason: &'static str| io::Error::new(io::ErrorKind::InvalidData, reason);
        if self.recovery_required
            || self.wal.is_poisoned()
            || self.wal.is_seal_bound()
            || self.wal.base_sequence() != 0
            || self.oldest_retained_snapshot() != 0
            || self.current_sequence() == 0
            || self.current_sequence() != self.wal.last_sequence()
            || manifest.current().durable_sequence() != self.current_sequence()
            || manifest.current().recovery_descriptor() != Some(self.descriptor())
        {
            return Err(invalid(
                "reclaim requires matching complete nonempty unpruned WAL and seal",
            ));
        }
        match std::fs::symlink_metadata(&self.checkpoint_path) {
            Ok(_) => return Err(invalid("existing checkpoint prevents sealed WAL cutover")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // Independently validate all SST versions and receipt correspondence,
        // then check the complete restored logical state against the live tablet.
        super::wal_sst_shadow::verify_prefix(&self.wal, self.descriptor(), manifest)?;
        let floor = manifest
            .current()
            .recovery_floor()
            .ok_or_else(|| invalid("missing sealed floor"))?;
        if floor > self.safe_retention_floor() {
            return Err(invalid("seal retention floor exceeds live snapshot pin"));
        }
        let mut restored = manifest.restore_memory_tablet()?;
        self.wal
            .replay_after_checkpoint(&mut restored)
            .map_err(io::Error::other)?;
        let mut expected = self.tablet.clone();
        if floor != 0 {
            expected
                .prune_history_before(floor)
                .map_err(io::Error::other)?;
        }
        if restored.snapshot_state() != expected.snapshot_state() {
            return Err(invalid("sealed recovery state disagrees with live tablet"));
        }
        drop(expected);
        // Re-establish durability even if a previous seal publication was
        // interrupted after rename but before its directory sync.
        let binding = manifest.sync_for_wal_binding()?;
        if let Err(error) = self.wal.reclaim_to_binding(binding) {
            self.recovery_required = true;
            return Err(io::Error::other(error));
        }
        self.tablet = restored;
        Ok(())
    }

    /// Prepare a sealed successor in a separate, empty operator-provided
    /// directory. The previous seal and WAL remain authoritative and intact.
    /// Failure may leave an incomplete inactive directory: use a new one.
    #[cfg(unix)]
    pub fn prepare_sst_successor(
        &self,
        previous: &super::sst_manifest::ManifestStore,
        next: &mut super::sst_manifest::ManifestStore,
        floor: u64,
    ) -> std::io::Result<()> {
        self.validate_successor_base(previous, floor)?;
        let meta = super::sst_recovery_meta::RecoveryMeta::new(
            self.descriptor().clone(),
            floor,
            self.current_sequence(),
            self.tablet.receipts(),
        )?;
        previous.copy_prefix_into(next)?;
        super::wal_sst_shadow::append_bound_tail(&self.wal, next)?;
        super::wal_sst_shadow::verify_successor(&self.wal, previous, next)?;
        next.seal(meta)
    }

    #[cfg(unix)]
    fn validate_successor_base(
        &self,
        previous: &super::sst_manifest::ManifestStore,
        floor: u64,
    ) -> std::io::Result<()> {
        use std::io;
        if self.recovery_required
            || self.wal.is_poisoned()
            || !self.wal.matches_seal(previous.wal_binding()?)
            || previous.current().recovery_descriptor() != Some(self.descriptor())
            || previous.current().recovery_floor() != Some(self.oldest_retained_snapshot())
            || self.current_sequence() != self.wal.last_sequence()
            || self.current_sequence() <= self.wal.base_sequence()
            || floor > self.safe_retention_floor()
        {
            return Err(io::Error::other(
                "invalid successor base, tail or pinned floor",
            ));
        }
        self.tablet
            .validate_snapshot(floor)
            .map_err(io::Error::other)?;
        match std::fs::symlink_metadata(&self.checkpoint_path) {
            Ok(_) => Err(io::Error::other(
                "existing checkpoint prevents successor cutover",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Atomically rotate a bound WAL to a verified successor seal. Retain both
    /// seal directories until recovery and external backup policy allow retirement.
    /// After interruption try the known old and new directories; only the
    /// exact WAL binding is accepted. No automatic discovery or deletion.
    #[cfg(unix)]
    pub fn reclaim_to_sst_successor(
        &mut self,
        previous: &super::sst_manifest::ManifestStore,
        next: &super::sst_manifest::ManifestStore,
    ) -> std::io::Result<()> {
        use std::io;
        let floor = next
            .current()
            .recovery_floor()
            .ok_or_else(|| io::Error::other("successor not sealed"))?;
        self.validate_successor_base(previous, floor)?;
        if next.current().recovery_descriptor() != Some(self.descriptor())
            || next.directory() == previous.directory()
        {
            return Err(io::Error::other(
                "invalid successor descriptor or directory",
            ));
        }
        super::wal_sst_shadow::verify_successor(&self.wal, previous, next)?;
        let mut restored = next.restore_memory_tablet()?;
        self.wal
            .replay_after_checkpoint(&mut restored)
            .map_err(io::Error::other)?;
        let mut expected = self.tablet.clone();
        expected
            .prune_history_before(floor)
            .map_err(io::Error::other)?;
        if restored.snapshot_state() != expected.snapshot_state() {
            return Err(io::Error::other(
                "successor state disagrees with live tablet",
            ));
        }
        drop(expected);
        // Revalidate both physical generations before changing the binding.
        if !self.wal.matches_seal(previous.sync_for_wal_binding()?) {
            return Err(io::Error::other("previous seal binding mismatch"));
        }
        let binding = next.sync_for_wal_binding()?;
        if let Err(error) = self.wal.reclaim_to_binding(binding) {
            self.recovery_required = true;
            return Err(io::Error::other(error));
        }
        self.tablet = restored;
        Ok(())
    }

    /// Verify the exact sealed prefix and its retained MVCC history before
    /// same-sequence rebinding. Compare both base snapshots, then replay the
    /// unchanged tail independently and compare with the acknowledged state.
    #[cfg(unix)]
    fn verify_compaction_binding(
        &self,
        previous: &super::sst_manifest::ManifestStore,
        next: &super::sst_manifest::ManifestStore,
    ) -> std::io::Result<(super::wal::SealBinding, super::wal::SealBinding)> {
        use std::io;
        let old = self.verified_registry_binding(previous)?;
        let new = next.sync_for_wal_binding()?;
        if old.sequence != new.sequence
            || old.digest == new.digest
            || next.current().recovery_descriptor() != Some(self.descriptor())
            || next.current().recovery_floor() != previous.current().recovery_floor()
            || previous.directory().starts_with(next.directory())
            || next.directory().starts_with(previous.directory())
            || next.current().entries().is_empty()
            || next.current().entries().len() > 16
        {
            return Err(io::Error::other("invalid compacted generation"));
        }
        match std::fs::symlink_metadata(&self.checkpoint_path) {
            Ok(_) => return Err(io::Error::other("legacy checkpoint prevents compaction")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut restored = next.restore_memory_tablet()?;
        if previous.restore_memory_tablet()?.snapshot_state() != restored.snapshot_state() {
            return Err(io::Error::other(
                "compacted base differs from retained prefix",
            ));
        }
        self.wal
            .replay_after_checkpoint(&mut restored)
            .map_err(io::Error::other)?;
        if restored.snapshot_state() != self.tablet.snapshot_state() {
            return Err(io::Error::other(
                "compacted base and WAL tail disagree with live state",
            ));
        }
        Ok((old, new))
    }

    #[cfg(unix)]
    pub(super) fn reserve_generation_compaction(
        &self,
        registry: &mut super::generation_registry::GenerationRegistry,
        previous: &super::sst_manifest::ManifestStore,
        next: &super::sst_manifest::ManifestStore,
    ) -> std::io::Result<()> {
        let (old, new) = self.verify_compaction_binding(previous, next)?;
        registry.reserve_compaction(self.wal.path(), old, new)
    }

    #[cfg(unix)]
    pub(super) fn rebind_compacted_generation(
        &mut self,
        previous: &super::sst_manifest::ManifestStore,
        next: &super::sst_manifest::ManifestStore,
    ) -> std::io::Result<()> {
        let (_, new) = self.verify_compaction_binding(previous, next)?;
        if let Err(error) = self.wal.rebind_compacted(new) {
            self.recovery_required = true;
            return Err(std::io::Error::other(error));
        }
        // Retention floor and logical state do not change; retain live pins.
        Ok(())
    }

    /// Export a point-in-time local backup while this handle owns the WAL
    /// writer lock and the caller owns the active manifest lock. Only a
    /// fresh destination is accepted; never reuse an interrupted directory.
    /// No concurrent writes during this synchronous operation.
    #[cfg(unix)]
    pub fn backup_sealed(
        &self,
        manifest: &super::sst_manifest::ManifestStore,
        destination: impl AsRef<Path>,
    ) -> std::io::Result<()> {
        use super::sealed_backup::{self, Completion};
        use std::io;
        let binding = manifest.wal_binding()?;
        if self.recovery_required
            || self.wal.is_poisoned()
            || !self.wal.matches_seal(binding)
            || manifest.current().recovery_descriptor() != Some(self.descriptor())
            || manifest.current().recovery_floor() != Some(self.oldest_retained_snapshot())
            || self.current_sequence() != self.wal.last_sequence()
        {
            return Err(io::Error::other(
                "backup requires active recovered sealed binding",
            ));
        }
        match std::fs::symlink_metadata(&self.checkpoint_path) {
            Ok(_) => return Err(io::Error::other("legacy checkpoint prevents sealed backup")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let destination = destination.as_ref();
        sealed_backup::new_directory(destination)?;
        let seal_dir = destination.join("sst");
        let copied = manifest.copy_seal_into(&seal_dir)?;
        if copied != binding {
            return Err(io::Error::other("backup seal changed"));
        }
        let wal_path = destination.join("tablet.wal");
        let (wal_len, wal_digest) = sealed_backup::portable_wal(&self.wal, Some(&wal_path))?;
        // Reopen the independent copy, not the source. Require exact live
        // logical state and ensure recovery did not truncate any bytes.
        let recovered =
            Self::open_from_sealed_sst(self.descriptor().clone(), &wal_path, &seal_dir)?;
        if recovered.tablet.snapshot_state() != self.tablet.snapshot_state()
            || std::fs::metadata(&wal_path)?.len() != wal_len
        {
            return Err(io::Error::other("backup recovery disagrees with source"));
        }
        drop(recovered);
        #[cfg(test)]
        super::interruption::hit(
            super::interruption::StorageInterruptionPoint::BackupBeforeCompletion,
        )?;
        Completion {
            sequence: self.current_sequence(),
            floor: self.oldest_retained_snapshot(),
            wal_len,
            wal_digest,
            seal_digest: binding.digest,
        }
        .publish(destination)
    }

    /// Restore a completed backup into a fresh, independent directory.
    /// Verify the exact WAL length and digest before any recovery parser can
    /// repair a torn tail. Never write to the archived WAL or overwrite a
    /// previous restore. The caller must not promote a failed destination.
    #[cfg(unix)]
    pub fn restore_sealed_backup(
        descriptor: TabletDescriptor,
        backup: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> std::io::Result<Self> {
        use super::sealed_backup::{self, Completion};
        use std::io;
        let backup = backup.as_ref();
        let destination = destination.as_ref();
        let completion = Completion::read(backup)?;
        let manifest = super::sst_manifest::ManifestStore::open(
            backup.join("sst"),
            descriptor.id().get(),
            descriptor.ownership_epoch(),
        )?;
        let binding = manifest.wal_binding()?;
        if binding.digest != completion.seal_digest
            || manifest.current().recovery_descriptor() != Some(&descriptor)
            || manifest.current().recovery_floor() != Some(completion.floor)
            || binding.sequence > completion.sequence
        {
            return Err(io::Error::other(
                "backup seal does not match completion marker",
            ));
        }
        sealed_backup::new_directory(destination)?;
        let seal_dir = destination.join("sst");
        manifest.copy_seal_into(&seal_dir)?;
        let wal_path = destination.join("tablet.wal");
        if sealed_backup::copy_wal(&backup.join("tablet.wal"), &wal_path, completion.wal_len)?
            != completion.wal_digest
        {
            return Err(io::Error::other("backup WAL digest mismatch"));
        }
        let restored = Self::open_from_sealed_sst(descriptor, &wal_path, &seal_dir)?;
        if !restored.wal.matches_seal(binding)
            || restored.current_sequence() != completion.sequence
            || restored.oldest_retained_snapshot() != completion.floor
            || std::fs::metadata(&wal_path)?.len() != completion.wal_len
        {
            return Err(io::Error::other(
                "restored state disagrees with backup completion",
            ));
        }
        sealed_backup::sync_directory(destination)?;
        sealed_backup::sync_directory(
            destination
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )?;
        Ok(restored)
    }

    /// Explicitly quarantine a strict obsolete prefix after verifying the
    /// current binding and an exact completed backup of the current state.
    /// The obsolete directory is moved, never deleted. Caller must ensure
    /// it is dedicated to this tablet, no other WAL/reader/backup relies on
    /// its original pathname, and the parent namespace is not concurrently
    /// modified. Do not reuse the old pathname. After ambiguous rename/sync,
    /// inspect both paths; the active generation and WAL are unchanged.
    #[cfg(unix)]
    pub fn quarantine_obsolete_sst(
        &self,
        active: &super::sst_manifest::ManifestStore,
        obsolete: super::sst_manifest::ManifestStore,
        backup: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> std::io::Result<()> {
        let _archive = self.verify_retirement_candidate(active, &obsolete, backup.as_ref())?;
        obsolete.quarantine(destination.as_ref())
    }

    /// Verify current recoverability and exact prefix lineage, retaining
    /// the archive lock through the caller's registry publication or move.
    #[cfg(unix)]
    pub(super) fn verify_retirement_candidate(
        &self,
        active: &super::sst_manifest::ManifestStore,
        obsolete: &super::sst_manifest::ManifestStore,
        backup: &Path,
    ) -> std::io::Result<super::sst_manifest::ManifestStore> {
        obsolete.verify_obsolete_prefix(active)?;
        obsolete.verify_dedicated_files()?;
        self.verify_current_retirement_backup(active, obsolete.directory(), backup)
    }

    /// Managed retirement additionally accepts committed lineage for this
    /// exact WAL. The raw API retains the stricter physical-prefix check.
    #[cfg(unix)]
    pub(super) fn verify_managed_retirement_candidate(
        &self,
        registry: &super::generation_registry::GenerationRegistry,
        active: &super::sst_manifest::ManifestStore,
        obsolete: &super::sst_manifest::ManifestStore,
        backup: &Path,
    ) -> std::io::Result<super::sst_manifest::ManifestStore> {
        let current = self.verified_registry_binding(active)?;
        let old = obsolete.wal_binding()?;
        if registry.proves_ancestry(self.wal.path(), old, current)? {
            obsolete.verify_retirement_scope(active)?;
            obsolete.verify_dedicated_files()?;
            self.verify_current_retirement_backup(active, obsolete.directory(), backup)
        } else {
            self.verify_retirement_candidate(active, obsolete, backup)
        }
    }

    #[cfg(unix)]
    pub(super) fn verify_current_retirement_backup(
        &self,
        active: &super::sst_manifest::ManifestStore,
        old: &Path,
        backup: &Path,
    ) -> std::io::Result<super::sst_manifest::ManifestStore> {
        use super::sealed_backup::{self, Completion};
        use std::io;
        let binding = active.wal_binding()?;
        if self.recovery_required
            || self.wal.is_poisoned()
            || !self.wal.matches_seal(binding)
            || active.current().recovery_descriptor() != Some(self.descriptor())
            || active.current().recovery_floor() != Some(self.oldest_retained_snapshot())
            || self.current_sequence() != self.wal.last_sequence()
        {
            return Err(io::Error::other(
                "quarantine requires active recovered sealed binding",
            ));
        }
        match std::fs::symlink_metadata(&self.checkpoint_path) {
            Ok(_) => return Err(io::Error::other("legacy checkpoint prevents quarantine")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let backup = backup.canonicalize()?;
        let active_dir = active.directory();
        let overlaps = |a: &Path, b: &Path| a.starts_with(b) || b.starts_with(a);
        if overlaps(old, active_dir)
            || overlaps(old, &backup)
            || overlaps(active_dir, &backup)
            || self.wal.path().starts_with(old)
            || self.wal.path().starts_with(&backup)
        {
            return Err(io::Error::other("quarantine paths must be independent"));
        }
        if active.sync_for_wal_binding()? != binding {
            return Err(io::Error::other("active seal binding changed"));
        }
        let completion = Completion::read(&backup)?;
        let (wal_len, live_digest) = sealed_backup::portable_wal(&self.wal, None)?;
        if completion.sequence != self.current_sequence()
            || completion.floor != self.oldest_retained_snapshot()
            || completion.seal_digest != binding.digest
            || completion.wal_len != wal_len
        {
            return Err(io::Error::other(
                "quarantine requires a current completed backup",
            ));
        }
        let archived = super::sst_manifest::ManifestStore::open(
            backup.join("sst"),
            self.descriptor().id().get(),
            self.descriptor().ownership_epoch(),
        )?;
        if archived.sync_for_wal_binding()? != binding
            || archived.current().recovery_descriptor() != Some(self.descriptor())
            || sealed_backup::wal_digest(&backup.join("tablet.wal"), wal_len)?
                != completion.wal_digest
            || live_digest != completion.wal_digest
        {
            return Err(io::Error::other(
                "backup contents do not match active generation and WAL",
            ));
        }
        Ok(archived)
    }

    /// Register the actual recovered bound WAL under its canonical path.
    /// This is opt-in: existing raw cutover APIs do not enforce registration.
    #[cfg(unix)]
    pub fn register_generation_wal(
        &self,
        registry: &mut super::generation_registry::GenerationRegistry,
        manifest: &super::sst_manifest::ManifestStore,
    ) -> std::io::Result<()> {
        registry.register_wal(self.wal.path(), self.verified_registry_binding(manifest)?)
    }

    #[cfg(unix)]
    pub(super) fn enroll_generation_registry(
        &mut self,
        registry: &super::generation_registry::GenerationRegistry,
    ) -> std::io::Result<()> {
        self.wal
            .enroll_registry(registry.enrollment_identity()?)
            .map_err(std::io::Error::other)
    }

    #[cfg(unix)]
    fn verified_registry_binding(
        &self,
        manifest: &super::sst_manifest::ManifestStore,
    ) -> std::io::Result<super::wal::SealBinding> {
        let binding = manifest.sync_for_wal_binding()?;
        if self.recovery_required
            || self.wal.is_poisoned()
            || !self.wal.matches_seal(binding)
            || manifest.current().recovery_descriptor() != Some(self.descriptor())
            || self.wal.last_sequence() != self.current_sequence()
            || manifest.current().recovery_floor() != Some(self.oldest_retained_snapshot())
        {
            return Err(std::io::Error::other(
                "registry requires recovered exact WAL binding",
            ));
        }
        self.wal
            .sync_for_registry()
            .map_err(std::io::Error::other)?;
        Ok(binding)
    }

    /// Reserve both generations durably before a caller performs rotation.
    /// If publication fails, do not rotate until the registry is reopened.
    #[cfg(unix)]
    pub fn reserve_generation_rotation(
        &self,
        registry: &mut super::generation_registry::GenerationRegistry,
        previous: &super::sst_manifest::ManifestStore,
        next: &super::sst_manifest::ManifestStore,
    ) -> std::io::Result<()> {
        let old = self.verified_registry_binding(previous)?;
        let floor = next
            .current()
            .recovery_floor()
            .ok_or_else(|| std::io::Error::other("successor not sealed"))?;
        self.validate_successor_base(previous, floor)?;
        if next.current().recovery_descriptor() != Some(self.descriptor())
            || next.directory() == previous.directory()
        {
            return Err(std::io::Error::other("invalid successor for registry"));
        }
        super::wal_sst_shadow::verify_successor(&self.wal, previous, next)?;
        registry.reserve_rotation(self.wal.path(), old, next.sync_for_wal_binding()?)
    }

    /// Resolve a pending rotation using the actual recovered bound WAL.
    #[cfg(unix)]
    pub fn reconcile_generation_wal(
        &self,
        registry: &mut super::generation_registry::GenerationRegistry,
        manifest: &super::sst_manifest::ManifestStore,
    ) -> std::io::Result<()> {
        registry.reconcile_wal(self.wal.path(), self.verified_registry_binding(manifest)?)
    }

    /// Diagnostic point read against the published shadow SST prefix.
    /// No fallback or authoritative read routing. Compare both the exact
    /// WAL mutation (including sequence and tombstone) and the live tablet.
    #[cfg(unix)]
    pub fn read_sst_shadow_at(
        &self,
        manifest: &super::sst_manifest::ManifestStore,
        key: &[u8],
        snapshot: u64,
    ) -> std::io::Result<super::indexed_sst::DiskLookup> {
        if self.recovery_required
            || self.wal.is_poisoned()
            || self.oldest_retained_snapshot() != 0
            || self.wal.base_sequence() != 0
            || self.wal.last_sequence() != self.current_sequence()
        {
            return Err(std::io::Error::other(
                "shadow read requires recovered complete unpruned WAL",
            ));
        }
        let live = self.read_at(key, snapshot).map_err(std::io::Error::other)?;
        super::wal_sst_shadow::read_verified(
            &self.wal,
            self.descriptor(),
            manifest,
            key,
            snapshot,
            live,
        )
    }

    /// Atomically publish a checkpoint without reclaiming the WAL.
    ///
    /// This is a valid crash state and is intentionally public so operators can
    /// separate checkpoint publication from later space reclamation.
    pub fn publish_checkpoint(&self) -> Result<(), WalBackedError> {
        if self.wal.is_seal_bound() {
            return Err(WalError::SealBindingMismatch.into());
        }
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
        if self.wal.is_seal_bound() {
            return Err(WalError::SealBindingMismatch.into());
        }
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
