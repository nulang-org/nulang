//! Opt-in managed storage coordinator. Owns the tablet, registry and active
//! manifest locks for its lifetime. No raw mutable access or extraction.
//! Enrolled WALs require their exact local registry. Unenrolled WALs and
//! older binaries remain outside this scope; all
//! cooperating consumers must use the same dedicated registry and namespace.
//! Destructive retirement is explicit and requires an offline/quiescent retired
//! directory, a durable journal, and a current verified backup.
use super::generation_registry::{GenerationRegistry, GenerationStatus, ReferenceKind};
use super::sst_manifest::ManifestStore;
use super::store::{SnapshotPin, WalBackedError, WalBackedTablet};
use super::tablet::{
    TabletDescriptor, TabletError, TabletMutation, TransactionId, TransactionReceipt,
};
use std::io;
use std::path::Path;

#[derive(Debug)]
pub struct ManagedTablet {
    tablet: WalBackedTablet,
    registry: GenerationRegistry,
    active: ManifestStore,
    fenced: bool,
}

impl ManagedTablet {
    /// Explicit enrollment of an already recovered bound tablet. The caller
    /// must supply the authoritative registry; do not enroll into a new
    /// registry to bypass prior reference tracking.
    pub fn adopt(
        mut tablet: WalBackedTablet,
        mut registry: GenerationRegistry,
        active: ManifestStore,
    ) -> io::Result<Self> {
        tablet.register_generation_wal(&mut registry, &active)?;
        tablet.enroll_generation_registry(&registry)?;
        Ok(Self {
            tablet,
            registry,
            active,
            fenced: false,
        })
    }

    /// Recover only a previously registered binding, resolving a pending
    /// rotation from the exact durable WAL identity. Retain the active
    /// manifest lock across recovery and use.
    pub fn open(
        descriptor: TabletDescriptor,
        wal: impl AsRef<Path>,
        seal: impl AsRef<Path>,
        mut registry: GenerationRegistry,
    ) -> io::Result<Self> {
        let active =
            ManifestStore::open(seal, descriptor.id().get(), descriptor.ownership_epoch())?;
        registry.require_registered_wal(wal.as_ref(), active.wal_binding()?)?;
        let mut tablet = WalBackedTablet::open_with_sealed_manifest(
            descriptor,
            wal.as_ref(),
            &active,
            Some(registry.enrollment_identity()?),
        )?;
        tablet.reconcile_generation_wal(&mut registry, &active)?;
        tablet.enroll_generation_registry(&registry)?;
        Ok(Self {
            tablet,
            registry,
            active,
            fenced: false,
        })
    }

    /// Explicit offline repair of a missing, empty or recognized damaged
    /// enrollment sidecar. Stop all WAL users first: an unlinked old lock
    /// inode cannot be fenced by acquiring its replacement. Does not open
    /// the tablet, repair the WAL tail, or change registry references.
    /// WAL4, conflicting valid markers, and unrelated files are refused.
    pub fn repair_enrollment(
        descriptor: TabletDescriptor,
        wal: impl AsRef<Path>,
        seal: impl AsRef<Path>,
        registry: &GenerationRegistry,
    ) -> io::Result<()> {
        let active =
            ManifestStore::open(seal, descriptor.id().get(), descriptor.ownership_epoch())?;
        if active.current().recovery_descriptor() != Some(&descriptor) {
            return Err(io::Error::other("sealed recovery descriptor mismatch"));
        }
        let binding = active.sync_for_wal_binding()?;
        registry.require_registered_wal(wal.as_ref(), binding)?;
        super::wal::FileWal::repair_enrollment(
            wal.as_ref(),
            binding,
            registry.enrollment_identity()?,
        )
        .map_err(io::Error::other)
    }

    fn ready(&self) -> io::Result<()> {
        if self.fenced {
            Err(io::Error::other("managed storage requires reopen"))
        } else {
            Ok(())
        }
    }

    pub fn current_sequence(&self) -> u64 {
        self.tablet.current_sequence()
    }
    pub fn oldest_retained_snapshot(&self) -> u64 {
        self.tablet.oldest_retained_snapshot()
    }

    pub fn read_at(&self, key: &[u8], snapshot: u64) -> Result<Option<&[u8]>, TabletError> {
        if self.fenced {
            return Err(TabletError::RecoveryRequired);
        }
        self.tablet.read_at(key, snapshot)
    }

    pub fn pin_snapshot(&mut self, snapshot: u64) -> Result<SnapshotPin, TabletError> {
        if self.fenced {
            return Err(TabletError::RecoveryRequired);
        }
        self.tablet.pin_snapshot(snapshot)
    }

    pub fn commit_idempotent(
        &mut self,
        id: TransactionId,
        mutations: Vec<TabletMutation>,
    ) -> Result<TransactionReceipt, WalBackedError> {
        if self.fenced {
            return Err(TabletError::RecoveryRequired.into());
        }
        self.tablet.commit_idempotent(id, mutations)
    }

    pub fn generation_status(&self, manifest: &ManifestStore) -> io::Result<GenerationStatus> {
        self.ready()?;
        self.registry.status(manifest)
    }

    /// Reserve a reference before exposing the current generation to a consumer.
    pub fn register_reference(&mut self, kind: ReferenceKind, id: [u8; 16]) -> io::Result<()> {
        self.ready()?;
        let result = self.registry.register_reference(kind, id, &self.active);
        // Conservative: even a validation error requires reopening. Never
        // continue with potentially ambiguous registry publication.
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    pub fn release_reference(
        &mut self,
        kind: ReferenceKind,
        id: [u8; 16],
        manifest: &ManifestStore,
    ) -> io::Result<()> {
        self.ready()?;
        let result = self.registry.release_reference(kind, id, manifest);
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    /// Prepare, reserve, rotate, and reconcile under all three locks. Any
    /// failure after starting reservation fences this managed handle.
    pub fn rotate(&mut self, mut next: ManifestStore, floor: u64) -> io::Result<()> {
        self.ready()?;
        self.tablet
            .prepare_sst_successor(&self.active, &mut next, floor)?;
        self.fenced = true;
        self.tablet
            .reserve_generation_rotation(&mut self.registry, &self.active, &next)?;
        self.tablet.reclaim_to_sst_successor(&self.active, &next)?;
        self.active = next;
        self.tablet
            .reconcile_generation_wal(&mut self.registry, &self.active)?;
        self.fenced = false;
        Ok(())
    }

    /// Build and atomically bind a compacted sealed prefix at the existing
    /// floor and base sequence, preserving every acknowledged WAL tail frame.
    /// Both directories remain intact. Successful reconciliation records
    /// committed lineage for later explicitly backup-gated retirement.
    pub fn compact(
        &mut self,
        next: ManifestStore,
    ) -> io::Result<super::sst_manifest::CompactionStats> {
        self.compact_with_target(next, 256 * 1024 * 1024)
    }

    /// Compaction with an explicit per-SST encoded-byte ceiling. No transaction
    /// sequence is split between files; all outputs are verified before cutover.
    pub fn compact_with_target(
        &mut self,
        mut next: ManifestStore,
        target_bytes: u64,
    ) -> io::Result<super::sst_manifest::CompactionStats> {
        self.ready()?;
        let stats = self
            .active
            .prepare_compacted_shadow_with_target(&mut next, target_bytes)?;
        if self.active.wal_binding()? == next.wal_binding()? {
            return Err(io::Error::other(
                "compaction produced identical seal; no cutover needed",
            ));
        }
        self.fenced = true;
        self.tablet
            .reserve_generation_compaction(&mut self.registry, &self.active, &next)?;
        self.tablet
            .rebind_compacted_generation(&self.active, &next)?;
        self.active = next;
        self.tablet
            .reconcile_generation_wal(&mut self.registry, &self.active)?;
        self.fenced = false;
        Ok(stats)
    }

    /// The durable backup reservation precedes all file creation. On failure
    /// it intentionally remains until the operator explicitly releases it.
    pub fn backup(&mut self, id: [u8; 16], destination: impl AsRef<Path>) -> io::Result<()> {
        self.register_reference(ReferenceKind::Backup, id)?;
        self.tablet.backup_sealed(&self.active, destination)
    }

    /// Permanently fence new registry references to a verified obsolete
    /// generation. Operator supplies the already quarantined dedicated
    /// directory and a current complete backup. This does not prove global
    /// ownership, move directories, authorize unlink, or delete any files.
    /// All consumers must participate in this same registry.
    pub fn reserve_retirement(
        &mut self,
        obsolete: &ManifestStore,
        backup: impl AsRef<Path>,
    ) -> io::Result<()> {
        self.ready()?;
        match self.registry.status(obsolete)? {
            GenerationStatus::Unreferenced | GenerationStatus::RetirementReserved => {}
            _ => {
                return Err(io::Error::other(
                    "retirement requires known unreferenced generation",
                ))
            }
        }
        let _archive = self.tablet.verify_managed_retirement_candidate(
            &self.registry,
            &self.active,
            obsolete,
            backup.as_ref(),
        )?;
        let result = self.registry.reserve_retirement(obsolete);
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    /// Report the persisted retirement state by its exact physical directory.
    pub fn retirement_status(&self, directory: impl AsRef<Path>) -> io::Result<GenerationStatus> {
        self.ready()?;
        self.registry.retirement_status(directory.as_ref())
    }

    /// Permanently unlink only journaled SST/manifest files in an explicitly
    /// reserved, quarantined generation. The operator MUST ensure no external
    /// or legacy consumer depends on this directory, and prevent namespace
    /// modification throughout. Keep the directory and stable lock tombstone.
    /// On retry, revalidate the current backup and every remaining file.
    pub fn delete_reserved_generation(
        &mut self,
        directory: impl AsRef<Path>,
        backup: impl AsRef<Path>,
    ) -> io::Result<()> {
        self.ready()?;
        let directory = directory.as_ref();
        let status = self.registry.retirement_status(directory)?;
        let _archive = if status == GenerationStatus::RetirementReserved {
            let obsolete = ManifestStore::open(
                directory,
                self.tablet.descriptor().id().get(),
                self.tablet.descriptor().ownership_epoch(),
            )?;
            let archive = self.tablet.verify_managed_retirement_candidate(
                &self.registry,
                &self.active,
                &obsolete,
                backup.as_ref(),
            )?;
            let result = self.registry.prepare_deletion(&obsolete);
            if result.is_err() {
                self.fenced = true;
            }
            result?;
            // Release the manifest lock only after journal publication; the
            // deletion executor reacquires the exact recorded lock inode.
            drop(obsolete);
            archive
        } else {
            self.tablet.verify_current_retirement_backup(
                &self.active,
                directory,
                backup.as_ref(),
            )?
        };
        let result = self.registry.execute_deletion(directory);
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    /// Quarantine only a known unreferenced generation. Existing exact-backup,
    /// lineage, file and path checks still apply. No permanent deletion.
    pub fn quarantine(
        &self,
        obsolete: ManifestStore,
        backup: impl AsRef<Path>,
        target: impl AsRef<Path>,
    ) -> io::Result<()> {
        self.ready()?;
        if self.registry.status(&obsolete)? != GenerationStatus::Unreferenced {
            return Err(io::Error::other("generation is unknown or referenced"));
        }
        let _archive = self.tablet.verify_managed_retirement_candidate(
            &self.registry,
            &self.active,
            &obsolete,
            backup.as_ref(),
        )?;
        obsolete.quarantine(target.as_ref())
    }
}
