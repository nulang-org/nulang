//! Single-node, one-owner NuDB tablet split publication.
//!
//! The durable decision is a checksummed, atomically replaced routing manifest:
//! while it names the parent, prepared child files are uncommitted staging;
//! once it names children, recovery must open both children or fail closed.
//! This coordinator requires exclusive ownership of its directory. It does
//! NOT provide multi-process leases, distributed fencing, Raft or atomic
//! multi-tablet transactions.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::checkpoint::{self, CheckpointError};
use super::store::{WalBackedError, WalBackedTablet};
use super::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletError, TabletId, TabletMutation,
    TabletSplitPlan,
};
use super::wal::{FileWal, WalError};

const MANIFEST_MAGIC: &[u8; 8] = b"NUDBRT01";
const MANIFEST_VERSION: u16 = 1;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DiskDescriptor {
    id: u64,
    range_start: Vec<u8>,
    range_end: Option<Vec<u8>>,
    ownership_epoch: u64,
}

impl DiskDescriptor {
    fn from_tablet(descriptor: &TabletDescriptor) -> Self {
        Self {
            id: descriptor.id().get(),
            range_start: descriptor.range().start().to_vec(),
            range_end: descriptor.range().end().map(ToOwned::to_owned),
            ownership_epoch: descriptor.ownership_epoch(),
        }
    }

    fn into_tablet(self) -> Result<TabletDescriptor, SplitError> {
        let id = TabletId::new(self.id)?;
        let range = KeyRange::new(self.range_start, self.range_end)?;
        Ok(TabletDescriptor::new(id, range, self.ownership_epoch)?)
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DiskSplit {
    split_key: Vec<u8>,
    left: DiskDescriptor,
    right: DiskDescriptor,
    source_sequence: u64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DiskManifest {
    version: u16,
    parent: DiskDescriptor,
    split: Option<DiskSplit>,
}

impl DiskManifest {
    fn for_parent(parent: &TabletDescriptor) -> Self {
        Self {
            version: MANIFEST_VERSION,
            parent: DiskDescriptor::from_tablet(parent),
            split: None,
        }
    }

    fn for_split(plan: &TabletSplitPlan, source_sequence: u64) -> Self {
        Self {
            version: MANIFEST_VERSION,
            parent: DiskDescriptor::from_tablet(&plan.source),
            split: Some(DiskSplit {
                split_key: plan.split_key.clone(),
                left: DiskDescriptor::from_tablet(&plan.left),
                right: DiskDescriptor::from_tablet(&plan.right),
                source_sequence,
            }),
        }
    }

    fn validate(
        self,
        requested_parent: &TabletDescriptor,
    ) -> Result<Option<(TabletSplitPlan, u64)>, SplitError> {
        if self.version != MANIFEST_VERSION
            || self.parent != DiskDescriptor::from_tablet(requested_parent)
        {
            return Err(SplitError::InvalidManifest(
                "unsupported manifest version or parent descriptor mismatch".into(),
            ));
        }
        let Some(split) = self.split else {
            return Ok(None);
        };
        let left = split.left.into_tablet()?;
        let right = split.right.into_tablet()?;
        let plan = requested_parent.plan_split(
            &split.split_key,
            left.id(),
            right.id(),
            left.ownership_epoch(),
        )?;
        if plan.left != left || plan.right != right {
            return Err(SplitError::InvalidManifest(
                "split children are not a canonical partition of parent".into(),
            ));
        }
        Ok(Some((plan, split.source_sequence)))
    }
}

#[derive(Debug)]
enum Active {
    Parent(WalBackedTablet),
    Children {
        split_key: Vec<u8>,
        left: WalBackedTablet,
        right: WalBackedTablet,
    },
    // Once catalog publication begins, any error might occur after rename.
    // Do not serve stale parent writes until reopening the durable manifest.
    Poisoned,
}

/// The only crate-internal authority that can open WALs inside a managed root.
/// The OS lock inode remains on disk even after this handle is dropped.
#[derive(Debug)]
pub(crate) struct OwnedDirectory {
    root: PathBuf,
    // Public WAL I/O must lock this inode during every mutation. Owning it
    // for the entire coordinator lifetime serializes acquisition with old
    // handles whose marker check preceded adoption.
    _io_gate: File,
    _lock: File,
}

impl OwnedDirectory {
    fn acquire(root: &Path) -> Result<Self, SplitError> {
        // Lock order is always write-gate -> owner. Public WAL handles only
        // lock the write-gate, so they cannot race with owner publication.
        let gate = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(root.join(".nudb-write-gate.lock"))?;
        match gate.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(SplitError::OwnerBusy),
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(root.join(".nudb-owner.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                root: root.to_path_buf(),
                _io_gate: gate,
                _lock: file,
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(SplitError::OwnerBusy),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    /// The coordinator constructs all tablet WAL paths directly inside its
    /// canonical root; a token cannot be reused for another directory.
    pub(crate) fn authorizes(&self, path: &Path) -> bool {
        path.parent() == Some(self.root.as_path())
    }
}

/// Single-node coordinator with an exclusive, advisory OS file lock.
///
/// Public WAL APIs reject managed directories; only this coordinator's
/// private managed-open path is authorized to mutate their tablet files.
/// Distributed fencing and non-cooperating filesystem writers remain out
/// of scope.
#[derive(Debug)]
pub struct SingleNodeSplitStore {
    root: PathBuf,
    parent: TabletDescriptor,
    active: Active,
    // Never delete or rename the lock file, including on Drop. Locking a new
    // inode would allow another process to own the old inode simultaneously.
    _owner_lock: OwnedDirectory,
}

impl SingleNodeSplitStore {
    pub fn open(root: impl AsRef<Path>, parent: TabletDescriptor) -> Result<Self, SplitError> {
        let root = root.as_ref();
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        // The advisory lock is acquired before reading or initializing
        // either the routing manifest or any tablet WAL.
        let owner_lock = OwnedDirectory::acquire(&root)?;
        let manifest_path = root.join("route.manifest");
        let parent_wal = root.join("parent.wal");
        let active = if manifest_path.exists() {
            let manifest = read_manifest(&manifest_path)?;
            match manifest.validate(&parent)? {
                None => {
                    verify_published_wal(&parent_wal)?;
                    Active::Parent(WalBackedTablet::open_managed(
                        parent.clone(),
                        &parent_wal,
                        &owner_lock,
                    )?)
                }
                Some((plan, sequence)) => {
                    // Never silently fall back to the parent after promotion.
                    for child in ["left", "right"] {
                        verify_published_wal(&root.join(format!("{child}.wal")))?;
                        if !root.join(format!("{child}.checkpoint")).exists() {
                            return Err(SplitError::InvalidManifest(
                                "published child checkpoint is missing".into(),
                            ));
                        }
                    }
                    let left = WalBackedTablet::open_managed(
                        plan.left,
                        root.join("left.wal"),
                        &owner_lock,
                    )?;
                    let right = WalBackedTablet::open_managed(
                        plan.right,
                        root.join("right.wal"),
                        &owner_lock,
                    )?;
                    if left.current_sequence() < sequence || right.current_sequence() < sequence {
                        return Err(SplitError::InvalidManifest(
                            "child state regressed behind split source sequence".into(),
                        ));
                    }
                    Active::Children {
                        split_key: plan.split_key,
                        left,
                        right,
                    }
                }
            }
        } else {
            // A missing manifest is not proof of a fresh store. In particular,
            // a lost manifest after promotion cannot resurrect an old parent.
            if parent_wal.exists()
                || [
                    "left.wal",
                    "right.wal",
                    "left.checkpoint",
                    "right.checkpoint",
                ]
                .iter()
                .any(|file| root.join(file).exists())
            {
                return Err(SplitError::InvalidManifest(
                    "tablet files exist without routing manifest".into(),
                ));
            }
            let parent_tablet =
                WalBackedTablet::open_managed(parent.clone(), &parent_wal, &owner_lock)?;
            write_manifest(&manifest_path, &DiskManifest::for_parent(&parent), None)?;
            Active::Parent(parent_tablet)
        };
        Ok(Self {
            root,
            parent,
            active,
            _owner_lock: owner_lock,
        })
    }

    pub fn is_split(&self) -> bool {
        matches!(&self.active, Active::Children { .. })
    }

    fn select(&self, key: &[u8]) -> Result<&WalBackedTablet, SplitError> {
        if !self.parent.range().contains(key) {
            return Err(SplitError::OutsideParentRange);
        }
        match &self.active {
            Active::Parent(parent) => Ok(parent),
            Active::Children {
                split_key,
                left,
                right,
            } => {
                if key < split_key.as_slice() {
                    Ok(left)
                } else {
                    Ok(right)
                }
            }
            Active::Poisoned => Err(SplitError::Poisoned),
        }
    }

    fn select_mut(&mut self, key: &[u8]) -> Result<&mut WalBackedTablet, SplitError> {
        if !self.parent.range().contains(key) {
            return Err(SplitError::OutsideParentRange);
        }
        match &mut self.active {
            Active::Parent(parent) => Ok(parent),
            Active::Children {
                split_key,
                left,
                right,
            } => {
                if key < split_key.as_slice() {
                    Ok(left)
                } else {
                    Ok(right)
                }
            }
            Active::Poisoned => Err(SplitError::Poisoned),
        }
    }

    /// One mutation per call. This does not imply distributed transactions
    /// between different tablets after a split.
    pub fn commit(&mut self, mutation: TabletMutation) -> Result<u64, SplitError> {
        let target = self.select_mut(mutation.key())?;
        let epoch = target.descriptor().ownership_epoch();
        let previous = target.current_sequence();
        let write = target.prepare_write(epoch, previous, vec![mutation])?;
        Ok(target.commit(write)?)
    }

    pub fn read_latest(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SplitError> {
        Ok(self.select(key)?.read_latest(key).map(ToOwned::to_owned))
    }

    /// Snapshot numbers are tablet-local, not a cross-tablet transaction ID.
    pub fn read_at(&self, key: &[u8], snapshot: u64) -> Result<Option<Vec<u8>>, SplitError> {
        Ok(self
            .select(key)?
            .read_at(key, snapshot)?
            .map(ToOwned::to_owned))
    }

    /// Prepare both durable child checkpoints/WALs, verify their recovery,
    /// then publish exactly one durable routing decision by atomic rename.
    pub fn split(&mut self, plan: &TabletSplitPlan) -> Result<(), SplitError> {
        self.split_inner(plan, None)
    }

    fn split_inner(
        &mut self,
        plan: &TabletSplitPlan,
        stop: Option<SplitStop>,
    ) -> Result<(), SplitError> {
        let parent = match &self.active {
            Active::Parent(parent) => parent,
            Active::Children { .. } => return Err(SplitError::AlreadySplit),
            Active::Poisoned => return Err(SplitError::Poisoned),
        };
        if &plan.source != parent.descriptor() {
            return Err(SplitError::InvalidManifest(
                "split source descriptor does not match active parent".into(),
            ));
        }
        let (left_state, right_state) = parent.materialize_split(plan)?;
        let source_sequence = parent.current_sequence();

        // Only the parent manifest is authoritative before publication. Any
        // earlier interrupted child files are uncommitted and can be replaced.
        for file in [
            "left.wal",
            "left.checkpoint",
            "right.wal",
            "right.checkpoint",
        ] {
            match fs::remove_file(self.root.join(file)) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        sync_directory(&self.root)?;

        let left = self.stage_child("left", &left_state)?;
        if stop == Some(SplitStop::AfterLeft) {
            return Err(SplitError::Interrupted("after left child staging"));
        }
        let right = self.stage_child("right", &right_state)?;
        if stop == Some(SplitStop::AfterRight) {
            return Err(SplitError::Interrupted("after right child staging"));
        }

        // Publication is a point of no return: a fsync error after rename is
        // ambiguous to this process. Mark it poisoned before attempting it.
        self.active = Active::Poisoned;
        let new_manifest = DiskManifest::for_split(plan, source_sequence);
        write_manifest(&self.root.join("route.manifest"), &new_manifest, stop)?;
        self.active = Active::Children {
            split_key: plan.split_key.clone(),
            left,
            right,
        };
        Ok(())
    }

    fn stage_child(
        &self,
        side: &str,
        tablet: &MemoryTablet,
    ) -> Result<WalBackedTablet, SplitError> {
        let wal_path = self.root.join(format!("{side}.wal"));
        let checkpoint_path = checkpoint::checkpoint_path_for_wal(&wal_path);
        checkpoint::write_checkpoint(&checkpoint_path, tablet)?;
        FileWal::seed_from_checkpoint(
            &wal_path,
            tablet.descriptor(),
            tablet.current_sequence(),
            &self._owner_lock,
        )?;
        Ok(WalBackedTablet::open_managed(
            tablet.descriptor().clone(),
            wal_path,
            &self._owner_lock,
        )?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitStop {
    AfterLeft,
    AfterRight,
    AfterManifestTempSync,
    AfterManifestRename,
}

fn write_manifest(
    path: &Path,
    manifest: &DiskManifest,
    interrupt: Option<SplitStop>,
) -> Result<(), SplitError> {
    let payload = serde_json::to_vec(manifest)
        .map_err(|error| SplitError::InvalidManifest(error.to_string()))?;
    if payload.len() > MAX_MANIFEST_BYTES {
        return Err(SplitError::InvalidManifest("manifest too large".into()));
    }
    let temp_path = path.with_extension("manifest.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp_path)?;
    file.write_all(MANIFEST_MAGIC)?;
    file.write_all(&(payload.len() as u32).to_le_bytes())?;
    file.write_all(&payload)?;
    file.write_all(blake3::hash(&payload).as_bytes())?;
    file.sync_data()?;
    drop(file);
    if interrupt == Some(SplitStop::AfterManifestTempSync) {
        return Err(SplitError::Interrupted("after manifest temp fsync"));
    }
    fs::rename(&temp_path, path)?;
    if interrupt == Some(SplitStop::AfterManifestRename) {
        return Err(SplitError::Interrupted("after routing manifest rename"));
    }
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(())
}

fn read_manifest(path: &Path) -> Result<DiskManifest, SplitError> {
    let mut file = File::open(path)?;
    let mut magic = [0; 8];
    file.read_exact(&mut magic)?;
    if &magic != MANIFEST_MAGIC {
        return Err(SplitError::InvalidManifest("invalid manifest magic".into()));
    }
    let mut len = [0; 4];
    file.read_exact(&mut len)?;
    let size = u32::from_le_bytes(len) as usize;
    if size > MAX_MANIFEST_BYTES {
        return Err(SplitError::InvalidManifest(
            "manifest exceeds size limit".into(),
        ));
    }
    let mut payload = vec![0; size];
    file.read_exact(&mut payload)?;
    let mut checksum = [0; 32];
    file.read_exact(&mut checksum)?;
    if checksum != *blake3::hash(&payload).as_bytes() {
        return Err(SplitError::InvalidManifest(
            "manifest checksum mismatch".into(),
        ));
    }
    let mut trailer = [0; 1];
    if file.read(&mut trailer)? != 0 {
        return Err(SplitError::InvalidManifest(
            "manifest has trailing bytes".into(),
        ));
    }
    serde_json::from_slice(&payload).map_err(|error| SplitError::InvalidManifest(error.to_string()))
}

/// FileWal::open intentionally creates/reinitializes empty files for fresh
/// stores. Published catalog entries may not use that path: an absent or
/// truncated WAL must fail closed rather than become an empty live tablet.
fn verify_published_wal(path: &Path) -> Result<(), SplitError> {
    let size = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(SplitError::InvalidManifest(
                "published tablet WAL is missing".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    // NuDB WAL header: magic 8 + base sequence 8 + tablet ID 8
    // + ownership epoch 8 + BLAKE3 digest 32.
    if size < 64 {
        return Err(SplitError::InvalidManifest(
            "published tablet WAL header is missing or truncated".into(),
        ));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Debug)]
pub enum SplitError {
    Io(io::Error),
    Tablet(TabletError),
    Wal(WalError),
    Checkpoint(CheckpointError),
    Storage(WalBackedError),
    InvalidManifest(String),
    OutsideParentRange,
    AlreadySplit,
    OwnerBusy,
    Poisoned,
    Interrupted(&'static str),
}

impl From<io::Error> for SplitError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<TabletError> for SplitError {
    fn from(error: TabletError) -> Self {
        Self::Tablet(error)
    }
}
impl From<WalError> for SplitError {
    fn from(error: WalError) -> Self {
        Self::Wal(error)
    }
}
impl From<CheckpointError> for SplitError {
    fn from(error: CheckpointError) -> Self {
        Self::Checkpoint(error)
    }
}
impl From<WalBackedError> for SplitError {
    fn from(error: WalBackedError) -> Self {
        Self::Storage(error)
    }
}
impl fmt::Display for SplitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "NuDB split I/O failure: {error}"),
            Self::Tablet(error) => write!(f, "NuDB split tablet rejected: {error}"),
            Self::Wal(error) => write!(f, "NuDB split WAL failure: {error}"),
            Self::Checkpoint(error) => write!(f, "NuDB split checkpoint failure: {error}"),
            Self::Storage(error) => write!(f, "NuDB split storage failure: {error}"),
            Self::InvalidManifest(message) => write!(f, "invalid routing manifest: {message}"),
            Self::OutsideParentRange => f.write_str("key outside source tablet range"),
            Self::AlreadySplit => f.write_str("tablet has already been split"),
            Self::OwnerBusy => {
                f.write_str("another process currently owns this NuDB tablet directory")
            }
            Self::Poisoned => f.write_str("tablet routing must be reopened after ambiguous split"),
            Self::Interrupted(message) => write!(f, "injected NuDB split interruption: {message}"),
        }
    }
}
impl std::error::Error for SplitError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn case(stop: SplitStop) {
        let root = std::env::temp_dir().join(format!(
            "nudb_split_interrupt_{}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            stop as u8,
        ));
        let parent = TabletDescriptor::new(
            TabletId::new(801).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            7,
        )
        .unwrap();
        let plan = parent
            .plan_split(
                b"m",
                TabletId::new(802).unwrap(),
                TabletId::new(803).unwrap(),
                8,
            )
            .unwrap();
        {
            let mut store = SingleNodeSplitStore::open(&root, parent.clone()).unwrap();
            store
                .commit(TabletMutation::Put {
                    key: b"b".to_vec(),
                    value: b"left".to_vec(),
                })
                .unwrap();
            store
                .commit(TabletMutation::Put {
                    key: b"n".to_vec(),
                    value: b"right".to_vec(),
                })
                .unwrap();
            assert!(matches!(
                store.split_inner(&plan, Some(stop)),
                Err(SplitError::Interrupted(_))
            ));
            if stop == SplitStop::AfterManifestRename {
                assert!(matches!(store.read_latest(b"b"), Err(SplitError::Poisoned)));
            }
        }

        let mut recovered = SingleNodeSplitStore::open(&root, parent.clone()).unwrap();
        let published = stop == SplitStop::AfterManifestRename;
        assert_eq!(recovered.is_split(), published);
        assert_eq!(recovered.read_latest(b"b").unwrap(), Some(b"left".to_vec()));
        assert_eq!(
            recovered.read_latest(b"n").unwrap(),
            Some(b"right".to_vec())
        );
        if !published {
            // Orphaned staging does not route reads or block a retried split.
            recovered.split(&plan).unwrap();
            assert!(recovered.is_split());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn crash_after_left_checkpoint_recovers_parent() {
        case(SplitStop::AfterLeft);
    }

    #[test]
    fn crash_after_right_checkpoint_recovers_parent() {
        case(SplitStop::AfterRight);
    }

    #[test]
    fn crash_after_manifest_temp_sync_keeps_parent_active() {
        case(SplitStop::AfterManifestTempSync);
    }

    #[test]
    fn crash_after_manifest_rename_recovers_children() {
        case(SplitStop::AfterManifestRename);
    }

    /// Run only in an isolated child process. `exit` deliberately skips Rust
    /// destructors and releases the OS lock on process termination.
    #[test]
    #[ignore]
    fn abrupt_exit_during_split_fixture() {
        let root = std::env::var_os("NUDB_FAILSTOP_ROOT").unwrap();
        let stop = match std::env::var("NUDB_FAILSTOP_STAGE").unwrap().as_str() {
            "left" => SplitStop::AfterLeft,
            "right" => SplitStop::AfterRight,
            "manifest_sync" => SplitStop::AfterManifestTempSync,
            "manifest_rename" => SplitStop::AfterManifestRename,
            stage => panic!("unrecognized fail-stop stage: {stage}"),
        };
        let parent = TabletDescriptor::new(
            TabletId::new(801).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            7,
        )
        .unwrap();
        let plan = parent
            .plan_split(
                b"m",
                TabletId::new(802).unwrap(),
                TabletId::new(803).unwrap(),
                8,
            )
            .unwrap();
        let mut store = SingleNodeSplitStore::open(root, parent).unwrap();
        assert!(matches!(
            store.split_inner(&plan, Some(stop)),
            Err(SplitError::Interrupted(_))
        ));
        std::process::exit(72);
    }

    #[test]
    fn real_process_exit_recovers_parent_or_children_at_each_cutover_boundary() {
        use std::process::Command;

        for (stage, published) in [
            ("left", false),
            ("right", false),
            ("manifest_sync", false),
            ("manifest_rename", true),
        ] {
            let root = std::env::temp_dir().join(format!(
                "nudb_failstop_split_{}_{}_{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                stage,
            ));
            let parent = TabletDescriptor::new(
                TabletId::new(801).unwrap(),
                KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
                7,
            )
            .unwrap();
            {
                let mut store = SingleNodeSplitStore::open(&root, parent.clone()).unwrap();
                for (key, value) in [(&b"b"[..], &b"left"[..]), (&b"n"[..], &b"right"[..])] {
                    store
                        .commit(TabletMutation::Put {
                            key: key.to_vec(),
                            value: value.to_vec(),
                        })
                        .unwrap();
                }
            }

            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "database::split::tests::abrupt_exit_during_split_fixture",
                    "--ignored",
                    "--nocapture",
                ])
                .env("NUDB_FAILSTOP_ROOT", &root)
                .env("NUDB_FAILSTOP_STAGE", stage)
                .output()
                .unwrap();
            assert_eq!(
                child.status.code(),
                Some(72),
                "child must exit at {stage}: stdout={} stderr={}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
            // The child never ran Drop. The OS releases its advisory lock;
            // recovery must follow the catalog, not whichever tablet files
            // happen to be present.
            let mut recovered = SingleNodeSplitStore::open(&root, parent.clone()).unwrap();
            assert_eq!(recovered.is_split(), published, "stage={stage}");
            assert_eq!(recovered.read_latest(b"b").unwrap(), Some(b"left".to_vec()));
            assert_eq!(
                recovered.read_latest(b"n").unwrap(),
                Some(b"right".to_vec())
            );
            let plan = parent
                .plan_split(
                    b"m",
                    TabletId::new(802).unwrap(),
                    TabletId::new(803).unwrap(),
                    8,
                )
                .unwrap();
            if !published {
                recovered.split(&plan).unwrap();
            }
            assert!(recovered.is_split());
            drop(recovered);
            let _ = fs::remove_dir_all(root);
        }
    }
}
