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

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use super::checkpoint::{self, CheckpointError};
use super::tablet::{MemoryTablet, TabletDescriptor, TabletError, TabletMutation, TabletWrite};
use super::wal::{FileWal, WalError};

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
        }
    }
}

impl std::error::Error for WalBackedError {}
