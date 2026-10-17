//! Stage a web build beside its destination, preserving the last good
//! output until the build has completed all checks. Directory replacement of
//! an existing nonempty target requires two renames in portable std; this is
//! rollbackable on ordinary errors but is not an atomic swap under a crash.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "linux")]
fn linux_atomic_exchange(stage: &Path, target: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let stage = CString::new(stage.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "NUL in staging path")
    })?;
    let target = CString::new(target.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "NUL in output path")
    })?;

    // On Linux, RENAME_EXCHANGE swaps both existing directories as a single
    // namespace operation. Unsupported kernels/filesystems fail closed.
    // SAFETY: both paths are valid NUL-terminated C strings owned by us.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            stage.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

static NEXT_STAGE: AtomicU64 = AtomicU64::new(1);

/// A cooperative, per-output build lock. The create_new operation is atomic
/// across processes on ordinary local filesystems. We never steal a lock:
/// after a crash, a human must confirm the PID is no longer running and
/// remove the orphaned lockfile explicitly.
struct WebBuildLock {
    path: PathBuf,
    handle: Option<fs::File>,
}

impl WebBuildLock {
    fn acquire(parent: &Path, name: &str) -> io::Result<Self> {
        let path = parent.join(format!(".{name}.nulang-build.lock"));
        let handle = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "another Nulang web build owns {}; verify the process before removing a stale lock",
                            path.display()
                        ),
                    )
                } else {
                    e
                }
            })?;
        let mut lock = Self {
            path,
            handle: Some(handle),
        };
        if let Some(handle) = lock.handle.as_mut() {
            writeln!(handle, "pid={}", std::process::id())?;
        }
        Ok(lock)
    }
}

impl Drop for WebBuildLock {
    fn drop(&mut self) {
        // Close the handle before unlinking for Windows compatibility.
        drop(self.handle.take());
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) struct StagedWebOutput {
    target: PathBuf,
    stage: PathBuf,
    #[cfg(not(target_os = "linux"))]
    backup: PathBuf,
    published: bool,
    // Held throughout copying, building, promotion and backup cleanup.
    _lock: WebBuildLock,
}

impl StagedWebOutput {
    pub(super) fn prepare(target: &Path) -> io::Result<Self> {
        let parent = target.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "web output has no parent directory",
            )
        })?;
        let name = target
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "web output has no valid directory name",
                )
            })?;
        fs::create_dir_all(parent)?;
        let parent = fs::canonicalize(parent)?;
        let target = parent.join(name);
        let lock = WebBuildLock::acquire(&parent, name)?;
        let serial = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
        let label = format!("{}-{}-{}", name, std::process::id(), serial);
        let stage = parent.join(format!(".{label}.staging"));
        #[cfg(not(target_os = "linux"))]
        let backup = parent.join(format!(".{label}.previous"));
        fs::create_dir(&stage)?;
        let output = Self {
            target: target.clone(),
            stage,
            #[cfg(not(target_os = "linux"))]
            backup,
            published: false,
            _lock: lock,
        };
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "existing web output must be a real directory",
                ));
            }
            Ok(_) => copy_regular_tree(&target, &output.stage)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        };
        Ok(output)
    }

    pub(super) fn path(&self) -> &Path {
        &self.stage
    }

    pub(super) fn publish(mut self) -> io::Result<()> {
        let had_previous = match fs::symlink_metadata(&self.target) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => true,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "web output changed to a non-directory before publication",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };

        if had_previous {
            #[cfg(target_os = "linux")]
            {
                // The output name always resolves to either the complete old
                // directory or the complete new one, never a missing path.
                // Do not fall back to two renames if exchange is unsupported.
                linux_atomic_exchange(&self.stage, &self.target)?;
                self.published = true;
                // After exchange, the old release is at the former stage path.
                // Failure to remove it leaves a reclaimable backup, not a
                // failed or partially published deployment.
                if let Err(e) = fs::remove_dir_all(&self.stage) {
                    eprintln!(
                        "warning: atomic web publication succeeded but old release {} could not be removed: {}",
                        self.stage.display(),
                        e
                    );
                }
                return Ok(());
            }

            #[cfg(not(target_os = "linux"))]
            {
                // Other platforms retain the rollbackable portable behavior,
                // which is not crash-atomic for an existing destination.
                fs::rename(&self.target, &self.backup)?;
                if let Err(e) = fs::rename(&self.stage, &self.target) {
                    if let Err(rollback_error) = fs::rename(&self.backup, &self.target) {
                        return Err(io::Error::other(format!(
                            "cannot publish web output: {e}; cannot restore previous output: {rollback_error}"
                        )));
                    }
                    return Err(e);
                }
                self.published = true;
                if let Err(e) = fs::remove_dir_all(&self.backup) {
                    eprintln!(
                        "warning: web output published but old build backup {} could not be removed: {}",
                        self.backup.display(),
                        e
                    );
                }
                return Ok(());
            }
        }

        // An initial deployment has no previous output to exchange.
        fs::rename(&self.stage, &self.target)?;
        self.published = true;
        Ok(())
    }
}

impl Drop for StagedWebOutput {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.stage);
        }
    }
}

/// Copy the previous output without following symlinks. Preserving arbitrary
/// symlinks would let stale build content read files outside the output root.
fn copy_regular_tree(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst = destination.join(entry.file_name());
        if ty.is_dir() {
            fs::create_dir(&dst)?;
            copy_regular_tree(&entry.path(), &dst)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), dst)?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsafe entry in prior web output: {}",
                    entry.path().display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "nulang-web-stage-{name}-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn output(&self) -> PathBuf {
            self.0.join("dist")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn concurrent_build_cannot_stage_same_output() {
        let fixture = Fixture::new("concurrent");
        let first = StagedWebOutput::prepare(&fixture.output()).unwrap();
        let err = match StagedWebOutput::prepare(&fixture.output()) {
            Err(err) => err,
            Ok(_) => panic!("a second build must not stage while the first holds the lock"),
        };
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(first.path().exists());
        drop(first);

        // Failed/interrupted ordinary builds release their cooperative lock.
        let next = StagedWebOutput::prepare(&fixture.output()).unwrap();
        assert!(next.path().exists());
    }

    #[test]
    fn aliases_of_the_same_output_share_one_build_lock() {
        let fixture = Fixture::new("aliases");
        let child = fixture.0.join("subdir");
        fs::create_dir_all(&child).unwrap();
        let first = StagedWebOutput::prepare(&fixture.output()).unwrap();
        let alias = child.join("../dist");
        let err = match StagedWebOutput::prepare(&alias) {
            Ok(_) => panic!("path aliases must not bypass the build lock"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        drop(first);
        assert!(StagedWebOutput::prepare(&alias).is_ok());
    }

    #[test]
    fn orphaned_lock_is_not_silently_stolen() {
        let fixture = Fixture::new("orphan");
        let lock = fixture.0.join(".dist.nulang-build.lock");
        fs::write(&lock, b"pid=999999\n").unwrap();

        let err = match StagedWebOutput::prepare(&fixture.output()) {
            Ok(_) => panic!("orphaned lock must fail closed"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&lock).unwrap(), b"pid=999999\n");

        // An operator has verified there is no active owner.
        fs::remove_file(lock).unwrap();
        assert!(StagedWebOutput::prepare(&fixture.output()).is_ok());
    }

    #[test]
    fn successful_build_releases_lock_for_next_build() {
        let fixture = Fixture::new("unlock");
        let first = StagedWebOutput::prepare(&fixture.output()).unwrap();
        fs::write(first.path().join("index.html"), b"first").unwrap();
        first.publish().unwrap();
        let next = StagedWebOutput::prepare(&fixture.output()).unwrap();
        assert_eq!(fs::read(next.path().join("index.html")).unwrap(), b"first");
    }

    #[cfg(unix)]
    #[test]
    fn failed_stage_preparation_releases_lock_without_leaking_files() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("invalid-before-lock");
        let external = fixture.0.join("private");
        fs::create_dir_all(&external).unwrap();
        symlink(&external, fixture.output()).unwrap();

        assert!(StagedWebOutput::prepare(&fixture.output()).is_err());
        fs::remove_file(fixture.output()).unwrap();
        let stage = StagedWebOutput::prepare(&fixture.output()).unwrap();
        assert!(stage.path().exists());
    }

    #[test]
    fn failed_staged_build_does_not_mutate_previous_output() {
        let fixture = Fixture::new("failed");
        fs::create_dir_all(&fixture.output()).unwrap();
        fs::write(fixture.output().join("index.html"), b"old").unwrap();
        {
            let stage = StagedWebOutput::prepare(&fixture.output()).unwrap();
            fs::write(stage.path().join("index.html"), b"partial").unwrap();
            // Simulate build failure by dropping the stage without publication.
        }
        assert_eq!(
            fs::read(fixture.output().join("index.html")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn successful_staged_build_promotes_new_output_and_keeps_other_assets() {
        let fixture = Fixture::new("success");
        fs::create_dir_all(&fixture.output()).unwrap();
        fs::write(fixture.output().join("index.html"), b"old").unwrap();
        fs::write(fixture.output().join("unmanaged.txt"), b"retain").unwrap();
        let stage = StagedWebOutput::prepare(&fixture.output()).unwrap();
        fs::write(stage.path().join("index.html"), b"new").unwrap();
        stage.publish().unwrap();
        assert_eq!(
            fs::read(fixture.output().join("index.html")).unwrap(),
            b"new"
        );
        assert_eq!(
            fs::read(fixture.output().join("unmanaged.txt")).unwrap(),
            b"retain"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_atomic_exchange_preserves_both_nonempty_directories() {
        let fixture = Fixture::new("exchange");
        let previous = fixture.0.join("previous");
        let candidate = fixture.0.join("candidate");
        fs::create_dir(&previous).unwrap();
        fs::create_dir(&candidate).unwrap();
        fs::write(previous.join("index.html"), b"old").unwrap();
        fs::write(candidate.join("index.html"), b"new").unwrap();

        linux_atomic_exchange(&candidate, &previous).unwrap();
        assert_eq!(fs::read(previous.join("index.html")).unwrap(), b"new");
        assert_eq!(fs::read(candidate.join("index.html")).unwrap(), b"old");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_atomic_exchange_failure_does_not_remove_previous_output() {
        let fixture = Fixture::new("exchange-missing");
        let previous = fixture.0.join("previous");
        let missing = fixture.0.join("missing");
        fs::create_dir(&previous).unwrap();
        fs::write(previous.join("index.html"), b"preserved").unwrap();

        assert!(linux_atomic_exchange(&missing, &previous).is_err());
        assert_eq!(
            fs::read(previous.join("index.html")).unwrap(),
            b"preserved"
        );
    }

    #[test]
    fn publication_rename_failure_restores_previous_directory() {
        let fixture = Fixture::new("rollback");
        fs::create_dir_all(&fixture.output()).unwrap();
        fs::write(fixture.output().join("index.html"), b"previous").unwrap();

        let stage = StagedWebOutput::prepare(&fixture.output()).unwrap();
        // Simulate a failure between the previous-output rename and promotion.
        fs::remove_dir_all(stage.path()).unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(
            fs::read(fixture.output().join("index.html")).unwrap(),
            b"previous"
        );
    }

    #[cfg(unix)]
    #[test]
    fn staged_build_rejects_nested_prior_output_symlinks() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("nested-link");
        fs::create_dir_all(fixture.output().join("nested")).unwrap();
        fs::write(fixture.0.join("secret.txt"), b"outside").unwrap();
        symlink(
            fixture.0.join("secret.txt"),
            fixture.output().join("nested/secret.txt"),
        )
        .unwrap();
        assert!(StagedWebOutput::prepare(&fixture.output()).is_err());
        assert_eq!(fs::read(fixture.0.join("secret.txt")).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn staged_build_refuses_symlinked_prior_output() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("symlink");
        let private = fixture.0.join("private");
        fs::create_dir_all(&private).unwrap();
        fs::write(private.join("index.html"), b"secret").unwrap();
        symlink(&private, fixture.output()).unwrap();
        assert!(StagedWebOutput::prepare(&fixture.output()).is_err());
        assert_eq!(fs::read(private.join("index.html")).unwrap(), b"secret");
    }
}
