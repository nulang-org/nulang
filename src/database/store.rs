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
use super::split::OwnedDirectory;
use super::tablet::{
    MemoryTablet, TabletDescriptor, TabletError, TabletMutation, TabletScanRow, TabletSplitPlan,
    TabletWrite,
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
        let wal = FileWal::open(wal_path)?;
        Self::recover(descriptor, wal_path, wal)
    }

    /// Managed files are reachable only through a live coordinator's OS lock.
    pub(crate) fn open_managed(
        descriptor: TabletDescriptor,
        wal_path: impl AsRef<Path>,
        owner: &OwnedDirectory,
    ) -> Result<Self, WalBackedError> {
        let wal_path = wal_path.as_ref();
        let wal = FileWal::open_managed(wal_path, owner)?;
        Self::recover(descriptor, wal_path, wal)
    }

    fn recover(
        descriptor: TabletDescriptor,
        wal_path: &Path,
        wal: FileWal,
    ) -> Result<Self, WalBackedError> {
        let checkpoint_path = checkpoint::checkpoint_path_for_wal(wal_path);
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

    /// Return owned rows from one committed snapshot, suitable for a future
    /// Arrow/columnar batch adapter. This does not expose mutable tablet state.
    pub fn scan_at(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        snapshot: u64,
        limit: usize,
    ) -> Result<Vec<TabletScanRow>, TabletError> {
        self.tablet.scan_at(start, end, snapshot, limit)
    }

    /// Derive detached child MVCC tablets from the recovered durable state.
    ///
    /// No WAL, routing table or ownership fence is written here. A future
    /// split coordinator must durably publish the cutover before child writes
    /// are admitted or the parent is retired.
    pub fn materialize_split(
        &self,
        plan: &TabletSplitPlan,
    ) -> Result<(MemoryTablet, MemoryTablet), TabletError> {
        self.tablet.materialize_split(plan)
    }

    /// Atomically publish a checkpoint without reclaiming the WAL.
    ///
    /// This is a valid crash state and is intentionally public so operators can
    /// separate checkpoint publication from later space reclamation.
    pub fn publish_checkpoint(&self) -> Result<(), WalBackedError> {
        // Prevent owner takeover between the marker check and the durable
        // checkpoint rename/fsync. The guard lives until publication ends.
        let _gate = self.wal.acquire_public_io_gate()?;
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

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use super::super::tablet::{KeyRange, TabletId};
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn public_commit_retains_gate_after_wal_fsync_until_mvcc_publish() {
        let root = std::env::temp_dir().join(format!(
            "nudb_store_ack_gate_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let descriptor = TabletDescriptor::new(
            TabletId::new(941).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            3,
        )
        .unwrap();
        let mut tablet = WalBackedTablet::open(descriptor, root.join("ordinary.wal")).unwrap();
        let write = tablet
            .prepare_write(
                3,
                0,
                vec![TabletMutation::Put {
                    key: b"b".to_vec(),
                    value: b"ack".to_vec(),
                }],
            )
            .unwrap();

        tablet
            .commit_inner(write, || {
                // This callback executes after the durable WAL sync and
                // before the in-memory MVCC publication and acknowledgement.
                let contender = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(root.join(".nudb-write-gate.lock"))
                    .unwrap();
                assert_eq!(
                    contender.try_lock().unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
            })
            .unwrap();
        assert_eq!(tablet.read_latest(b"b"), Some(&b"ack"[..]));
        drop(tablet);
        let _ = fs::remove_dir_all(root);
    }
}
