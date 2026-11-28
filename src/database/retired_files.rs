//! Journaled local deletion of an explicitly reserved, dedicated generation.
//! The registry records the complete inventory before any unlink. Retain the
//! directory and its locked tombstone forever; never use recursive deletion.
use super::sidecar_lock::SidecarLock;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

const MAX_FILES: usize = 1025;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const LOCK_NAME: &str = "MANIFEST.lock";

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredFile {
    name: String,
    device: u64,
    inode: u64,
    len: u64,
    digest: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeletionPlan {
    lock_device: u64,
    lock_inode: u64,
    files: Vec<RetiredFile>,
    pub(super) complete: bool,
}

fn valid_name(name: &str) -> bool {
    name == "MANIFEST"
        || name
            .strip_prefix("nudb-sst-")
            .and_then(|s| s.strip_suffix(".sst"))
            .is_some_and(|s| {
                s.len() == 20
                    && s.bytes().all(|b| b.is_ascii_digit())
                    && s.parse::<u64>().is_ok_and(|n| n != 0)
            })
}

fn metadata(path: &Path) -> io::Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(invalid("retirement requires unique regular file"));
    }
    Ok(meta)
}

fn inspect(dir: &Path, name: &str) -> io::Result<RetiredFile> {
    if !valid_name(name) {
        return Err(invalid("invalid retirement file name"));
    }
    let path = dir.join(name);
    let meta = metadata(&path)?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(invalid("retirement file too large"));
    }
    let mut file = File::open(&path)?;
    let opened = file.metadata()?;
    if (opened.dev(), opened.ino(), opened.len()) != (meta.dev(), meta.ino(), meta.len()) {
        return Err(invalid("retirement file identity changed"));
    }
    let mut hash = blake3::Hasher::new();
    let mut buf = [0; 64 * 1024];
    let mut remaining = meta.len();
    while remaining != 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..n])?;
        hash.update(&buf[..n]);
        remaining -= n as u64;
    }
    if file.read(&mut buf[..1])? != 0 {
        return Err(invalid("retirement file grew"));
    }
    Ok(RetiredFile {
        name: name.into(),
        device: meta.dev(),
        inode: meta.ino(),
        len: meta.len(),
        digest: *hash.finalize().as_bytes(),
    })
}

impl DeletionPlan {
    /// Caller holds the verified manifest lock and has verified exact inventory.
    pub(super) fn capture(dir: &Path) -> io::Result<Self> {
        let lock = metadata(&dir.join(LOCK_NAME))?;
        if lock.len() != 0 {
            return Err(invalid("unexpected manifest lock contents"));
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(dir)? {
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| invalid("invalid retirement filename"))?;
            if name == LOCK_NAME {
                continue;
            }
            if files.len() >= MAX_FILES {
                return Err(invalid("retirement file count limit"));
            }
            files.push(inspect(dir, &name)?);
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        let plan = Self {
            lock_device: lock.dev(),
            lock_inode: lock.ino(),
            files,
            complete: false,
        };
        plan.validate()?;
        Ok(plan)
    }

    pub(super) fn validate(&self) -> io::Result<()> {
        if self.lock_inode == 0
            || self.files.len() < 2
            || self.files.len() > MAX_FILES
            || self.files.first().is_none_or(|f| f.name != "MANIFEST")
        {
            return Err(invalid("invalid retirement inventory"));
        }
        let mut previous = None;
        let mut identities =
            std::collections::BTreeSet::from([(self.lock_device, self.lock_inode)]);
        for f in &self.files {
            if !valid_name(&f.name)
                || previous.is_some_and(|p| p >= &f.name)
                || f.inode == 0
                || f.len > MAX_FILE_BYTES
                || !identities.insert((f.device, f.inode))
            {
                return Err(invalid("invalid retirement file identity"));
            }
            previous = Some(&f.name);
        }
        Ok(())
    }

    /// Caller holds registry, active generation, WAL and verified archive locks.
    /// Only trusted, offline/quiescent retired directory namespaces are supported.
    pub(super) fn execute(&self, dir: &Path, digest: [u8; 32]) -> io::Result<SidecarLock> {
        self.validate()?;
        let lock_path = dir.join(LOCK_NAME);
        let meta = metadata(&lock_path)?;
        if (meta.dev(), meta.ino()) != (self.lock_device, self.lock_inode) || meta.len() > 40 {
            return Err(invalid("retired manifest lock changed"));
        }
        let file = OpenOptions::new().read(true).write(true).open(&lock_path)?;
        let lock = SidecarLock::acquire(file.try_clone()?, "retired manifest still in use")?;
        let held = file.metadata()?;
        let named = metadata(&lock_path)?;
        if (held.dev(), held.ino()) != (self.lock_device, self.lock_inode)
            || (named.dev(), named.ino()) != (self.lock_device, self.lock_inode)
            || held.len() > 40
        {
            return Err(invalid("retired manifest lock changed"));
        }
        let mut marker = [0; 40];
        marker[..8].copy_from_slice(b"NUDBDEAD");
        marker[8..].copy_from_slice(&digest);
        let mut actual = [0; 40];
        let len = held.len() as usize;
        file.read_exact_at(&mut actual[..len], 0)?;
        if !marker.starts_with(&actual[..len]) || (self.complete && len != 40) {
            return Err(invalid("retired manifest tombstone mismatch"));
        }
        let mut present = std::collections::BTreeSet::new();
        // Validate ALL remaining files before any unlink. Missing files are
        // acceptable only after a full tombstone was published.
        for entry in fs::read_dir(dir)? {
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| invalid("unexpected retirement file"))?;
            if name == LOCK_NAME {
                continue;
            }
            if self.complete {
                return Err(invalid("completed retirement directory reused"));
            }
            let index = self
                .files
                .binary_search_by(|f| f.name.cmp(&name))
                .map_err(|_| invalid("unexpected retirement file"))?;
            if inspect(dir, &name)? != self.files[index] {
                return Err(invalid("retirement file changed"));
            }
            present.insert(name);
        }
        if len != 40 && present.len() != self.files.len() {
            return Err(invalid("missing file before retirement tombstone"));
        }
        if self.complete {
            return Ok(lock);
        }
        file.write_all_at(&marker, 0)?;
        file.sync_all()?;
        let directory = File::open(dir)?;
        directory.sync_all()?;
        #[cfg(test)]
        super::interruption::hit(
            super::interruption::StorageInterruptionPoint::RetirementAfterTombstone,
        )?;
        // Keep MANIFEST until last; the lock tombstone rejects ordinary
        // manifest open/create even if interrupted before its unlink.
        for f in self.files.iter().skip(1).chain(self.files.first()) {
            if !present.contains(&f.name) {
                continue;
            }
            if inspect(dir, &f.name)? != *f {
                return Err(invalid("retirement file changed before unlink"));
            }
            fs::remove_file(dir.join(&f.name))?;
            #[cfg(test)]
            super::interruption::hit(
                super::interruption::StorageInterruptionPoint::RetirementAfterUnlink,
            )?;
            directory.sync_all()?;
        }
        for entry in fs::read_dir(dir)? {
            if entry?.file_name() != LOCK_NAME {
                return Err(invalid("retirement directory changed during deletion"));
            }
        }
        directory.sync_all()?;
        #[cfg(test)]
        super::interruption::hit(
            super::interruption::StorageInterruptionPoint::RetirementAfterFilesSync,
        )?;
        Ok(lock)
    }
}
