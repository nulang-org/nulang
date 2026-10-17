//! Stage a web build beside its destination, preserving the last good
//! output until the build has completed all checks. Directory replacement of
//! an existing nonempty target requires two renames in portable std; this is
//! rollbackable on ordinary errors but is not an atomic swap under a crash.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_STAGE: AtomicU64 = AtomicU64::new(1);

pub(super) struct StagedWebOutput {
    target: PathBuf,
    stage: PathBuf,
    backup: PathBuf,
    published: bool,
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
        let serial = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
        let label = format!("{}-{}-{}", name, std::process::id(), serial);
        let stage = parent.join(format!(".{label}.staging"));
        let backup = parent.join(format!(".{label}.previous"));
        fs::create_dir(&stage)?;
        let output = Self {
            target: target.to_path_buf(),
            stage,
            backup,
            published: false,
        };
        match fs::symlink_metadata(target) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "existing web output must be a real directory",
                ));
            }
            Ok(_) => copy_regular_tree(target, &output.stage)?,
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
            fs::rename(&self.target, &self.backup)?;
        }
        if let Err(e) = fs::rename(&self.stage, &self.target) {
            if had_previous {
                if let Err(rollback_error) = fs::rename(&self.backup, &self.target) {
                    return Err(io::Error::other(format!(
                        "cannot publish web output: {e}; cannot restore previous output: {rollback_error}"
                    )));
                }
            }
            return Err(e);
        }
        self.published = true;
        if had_previous {
            if let Err(e) = fs::remove_dir_all(&self.backup) {
                eprintln!(
                    "warning: web output published but old build backup {} could not be removed: {}",
                    self.backup.display(),
                    e
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
