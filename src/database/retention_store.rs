//! Experimental single-host durable retention pins.
//!
//! A registry persists consumer positions before the caller can rely on them.
//! Never substitute this for a distributed lease or use it as automatic
//! permission to reclaim WAL/MVCC history. No production reclaimer calls this.
//!
//! Operating assumptions: trusted *local* directory; cooperative single-host
//! exclusive file locking; no hostile rename or symlink replacement while in
//! use. Parent directory sync is required (Unix only).
//!
//! Every update is crash-conservative: a durable .pending marker is written
//! BEFORE replacing the complete checksummed snapshot. Reopen refuses to
//! interpret an ambiguous interrupted operation, including a successful
//! rename followed by an uncertain directory fsync. A human/recovery operator
//! must reconcile the consumer source of truth and reinstall a safe registry.
//! Never simply delete .pending and trust the resulting data automatically.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::retention::{Coverage, RetentionClass, RetentionPolicy};
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"NUDBPIN1";
const MAX_FILE: usize = 1024 * 1024;
const MAX_CONSUMERS: usize = 1024;
const VERSION: u16 = 1;

fn initial_wal_cursor() -> u64 {
    1
}

#[derive(Debug)]
pub enum RegistryError {
    Io(io::Error),
    Corrupt(&'static str),
    AmbiguousCommit,
    WriterLocked,
    UnsafePath,
    UnsupportedPlatform,
    IdentityMismatch,
    InvalidConsumer,
    InvalidCursor,
    AlreadyRegistered,
    MissingConsumer,
    CursorRegression,
    CapacityExceeded,
    Poisoned,
    RevisionOverflow,
    MissingRegistry,
    InvalidReclaimBound,
    ReclaimBlocked(String),
    HistoryUnavailable { earliest: u64, requested: u64 },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(out, "retention registry I/O failure: {e}"),
            Self::Corrupt(s) => write!(out, "retention registry corrupt: {s}"),
            Self::AmbiguousCommit => {
                out.write_str("interrupted retention update must be reconciled")
            }
            Self::WriterLocked => out.write_str("another retention owner holds the local lock"),
            Self::UnsafePath => out.write_str("unsafe retention file alias or parent path"),
            Self::UnsupportedPlatform => {
                out.write_str("durable registry requires Unix directory sync")
            }
            Self::IdentityMismatch => {
                out.write_str("tablet ID or owner epoch differs from registry")
            }
            Self::InvalidConsumer => out.write_str("invalid retention consumer ID"),
            Self::InvalidCursor => out.write_str("invalid retention cursor"),
            Self::AlreadyRegistered => out.write_str("retention consumer already registered"),
            Self::MissingConsumer => out.write_str("retention consumer not registered"),
            Self::CursorRegression => out.write_str("cannot rewind an acknowledged cursor"),
            Self::CapacityExceeded => {
                out.write_str("too many retention pins or snapshot too large")
            }
            Self::Poisoned => out.write_str("owner encountered an ambiguous write; reopen blocked"),
            Self::RevisionOverflow => out.write_str("retention revision exhausted"),
            Self::MissingRegistry => {
                out.write_str("durable registry missing after prior initialization")
            }
            Self::InvalidReclaimBound => out.write_str("invalid or overflowing reclaim watermark"),
            Self::ReclaimBlocked(reason) => write!(out, "retention watermark blocked: {reason}"),
            Self::HistoryUnavailable {
                earliest,
                requested,
            } => write!(
                out,
                "consumer requested WAL position {requested} before durable floor {earliest}"
            ),
        }
    }
}
impl std::error::Error for RegistryError {}
impl From<io::Error> for RegistryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug)]
struct RegistryState {
    tablet_id: u64,
    epoch: u64,
    revision: u64,
    minimum_wal_cursor: u64,
    certified: BTreeSet<RetentionClass>,
    pins: BTreeMap<(RetentionClass, String), u64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskState {
    version: u16,
    tablet_id: u64,
    ownership_epoch: u64,
    revision: u64,
    // Old NUDBPIN1 snapshots had no watermark; treating them as 1 is
    // conservative. Older binaries reject the new field and fail closed.
    #[serde(default = "initial_wal_cursor")]
    minimum_wal_cursor: u64,
    certified: Vec<RetentionClass>,
    pins: Vec<DiskPin>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskPin {
    class: RetentionClass,
    consumer: String,
    next_needed: u64,
}

impl RegistryState {
    fn encode(&self) -> Result<Vec<u8>, RegistryError> {
        let disk = DiskState {
            version: VERSION,
            tablet_id: self.tablet_id,
            ownership_epoch: self.epoch,
            revision: self.revision,
            minimum_wal_cursor: self.minimum_wal_cursor,
            certified: self.certified.iter().copied().collect(),
            pins: self
                .pins
                .iter()
                .map(|((class, consumer), &next_needed)| DiskPin {
                    class: *class,
                    consumer: consumer.clone(),
                    next_needed,
                })
                .collect(),
        };
        let payload = serde_json::to_vec(&disk)
            .map_err(|_| RegistryError::Corrupt("serialization failed"))?;
        if payload.len() > MAX_FILE - 44 {
            return Err(RegistryError::CapacityExceeded);
        }
        let mut bytes = Vec::with_capacity(payload.len() + 44);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(blake3::hash(&payload).as_bytes());
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, RegistryError> {
        if bytes.len() < 44 || &bytes[..8] != MAGIC {
            return Err(RegistryError::Corrupt("bad header"));
        }
        let len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        if len > MAX_FILE - 44 || bytes.len() != len + 44 {
            return Err(RegistryError::Corrupt(
                "invalid snapshot length or trailing bytes",
            ));
        }
        let payload = &bytes[12..12 + len];
        if bytes[12 + len..] != *blake3::hash(payload).as_bytes() {
            return Err(RegistryError::Corrupt("snapshot checksum mismatch"));
        }
        let disk: DiskState = serde_json::from_slice(payload)
            .map_err(|_| RegistryError::Corrupt("invalid payload schema"))?;
        if disk.version != VERSION
            || disk.tablet_id == 0
            || disk.ownership_epoch == 0
            || disk.minimum_wal_cursor == 0
        {
            return Err(RegistryError::Corrupt(
                "unsupported version or invalid identity",
            ));
        }
        if disk.pins.len() > MAX_CONSUMERS || disk.certified.len() > 5 {
            return Err(RegistryError::Corrupt("invalid pin capacity"));
        }
        let mut certified = BTreeSet::new();
        for class in disk.certified {
            if !certified.insert(class) {
                return Err(RegistryError::Corrupt("duplicate coverage entry"));
            }
        }
        let mut pins = BTreeMap::new();
        for pin in disk.pins {
            if !valid_consumer(&pin.consumer)
                || !valid_cursor(pin.class, pin.next_needed)
                || (pin.class != RetentionClass::Snapshot
                    && pin.next_needed < disk.minimum_wal_cursor)
            {
                return Err(RegistryError::Corrupt("invalid persisted pin"));
            }
            if pins
                .insert((pin.class, pin.consumer), pin.next_needed)
                .is_some()
            {
                return Err(RegistryError::Corrupt("duplicate consumer pin"));
            }
        }
        Ok(Self {
            tablet_id: disk.tablet_id,
            epoch: disk.ownership_epoch,
            revision: disk.revision,
            minimum_wal_cursor: disk.minimum_wal_cursor,
            certified,
            pins,
        })
    }
}

fn valid_consumer(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b':' | b'.'))
}
fn valid_cursor(class: RetentionClass, sequence: u64) -> bool {
    class == RetentionClass::Snapshot || sequence > 0
}

fn check_regular_unaliased(path: &Path) -> Result<bool, RegistryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(RegistryError::UnsafePath);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if metadata.nlink() != 1 {
                    return Err(RegistryError::UnsafePath);
                }
            }
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn new_private_file(path: &Path) -> Result<File, RegistryError> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn sync_directory(path: &Path) -> Result<(), RegistryError> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(RegistryError::UnsupportedPlatform)
    }
}

fn suffix(path: &Path, extension: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(extension);
    PathBuf::from(os)
}

fn read_snapshot(path: &Path) -> Result<RegistryState, RegistryError> {
    let size = fs::metadata(path)?.len();
    if size < 44 || size > MAX_FILE as u64 {
        return Err(RegistryError::Corrupt("invalid snapshot file size"));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    File::open(path)?
        .take(MAX_FILE as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != size {
        return Err(RegistryError::Corrupt("snapshot changed during read"));
    }
    RegistryState::decode(&bytes)
}

/// Durable local registry owner, fenced by a stable OS-locked sidecar.
/// Deleting the sidecar while an owner is alive breaks lock correctness.
/// External code must guarantee that certified coverage includes *every*
/// active/disconnected consumer of that class.
#[derive(Debug)]
pub struct DurableRetentionRegistry {
    path: PathBuf,
    parent: PathBuf,
    _lock: File,
    state: RegistryState,
    poisoned: bool,
}

impl DurableRetentionRegistry {
    /// Open or create a registry. A new registry starts with every source
    /// Unknown, never implicitly empty/complete. Durable operations and
    /// durability of the original WAL are independent.
    pub fn open(path: impl AsRef<Path>, tablet_id: u64, epoch: u64) -> Result<Self, RegistryError> {
        if tablet_id == 0 || epoch == 0 {
            return Err(RegistryError::IdentityMismatch);
        }
        #[cfg(not(unix))]
        {
            let _ = (path, tablet_id, epoch);
            return Err(RegistryError::UnsupportedPlatform);
        }
        #[cfg(unix)]
        {
            let requested = path.as_ref();
            let parent = requested
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            if fs::symlink_metadata(parent)?.file_type().is_symlink() {
                return Err(RegistryError::UnsafePath);
            }
            let parent = fs::canonicalize(parent)?;
            let name = requested.file_name().ok_or(RegistryError::UnsafePath)?;
            let path = parent.join(name);
            let lock_path = suffix(&path, ".lock");
            let pending_path = suffix(&path, ".pending");
            let stage_path = suffix(&path, ".stage");
            for file in [&path, &lock_path, &pending_path, &stage_path] {
                check_regular_unaliased(file)?;
            }
            // A surviving lock sidecar proves a previous registry existed.
            // Never silently recreate an empty registry if its authoritative
            // snapshot has disappeared: disconnected pins may be lost.
            let prior_lock = check_regular_unaliased(&lock_path)?;
            use std::os::unix::fs::OpenOptionsExt;
            let mut lock_options = OpenOptions::new();
            lock_options.create(true).read(true).write(true).mode(0o600);
            let lock = lock_options.open(&lock_path)?;
            lock.try_lock().map_err(|error| match error {
                fs::TryLockError::WouldBlock => RegistryError::WriterLocked,
                fs::TryLockError::Error(e) => RegistryError::Io(e),
            })?;
            if pending_path.exists() || stage_path.exists() {
                return Err(RegistryError::AmbiguousCommit);
            }
            let state = if check_regular_unaliased(&path)? {
                read_snapshot(&path)?
            } else {
                if prior_lock {
                    return Err(RegistryError::MissingRegistry);
                }
                let initial = RegistryState {
                    tablet_id,
                    epoch,
                    revision: 0,
                    minimum_wal_cursor: 1,
                    certified: BTreeSet::new(),
                    pins: BTreeMap::new(),
                };
                let mut file = new_private_file(&path)?;
                file.write_all(&initial.encode()?)?;
                file.sync_all()?;
                sync_directory(&parent)?;
                initial
            };
            if state.tablet_id != tablet_id || state.epoch != epoch {
                return Err(RegistryError::IdentityMismatch);
            }
            Ok(Self {
                path,
                parent,
                _lock: lock,
                state,
                poisoned: false,
            })
        }
    }

    pub fn revision(&self) -> u64 {
        self.state.revision
    }

    /// Minimum WAL first-needed cursor that a *new* consumer may register.
    /// A persisted reserve barrier must precede any physical WAL deletion.
    /// This getter never authorizes reclaim by itself.
    pub fn minimum_wal_cursor(&self) -> u64 {
        self.state.minimum_wal_cursor
    }

    /// Reserve a monotonic WAL reclamation barrier *before* physical cleanup.
    ///
    /// This does not inspect or delete WAL/checkpoint files. Callers must
    /// independently prove a durable checkpoint and coordinate all enrollment
    /// through this exclusive owner. Unknown coverage or a pin at/before the
    /// proposed boundary blocks the reservation. A crash during publication
    /// poisons the registry rather than authorizing stale registrations.
    pub fn reserve_reclaim_through(
        &mut self,
        reclaimed_through: u64,
        durable_tail: u64,
    ) -> Result<u64, RegistryError> {
        if self.poisoned {
            return Err(RegistryError::Poisoned);
        }
        if reclaimed_through > durable_tail || reclaimed_through == u64::MAX {
            return Err(RegistryError::InvalidReclaimBound);
        }
        let new_floor = reclaimed_through + 1;
        if new_floor <= self.state.minimum_wal_cursor {
            return Ok(self.state.revision);
        }
        let allowed = self
            .policy()
            .wal_reclaim_bound(Some(reclaimed_through), durable_tail)
            .map_err(|reason| RegistryError::ReclaimBlocked(format!("{reason:?}")))?;
        if allowed < reclaimed_through {
            return Err(RegistryError::ReclaimBlocked(format!(
                "requested {reclaimed_through} but pinned cursors only permit {allowed}"
            )));
        }
        let mut state = self.state.clone();
        state.minimum_wal_cursor = new_floor;
        self.persist(state)
    }

    pub fn consumer_cursor(
        &self,
        class: RetentionClass,
        consumer: &str,
    ) -> Result<Option<u64>, RegistryError> {
        if self.poisoned {
            return Err(RegistryError::Poisoned);
        }
        Ok(self.state.pins.get(&(class, consumer.to_string())).copied())
    }

    /// Only a certified class yields None/Earliest. Unknown classes stay
    /// Unknown even when pins have been registered provisionally.
    pub fn policy(&self) -> RetentionPolicy {
        let mut policy = RetentionPolicy::default();
        if self.poisoned {
            return policy;
        }
        for class in [
            RetentionClass::Snapshot,
            RetentionClass::Changefeed,
            RetentionClass::Backup,
            RetentionClass::Replica,
            RetentionClass::Branch,
        ] {
            if !self.state.certified.contains(&class) {
                continue;
            }
            let oldest = self
                .state
                .pins
                .iter()
                .filter_map(|((c, _), &value)| (*c == class).then_some(value))
                .min();
            policy.set_coverage(
                class,
                match oldest {
                    Some(sequence) => Coverage::Earliest(sequence),
                    None => Coverage::None,
                },
            );
        }
        policy
    }

    /// An external authoritative owner must prove complete enrollment of
    /// every live and disconnected consumer before certifying a class.
    /// This is NOT auto-discovery or a lease service.
    pub fn certify_complete(&mut self, class: RetentionClass) -> Result<u64, RegistryError> {
        if self.poisoned {
            return Err(RegistryError::Poisoned);
        }
        if self.state.certified.contains(&class) {
            return Ok(self.state.revision);
        }
        let mut state = self.state.clone();
        state.certified.insert(class);
        self.persist(state)
    }

    /// Register before relying on an externally held pin; callers must
    /// atomically coordinate this with creation of that consumer.
    pub fn register(
        &mut self,
        class: RetentionClass,
        consumer: &str,
        next_needed: u64,
    ) -> Result<u64, RegistryError> {
        if self.poisoned {
            return Err(RegistryError::Poisoned);
        }
        if !valid_consumer(consumer) {
            return Err(RegistryError::InvalidConsumer);
        }
        if !valid_cursor(class, next_needed) {
            return Err(RegistryError::InvalidCursor);
        }
        if class != RetentionClass::Snapshot && next_needed < self.state.minimum_wal_cursor {
            return Err(RegistryError::HistoryUnavailable {
                earliest: self.state.minimum_wal_cursor,
                requested: next_needed,
            });
        }
        let key = (class, consumer.to_string());
        if self.state.pins.contains_key(&key) {
            return Err(RegistryError::AlreadyRegistered);
        }
        if self.state.pins.len() >= MAX_CONSUMERS {
            return Err(RegistryError::CapacityExceeded);
        }
        let mut state = self.state.clone();
        state.pins.insert(key, next_needed);
        self.persist(state)
    }

    /// Only acknowledge a cursor after the downstream effect and its own
    /// resume state have become durable. A cursor cannot be rewound once
    /// earlier WAL history may have been reclaimed.
    pub fn advance(
        &mut self,
        class: RetentionClass,
        consumer: &str,
        next_needed: u64,
    ) -> Result<u64, RegistryError> {
        if self.poisoned {
            return Err(RegistryError::Poisoned);
        }
        if !valid_consumer(consumer) {
            return Err(RegistryError::InvalidConsumer);
        }
        if !valid_cursor(class, next_needed) {
            return Err(RegistryError::InvalidCursor);
        }
        let key = (class, consumer.to_string());
        let old = *self
            .state
            .pins
            .get(&key)
            .ok_or(RegistryError::MissingConsumer)?;
        if next_needed < old {
            return Err(RegistryError::CursorRegression);
        }
        if next_needed == old {
            return Ok(self.state.revision);
        }
        let mut state = self.state.clone();
        state.pins.insert(key, next_needed);
        self.persist(state)
    }

    /// Explicitly release a consumer ONLY after durable proof it no longer
    /// needs history. This is not automatic on disconnect/drop/lease expiry.
    pub fn release(&mut self, class: RetentionClass, consumer: &str) -> Result<u64, RegistryError> {
        if self.poisoned {
            return Err(RegistryError::Poisoned);
        }
        if !valid_consumer(consumer) {
            return Err(RegistryError::InvalidConsumer);
        }
        let mut state = self.state.clone();
        if state.pins.remove(&(class, consumer.to_string())).is_none() {
            return Err(RegistryError::MissingConsumer);
        }
        self.persist(state)
    }

    fn persist(&mut self, mut next: RegistryState) -> Result<u64, RegistryError> {
        if self.poisoned {
            return Err(RegistryError::Poisoned);
        }
        next.revision = self
            .state
            .revision
            .checked_add(1)
            .ok_or(RegistryError::RevisionOverflow)?;
        let payload = next.encode()?;
        self.poisoned = true; // conservative on any uncertain I/O below
        let pending_path = suffix(&self.path, ".pending");
        let stage_path = suffix(&self.path, ".stage");
        if check_regular_unaliased(&pending_path)? || check_regular_unaliased(&stage_path)? {
            return Err(RegistryError::AmbiguousCommit);
        }

        // The durable pending marker is a prepare barrier. Any interrupted
        // update after this point makes reopen fail closed, not replay an
        // unsafe possibly-released consumer set.
        let mut pending = new_private_file(&pending_path)?;
        pending.write_all(b"NUDBPIN-PENDING-1")?;
        pending.sync_all()?;
        sync_directory(&self.parent)?;
        drop(pending);

        let mut stage = new_private_file(&stage_path)?;
        stage.write_all(&payload)?;
        stage.sync_all()?;
        drop(stage);
        fs::rename(&stage_path, &self.path)?;
        sync_directory(&self.parent)?;
        fs::remove_file(&pending_path)?;
        sync_directory(&self.parent)?;
        self.state = next;
        self.poisoned = false;
        Ok(self.state.revision)
    }
}
