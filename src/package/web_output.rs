//! Isolated, fail-closed publication of static Web build artifacts.
//!
//! A failed compile, render, asset copy, or budget check must not mutate the
//! last successful `dist` directory. Promotion uses same-parent renames and
//! restores the old directory if installation returns an error.
//!
//! This is not a crash-atomic filesystem transaction: a process kill between
//! the two renames can leave an explicitly named backup and a stale lock.
//! Keep those artifacts for operator recovery rather than guessing on restart.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static BUILD_ID: AtomicU64 = AtomicU64::new(0);

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

fn normal_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Deny output paths that could replace sources or escape the package root.
fn validate_target(root: &Path, output: &str, static_dir: &str) -> io::Result<PathBuf> {
    let relative = Path::new(output);
    if !normal_relative(relative) {
        return Err(invalid("web output_dir must be a nonempty relative path without . or .."));
    }

    for protected in ["src", ".git", ".nula"] {
        if overlaps(relative, Path::new(protected)) {
            return Err(invalid("web output_dir overlaps protected project inputs"));
        }
    }
    let static_source = Path::new(static_dir);
    if !normal_relative(static_source) || overlaps(relative, static_source) {
        return Err(invalid("web output_dir overlaps static_dir or static_dir is unsafe"));
    }

    let canonical_root = root.canonicalize()?;
    let mut target = canonical_root.clone();
    for part in relative.components() {
        target.push(part.as_os_str());
        match fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid("web output_dir traverses a symlink"));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(invalid("web output_dir collides with a non-directory"));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(target)
}

struct BuildLock {
    path: PathBuf,
}

impl BuildLock {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        // A stale lock is intentional: never silently overwrite output after
        // a killed build without inspecting its staging and backup directories.
        let _file = OpenOptions::new().write(true).create_new(true).open(&path)?;
        Ok(Self { path })
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) struct StagedWebOutput {
    target: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
    _lock: BuildLock,
    published: bool,
}

impl StagedWebOutput {
    pub(super) fn new(root: &Path, output: &str, static_dir: &str) -> io::Result<Self> {
        let target = validate_target(root, output, static_dir)?;
        let parent = target
            .parent()
            .ok_or_else(|| invalid("web output_dir has no parent"))?;
        fs::create_dir_all(parent)?;
        let name = target
            .file_name()
            .ok_or_else(|| invalid("web output_dir has no filename"))?
            .to_string_lossy();
        let lock = BuildLock::acquire(parent.join(format!(".{name}.nulang-build.lock")))?;

        for _ in 0..32 {
            let id = BUILD_ID.fetch_add(1, Ordering::Relaxed);
            let suffix = format!("{}-{id}", std::process::id());
            let stage = parent.join(format!(".{name}.nulang-stage-{suffix}"));
            let backup = parent.join(format!(".{name}.nulang-previous-{suffix}"));
            if backup.exists() {
                continue;
            }
            match fs::create_dir(&stage) {
                Ok(()) => {
                    return Ok(Self {
                        target,
                        stage,
                        backup,
                        _lock: lock,
                        published: false,
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "unable to reserve unique web build staging directory",
        ))
    }

    pub(super) fn path(&self) -> &Path {
        &self.stage
    }

    pub(super) fn publish(self) -> io::Result<()> {
        self.publish_with(fs::rename)
    }

    /// The injected install operation makes rollback testable without timing
    /// races or reliance on OS-specific permission behavior.
    fn publish_with<F>(mut self, install: F) -> io::Result<()>
    where
        F: FnOnce(&Path, &Path) -> io::Result<()>,
    {
        match fs::symlink_metadata(&self.target) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(invalid("published web output is not a real directory"));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }

        let had_previous = self.target.exists();
        if had_previous {
            fs::rename(&self.target, &self.backup)?;
        }

        if let Err(install_error) = install(&self.stage, &self.target) {
            if had_previous {
                if let Err(rollback_error) = fs::rename(&self.backup, &self.target) {
                    return Err(io::Error::other(format!(
                        "web publish failed: {install_error}; rollback failed: {rollback_error}; previous build is at {}",
                        self.backup.display()
                    )));
                }
            }
            return Err(install_error);
        }

        self.published = true;
        if had_previous {
            // Installation has succeeded. Failed backup cleanup is nonfatal;
            // retain it for manual recovery rather than misreport the build.
            if let Err(err) = fs::remove_dir_all(&self.backup) {
                eprintln!(
                    "warning: old web build retained at {}: {}",
                    self.backup.display(),
                    err
                );
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "nulang_web_output_{}_{}_{}",
            label,
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn successful_publish_replaces_stale_output_instead_of_merging() {
        let root = fixture("replace");
        std::fs::create_dir(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/obsolete.html"), "old").unwrap();
        let pending = StagedWebOutput::new(&root, "dist", "public").unwrap();
        std::fs::write(pending.path().join("index.html"), "new").unwrap();

        pending.publish().unwrap();

        assert_eq!(std::fs::read_to_string(root.join("dist/index.html")).unwrap(), "new");
        assert!(!root.join("dist/obsolete.html").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dropped_or_failed_build_preserves_previous_output() {
        let root = fixture("discard");
        std::fs::create_dir(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/index.html"), "old").unwrap();
        {
            let pending = StagedWebOutput::new(&root, "dist", "public").unwrap();
            std::fs::write(pending.path().join("index.html"), "new").unwrap();
            assert_eq!(std::fs::read_to_string(root.join("dist/index.html")).unwrap(), "old");
        }
        assert_eq!(std::fs::read_to_string(root.join("dist/index.html")).unwrap(), "old");
        let pending = StagedWebOutput::new(&root, "dist", "public").unwrap();
        std::fs::write(pending.path().join("index.html"), "later").unwrap();
        pending.publish().unwrap();
        assert_eq!(std::fs::read_to_string(root.join("dist/index.html")).unwrap(), "later");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_install_renames_previous_output_back() {
        let root = fixture("rollback");
        std::fs::create_dir(root.join("dist")).unwrap();
        std::fs::write(root.join("dist/index.html"), "old").unwrap();
        let pending = StagedWebOutput::new(&root, "dist", "public").unwrap();
        std::fs::write(pending.path().join("index.html"), "new").unwrap();

        let result = pending.publish_with(|_, _| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected install failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(root.join("dist/index.html")).unwrap(), "old");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_builds_are_rejected_without_disturbing_existing_output() {
        let root = fixture("lock");
        let pending = StagedWebOutput::new(&root, "dist", "public").unwrap();
        assert!(StagedWebOutput::new(&root, "dist", "public").is_err());
        drop(pending);
        assert!(StagedWebOutput::new(&root, "dist", "public").is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reject_output_paths_that_can_replace_project_inputs() {
        let root = fixture("unsafe");
        for output in ["", ".", "..", "../escape", "/tmp/out", "src", "src/generated", ".git", ".nula", "public", "public/build"] {
            assert!(
                StagedWebOutput::new(&root, output, "public").is_err(),
                "output {output:?} must not be writable"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn reject_symlinked_output_or_parent() {
        use std::os::unix::fs::symlink;
        let root = fixture("symlink");
        std::fs::create_dir(root.join("outside")).unwrap();
        symlink(root.join("outside"), root.join("dist")).unwrap();
        assert!(StagedWebOutput::new(&root, "dist", "public").is_err());
        symlink(root.join("outside"), root.join("alias")).unwrap();
        assert!(StagedWebOutput::new(&root, "alias/dist", "public").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
