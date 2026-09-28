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
use std::path::Path;

use super::checkpoint::{load_checkpoint, write_checkpoint, CheckpointError};
use super::tablet::{MemoryTablet, TabletDescriptor, TabletError, TabletMutation, TabletWrite};
use super::wal::{FileWal, WalError};

#[derive(Debug)]
pub struct WalBackedTablet {
    tablet: MemoryTablet,
    wal: FileWal,
}

impl WalBackedTablet {
    /// Open a tablet and reconstruct its MVCC state from the valid WAL prefix.
    pub fn open(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
    ) -> Result<Self, WalBackedError> {
        let wal = FileWal::open(wal_path)?;
        let tablet = wal.recover_memory_tablet(descriptor)?;
        Ok(Self { tablet, wal })
    }

    /// Open a tablet using an optional checkpoint plus its WAL.
    ///
    /// The checkpoint is authoritative through its sequence. The WAL may
    /// still contain the full pre-checkpoint prefix (crash before reclamation)
    /// or a compacted suffix retaining the checkpoint record as an anchor.
    pub fn open_with_checkpoint(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
        checkpoint_path: impl AsRef<Path>,
    ) -> Result<Self, WalBackedError> {
        let wal = FileWal::open(wal_path)?;
        let tablet = match load_checkpoint(checkpoint_path, descriptor.clone())? {
            Some(mut tablet) => {
                wal.replay_into(&mut tablet)?;
                tablet
            }
            None => wal.recover_memory_tablet(descriptor)?,
        };
        Ok(Self { tablet, wal })
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

    /// Durably commit one write before making it visible to readers.
    pub fn commit(&mut self, write: TabletWrite) -> Result<u64, WalBackedError> {
        self.tablet.validate_write(&write)?;
        self.wal.append_write(&write)?;
        Ok(self.tablet.publish_validated(write))
    }

    /// Publish a checkpoint but deliberately leave the WAL unchanged.
    ///
    /// This is useful for validating the crash-safe ordering contract: a
    /// published checkpoint must recover correctly even if the process dies
    /// before WAL reclamation begins.
    pub fn write_checkpoint_only(
        &self,
        checkpoint_path: impl AsRef<Path>,
    ) -> Result<u64, WalBackedError> {
        if self.wal.is_poisoned() {
            return Err(WalError::Poisoned.into());
        }
        Ok(write_checkpoint(checkpoint_path, &self.tablet)?)
    }

    /// Atomically publish a checkpoint and then reclaim older WAL history.
    ///
    /// Reclamation happens only after checkpoint publication succeeds. If
    /// reclamation fails, the durable checkpoint plus the unreclaimed WAL
    /// remains a valid recovery state.
    pub fn checkpoint(&mut self, checkpoint_path: impl AsRef<Path>) -> Result<u64, WalBackedError> {
        if self.wal.is_poisoned() {
            return Err(WalError::Poisoned.into());
        }
        let sequence = write_checkpoint(checkpoint_path, &self.tablet)?;
        self.wal.reclaim_through(sequence)?;
        Ok(sequence)
    }

    pub fn read_at(&self, key: &[u8], snapshot: u64) -> Result<Option<&[u8]>, TabletError> {
        self.tablet.read_at(key, snapshot)
    }

    pub fn read_latest(&self, key: &[u8]) -> Option<&[u8]> {
        self.tablet.read_latest(key)
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
