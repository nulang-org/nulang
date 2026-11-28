//! Cooperative durable reference registry for one tablet and ownership epoch.
//! A digest identifies sealed content, not a unique physical copy. All copies
//! with the same digest are conservatively protected. Unregistered consumers,
//! external processes and direct legacy APIs are outside this registry's scope.
//! Unreferenced is advisory, never a deletion authorization.
use super::sst_manifest::ManifestStore;
use super::wal::SealBinding;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: &[u8; 8] = b"NUDBREG1";
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ITEMS: usize = 8192;
const MAX_PATH: usize = 4096;
static NEXT: AtomicU64 = AtomicU64::new(0);
type Digest = [u8; 32];

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationStatus {
    Unknown,
    Referenced,
    Unreferenced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReferenceKind {
    Backup,
    Reader,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum Owner {
    Wal(Vec<u8>),
    External(ReferenceKind, [u8; 16]),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    owner: Owner,
    active: Digest,
    pending: Option<Digest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    tablet: u64,
    epoch: u64,
    known: BTreeSet<Digest>,
    references: Vec<Reference>,
}

impl State {
    fn validate(&self) -> io::Result<()> {
        if self.tablet == 0
            || self.epoch == 0
            || self.known.len() > MAX_ITEMS
            || self.references.len() > MAX_ITEMS
        {
            return Err(invalid("registry identity or count limit"));
        }
        let mut previous = None;
        for r in &self.references {
            if previous.is_some_and(|p| p >= &r.owner)
                || !self.known.contains(&r.active)
                || r.pending
                    .is_some_and(|p| p == r.active || !self.known.contains(&p))
            {
                return Err(invalid("invalid registry reference"));
            }
            match &r.owner {
                Owner::Wal(path)
                    if path.len() <= MAX_PATH
                        && path.first() == Some(&b'/')
                        && !path.contains(&0) => {}
                Owner::External(_, id) if *id != [0; 16] && r.pending.is_none() => {}
                _ => return Err(invalid("invalid registry owner")),
            }
            previous = Some(&r.owner);
        }
        Ok(())
    }

    fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        let mut bytes = MAGIC.to_vec();
        // Bound serialized growth rather than trusting record count alone.
        struct Bounded<'a>(&'a mut Vec<u8>);
        impl Write for Bounded<'_> {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.0.len().saturating_add(buf.len()).saturating_add(32) > MAX_BYTES {
                    return Err(invalid("registry byte limit"));
                }
                self.0.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(Bounded(&mut bytes), self).map_err(io::Error::other)?;
        let checksum = blake3::hash(&bytes);
        bytes.extend_from_slice(checksum.as_bytes());
        Ok(bytes)
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < 40 || bytes.len() > MAX_BYTES || &bytes[..8] != MAGIC {
            return Err(invalid("registry magic or length"));
        }
        let (body, checksum) = bytes.split_at(bytes.len() - 32);
        if blake3::hash(body).as_bytes() != checksum {
            return Err(invalid("registry checksum"));
        }
        let state: Self = serde_json::from_slice(&body[8..]).map_err(io::Error::other)?;
        if state.encode()? != bytes {
            return Err(invalid("noncanonical registry"));
        }
        Ok(state)
    }

    fn index(&self, owner: &Owner) -> Result<usize, usize> {
        self.references.binary_search_by(|r| r.owner.cmp(owner))
    }
}

#[derive(Debug)]
pub struct GenerationRegistry {
    dir: PathBuf,
    _lock: super::sidecar_lock::SidecarLock,
    state: State,
    requires_reopen: bool,
}

fn unique_file(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(invalid("registry requires unique regular file"));
    }
    Ok(())
}

fn lock(dir: &Path, initial: bool) -> io::Result<super::sidecar_lock::SidecarLock> {
    let path = dir.join("GENERATIONS.lock");
    let file = if initial {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?
    } else {
        unique_file(&path)?;
        OpenOptions::new().read(true).write(true).open(&path)?
    };
    super::sidecar_lock::SidecarLock::acquire(file, "registry already locked")
}

fn publish(dir: &Path, bytes: &[u8], initial: bool) -> io::Result<()> {
    let target = dir.join("GENERATIONS");
    if initial {
        match fs::symlink_metadata(&target) {
            Ok(_) => return Err(invalid("registry already exists")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    } else {
        unique_file(&target)?;
    }
    struct Stage(Option<PathBuf>);
    impl Drop for Stage {
        fn drop(&mut self) {
            if let Some(p) = &self.0 {
                let _ = fs::remove_file(p);
            }
        }
    }
    let mut staged = None;
    for _ in 0..64 {
        let path = dir.join(format!(
            ".GENERATIONS-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(f) => {
                staged = Some((Stage(Some(path)), f));
                break;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let (mut stage, mut f) = staged.ok_or_else(|| invalid("registry staging collision limit"))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    #[cfg(test)]
    super::interruption::hit(super::interruption::StorageInterruptionPoint::RegistryBeforeRename)?;
    drop(f);
    fs::rename(stage.0.as_ref().expect("stage"), &target)?;
    stage.0 = None;
    #[cfg(test)]
    super::interruption::hit(super::interruption::StorageInterruptionPoint::RegistryAfterRename)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

impl GenerationRegistry {
    /// Initialize only in a fresh, empty dedicated directory. A missing
    /// registry in a previously initialized directory is not an empty state.
    pub fn create(dir: impl AsRef<Path>, tablet: u64, epoch: u64) -> io::Result<Self> {
        let dir = dir.as_ref().canonicalize()?;
        let state = State {
            tablet,
            epoch,
            known: BTreeSet::new(),
            references: Vec::new(),
        };
        let bytes = state.encode()?;
        if fs::read_dir(&dir)?.next().is_some() {
            return Err(invalid("registry directory not empty"));
        }
        let lock = lock(&dir, true)?;
        publish(&dir, &bytes, true)?;
        File::open(
            dir.parent()
                .ok_or_else(|| invalid("registry has no parent"))?,
        )?
        .sync_all()?;
        Ok(Self {
            dir,
            _lock: lock,
            state,
            requires_reopen: false,
        })
    }

    pub fn open(dir: impl AsRef<Path>, tablet: u64, epoch: u64) -> io::Result<Self> {
        let dir = dir.as_ref().canonicalize()?;
        let lock = lock(&dir, false)?;
        let path = dir.join("GENERATIONS");
        unique_file(&path)?;
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        let state = State::decode(&bytes)?;
        if state.tablet != tablet || state.epoch != epoch {
            return Err(invalid("registry scope mismatch"));
        }
        // A prior rename may have succeeded without its directory sync.
        File::open(dir.join("GENERATIONS"))?.sync_all()?;
        File::open(&dir)?.sync_all()?;
        File::open(
            dir.parent()
                .ok_or_else(|| invalid("registry has no parent"))?,
        )?
        .sync_all()?;
        Ok(Self {
            dir,
            _lock: lock,
            state,
            requires_reopen: false,
        })
    }

    fn ready(&self) -> io::Result<()> {
        if self.requires_reopen {
            Err(invalid("registry publication requires reopen"))
        } else {
            Ok(())
        }
    }

    fn digest(&self, binding: SealBinding) -> io::Result<Digest> {
        self.ready()?;
        if binding.tablet_id.get() != self.state.tablet || binding.epoch != self.state.epoch {
            return Err(invalid("registry binding scope mismatch"));
        }
        Ok(binding.digest)
    }

    fn update(&mut self, next: State) -> io::Result<()> {
        self.ready()?;
        let bytes = next.encode()?;
        if next == self.state {
            return Ok(());
        }
        if let Err(e) = publish(&self.dir, &bytes, false) {
            self.requires_reopen = true;
            return Err(e);
        }
        self.state = next;
        Ok(())
    }

    pub fn status(&self, manifest: &ManifestStore) -> io::Result<GenerationStatus> {
        let digest = self.digest(manifest.wal_binding()?)?;
        Ok(
            if self
                .state
                .references
                .iter()
                .any(|r| r.active == digest || r.pending == Some(digest))
            {
                GenerationStatus::Referenced
            } else if self.state.known.contains(&digest) {
                GenerationStatus::Unreferenced
            } else {
                GenerationStatus::Unknown
            },
        )
    }

    fn add(&mut self, owner: Owner, digest: Digest) -> io::Result<()> {
        let mut next = self.state.clone();
        match next.index(&owner) {
            Ok(i)
                if next.references[i].active == digest && next.references[i].pending.is_none() =>
            {
                return self.ready()
            }
            Ok(_) => {
                return Err(invalid(
                    "owner already registered to another generation or rotation pending",
                ))
            }
            Err(i) => next.references.insert(
                i,
                Reference {
                    owner,
                    active: digest,
                    pending: None,
                },
            ),
        }
        next.known.insert(digest);
        self.update(next)
    }

    /// Explicit durable reference, not a time-expiring lease. The caller
    /// must acquire before exposing a backup or reader and release after use.
    pub fn register_reference(
        &mut self,
        kind: ReferenceKind,
        id: [u8; 16],
        manifest: &ManifestStore,
    ) -> io::Result<()> {
        let digest = self.digest(manifest.sync_for_wal_binding()?)?;
        self.add(Owner::External(kind, id), digest)
    }

    pub fn release_reference(
        &mut self,
        kind: ReferenceKind,
        id: [u8; 16],
        manifest: &ManifestStore,
    ) -> io::Result<()> {
        let digest = self.digest(manifest.wal_binding()?)?;
        let mut next = self.state.clone();
        let i = next
            .index(&Owner::External(kind, id))
            .map_err(|_| invalid("reference not registered"))?;
        if next.references[i].active != digest {
            return Err(invalid("reference generation mismatch"));
        }
        next.references.remove(i);
        self.update(next)
    }

    fn wal_owner(path: &Path) -> Owner {
        Owner::Wal(path.as_os_str().as_bytes().to_vec())
    }

    pub(super) fn register_wal(&mut self, path: &Path, binding: SealBinding) -> io::Result<()> {
        let digest = self.digest(binding)?;
        self.add(Self::wal_owner(path), digest)
    }

    pub(super) fn reserve_rotation(
        &mut self,
        path: &Path,
        old: SealBinding,
        new: SealBinding,
    ) -> io::Result<()> {
        let old_digest = self.digest(old)?;
        let new_digest = self.digest(new)?;
        if new.sequence <= old.sequence || old_digest == new_digest {
            return Err(invalid("rotation must advance generation"));
        }
        let mut next = self.state.clone();
        let i = next
            .index(&Self::wal_owner(path))
            .map_err(|_| invalid("WAL not registered"))?;
        let r = &mut next.references[i];
        if r.active != old_digest || r.pending.is_some_and(|p| p != new_digest) {
            return Err(invalid("rotation does not match registered WAL"));
        }
        r.pending = Some(new_digest);
        next.known.insert(new_digest);
        self.update(next)
    }

    /// Only the live/reopened WAL coordinator can supply the observed binding.
    /// Resolving the old binding aborts the pending rotation; resolving the
    /// new binding commits it. Any third binding fails closed.
    pub(super) fn reconcile_wal(&mut self, path: &Path, observed: SealBinding) -> io::Result<()> {
        let digest = self.digest(observed)?;
        let mut next = self.state.clone();
        let i = next
            .index(&Self::wal_owner(path))
            .map_err(|_| invalid("WAL not registered"))?;
        let r = &mut next.references[i];
        if r.active != digest && r.pending != Some(digest) {
            return Err(invalid("observed WAL not in registered rotation"));
        }
        r.active = digest;
        r.pending = None;
        self.update(next)
    }
}

#[cfg(test)]
mod lock_lifetime_tests {
    use super::*;

    #[test]
    fn registry_logical_drop_releases_lock_with_duplicated_descriptor() {
        let dir = std::env::temp_dir().join(format!(
            "nudb-registry-lock-lifetime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let first = GenerationRegistry::create(&dir, 7, 3).unwrap();
        let inherited = first._lock.duplicate_descriptor().unwrap();
        assert_eq!(
            GenerationRegistry::open(&dir, 7, 3).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        let second = GenerationRegistry::open(&dir, 7, 3).unwrap();
        drop(inherited);
        assert_eq!(
            GenerationRegistry::open(&dir, 7, 3).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(second);
        drop(GenerationRegistry::open(&dir, 7, 3).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }
}
