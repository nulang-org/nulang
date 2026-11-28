//! Opt-in managed storage coordinator. Owns the tablet, registry and active
//! manifest locks for its lifetime. No raw mutable access or extraction.
//! Enrolled WALs require their exact local registry. Unenrolled WALs and
//! older binaries remain outside this scope; all
//! cooperating consumers must use the same dedicated registry and namespace.
//! This is not a permanent deletion authority.
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
        tablet: WalBackedTablet,
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
        let tablet = WalBackedTablet::open_with_sealed_manifest(
            descriptor,
            wal.as_ref(),
            &active,
            Some(registry.enrollment_identity()?),
        )?;
        tablet.reconcile_generation_wal(&mut registry, &active)?;
        Ok(Self {
            tablet,
            registry,
            active,
            fenced: false,
        })
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

    /// The durable backup reservation precedes all file creation. On failure
    /// it intentionally remains until the operator explicitly releases it.
    pub fn backup(&mut self, id: [u8; 16], destination: impl AsRef<Path>) -> io::Result<()> {
        self.register_reference(ReferenceKind::Backup, id)?;
        self.tablet.backup_sealed(&self.active, destination)
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
        self.tablet
            .quarantine_obsolete_sst(&self.active, obsolete, backup, target)
    }
}
