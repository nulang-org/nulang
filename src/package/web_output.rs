//! Staged publication of generated web artifacts. A successful build replaces
//! the output tree; ordinary failures leave the previous tree in place.
//! Two renames are not a crash-atomic filesystem transaction.

use crate::web::ir::DeploymentIr;
use std::fs;
use std::path::{Component, Path, PathBuf};

struct BuildLock(PathBuf);

impl BuildLock {
    fn acquire(path: PathBuf) -> Result<Self, String> {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|err| format!("cannot acquire web build lock {}: {err}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// A web output directory is generated, not user-owned source. Build in a
/// fresh sibling directory so failures leave the previous site untouched and
/// removed/dynamic routes cannot leave stale public HTML behind.
pub(crate) struct BuildOutput {
    staging: PathBuf,
    site: PathBuf,
    destination: PathBuf,
    preserve_previous: bool,
    _lock: BuildLock,
}

impl BuildOutput {
    pub(crate) fn new(root: &Path, output: &str, assets: &Path) -> Result<Self, String> {
        let relative = Path::new(output);
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("web output_dir must be a relative directory inside the package".into());
        }
        let canonical_root = root.canonicalize().map_err(|err| err.to_string())?;
        let destination = canonical_root.join(relative);
        let assets = if assets.exists() {
            assets.canonicalize().map_err(|err| err.to_string())?
        } else if let Ok(relative_assets) = assets.strip_prefix(root) {
            canonical_root.join(relative_assets)
        } else {
            assets.to_path_buf()
        };
        let root = canonical_root;
        for protected in [
            root.join("src"),
            root.join(".nula"),
            root.join(".git"),
            assets,
        ] {
            if destination.starts_with(&protected) || protected.starts_with(&destination) {
                return Err(format!(
                    "web output_dir overlaps protected path {}",
                    protected.display()
                ));
            }
        }
        let mut current = root;
        for component in relative.components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(format!(
                        "web output path is not a real directory: {}",
                        current.display()
                    ))
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.to_string()),
            }
        }
        let parent = destination.parent().ok_or("web output_dir has no parent")?;
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        let name = destination
            .file_name()
            .ok_or("web output_dir has no name")?
            .to_string_lossy();
        let lock = BuildLock::acquire(parent.join(format!(".{name}.nulang-build.lock")))?;
        // Refuse to replace a non-generated directory. Accept an existing
        // valid web IR (including pre-policy builds) or dev-only artifacts.
        if destination.exists() {
            let owned = fs::read_to_string(destination.join("nulang-app.ir.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<DeploymentIr>(&text).ok())
                .is_some_and(|ir| ir.version == 1);
            if !owned {
                for entry in fs::read_dir(&destination).map_err(|err| err.to_string())? {
                    let entry = entry.map_err(|err| err.to_string())?;
                    if !matches!(
                        entry.file_name().to_str(),
                        Some("app.client.js" | "app.signals.json")
                    ) || !entry.file_type().map_err(|err| err.to_string())?.is_file()
                    {
                        return Err(format!(
                            "refusing to replace non-generated output directory {}; choose a new output_dir",
                            destination.display()
                        ));
                    }
                }
            }
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|err| err.to_string())?
            .as_nanos();
        let staging = parent.join(format!(".nulang-web-{}-{stamp}", std::process::id()));
        fs::create_dir(&staging).map_err(|err| err.to_string())?;
        let site = staging.join("site");
        let output = Self {
            staging,
            site,
            destination,
            preserve_previous: false,
            _lock: lock,
        };
        fs::create_dir(&output.site).map_err(|err| err.to_string())?;
        Ok(output)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.site
    }

    pub(crate) fn publish(self) -> Result<(), String> {
        self.publish_with(|from, to| fs::rename(from, to))
    }

    fn publish_with(
        mut self,
        install: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<(), String> {
        let previous = self.staging.join("previous");
        let had_previous = self.destination.exists();
        if had_previous {
            fs::rename(&self.destination, &previous)
                .map_err(|err| format!("cannot preserve previous web output: {err}"))?;
        }
        if let Err(err) = install(&self.site, &self.destination) {
            if had_previous {
                if let Err(restore) = fs::rename(&previous, &self.destination) {
                    self.preserve_previous = true;
                    return Err(format!("cannot publish web output: {err}; cannot restore previous output: {restore}; previous output preserved at {}", previous.display()));
                }
            }
            return Err(format!("cannot publish web output: {err}"));
        }
        Ok(())
    }
}

impl Drop for BuildOutput {
    fn drop(&mut self) {
        if !self.preserve_previous {
            if let Err(err) = fs::remove_dir_all(&self.staging) {
                eprintln!(
                    "warning: retained web build staging directory {}: {err}",
                    self.staging.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "nulang-web-output-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn stage(&self) -> Result<BuildOutput, String> {
            BuildOutput::new(&self.0, "dist", &self.0.join("public"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn concurrent_builds_cannot_share_the_destination() {
        let fixture = Fixture::new();
        let first = fixture.stage().unwrap();
        assert!(fixture.stage().err().unwrap().contains("build lock"));
        drop(first);
        assert!(fixture.stage().is_ok());
    }

    #[test]
    fn failed_publication_restores_previous_generated_tree() {
        let fixture = Fixture::new();
        let first = fixture.stage().unwrap();
        fs::write(
            first.path().join("nulang-app.ir.json"),
            DeploymentIr {
                version: 1,
                ..DeploymentIr::default()
            }
            .to_json(),
        )
        .unwrap();
        fs::write(first.path().join("index.html"), "previous").unwrap();
        first.publish().unwrap();

        let replacement = fixture.stage().unwrap();
        fs::write(replacement.path().join("index.html"), "new").unwrap();
        assert!(replacement
            .publish_with(|_, _| Err(std::io::Error::other("injected install failure")))
            .is_err());
        assert_eq!(
            fs::read_to_string(fixture.0.join("dist/index.html")).unwrap(),
            "previous"
        );
        assert!(fixture.stage().is_ok());
    }

    #[test]
    fn protected_source_and_repository_paths_cannot_be_replaced() {
        let fixture = Fixture::new();
        for output in [
            "",
            ".",
            "..",
            "../outside",
            "/tmp/out",
            "src",
            "src/generated",
            ".git",
            ".nula",
            "public",
            "public/nested",
        ] {
            assert!(
                BuildOutput::new(&fixture.0, output, &fixture.0.join("public")).is_err(),
                "{output}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_output_is_rejected() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("outside")).unwrap();
        std::os::unix::fs::symlink(fixture.0.join("outside"), fixture.0.join("dist")).unwrap();
        assert!(fixture.stage().is_err());
        assert!(BuildOutput::new(&fixture.0, "dist/nested", &fixture.0.join("public")).is_err());
    }
}
