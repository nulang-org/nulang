//! Shared single-host admission lock for NuDB retention activation and WAL
//! reclamation. All cooperative callers use one persistent lock inode:
//! `<wal>.pins.admission.lock`. It must never be unlinked while live.
//!
//! This closes the registry-activation versus low-level-WAL-reclaim race when
//! both users cooperate. It does not protect against hostile directory entry
//! replacement, a deleted live lock sidecar, or unreliable network FS locks.
//! Both owners use non-blocking exclusive admission; contention fails closed.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub(crate) fn registry_path_for_wal(wal_path: &Path) -> PathBuf {
    let mut name = wal_path.as_os_str().to_os_string();
    name.push(".pins");
    PathBuf::from(name)
}

pub(crate) fn suffix(path: &Path, extension: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(extension);
    PathBuf::from(name)
}

fn reject_alias(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsafe retention admission lock type",
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "aliased retention admission lock",
                    ));
                }
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub(crate) fn acquire(registry_path: &Path) -> io::Result<File> {
    let lock_path = suffix(registry_path, ".admission.lock");
    reject_alias(&lock_path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(&lock_path)?;
    reject_alias(&lock_path)?;
    lock.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "retention admission lock busy")
        }
        fs::TryLockError::Error(error) => error,
    })?;
    Ok(lock)
}

pub(crate) fn managed_artifact_exists(registry_path: &Path) -> io::Result<bool> {
    for extension in ["", ".lock", ".pending", ".stage"] {
        let path = suffix(registry_path, extension);
        match fs::symlink_metadata(path) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}
