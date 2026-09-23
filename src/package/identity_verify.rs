//! Verification bridge between canonical package identities and `Nulang.lock`.
//!
//! Lockfile parsing intentionally remains side-effect free: loading a lockfile
//! must not unexpectedly fail merely because a local path dependency is absent
//! on the current machine. Build/resolve policy can opt into this explicit
//! verification step when the source is available.

use std::path::Path;

use crate::package::identity::source_id_for_package_dir;
use crate::package::lockfile::Lockfile;
use crate::types::{NuError, NuResult, Span};

impl Lockfile {
    /// Verify every stored path-dependency [`crate::content_identity::SourceId`]
    /// whose source directory is currently available.
    ///
    /// Missing local sources are skipped, matching save-time enrichment. A
    /// present source that cannot be canonicalized or whose identity differs
    /// from the lockfile fails closed. Git/registry pins remain governed by
    /// their existing commit/source mechanisms until strong source identity is
    /// defined for fetched package contents.
    pub fn verify_available_source_identities(&self) -> NuResult<()> {
        for identity in &self.identity {
            let Some(expected) = identity.parsed_source_id()? else {
                continue;
            };
            let Some(path) = identity.source.strip_prefix("path+") else {
                continue;
            };
            let path = Path::new(path);
            if !path.exists() {
                continue;
            }

            let actual =
                source_id_for_package_dir(path).map_err(|error| NuError::PackageError {
                    msg: format!(
                        "cannot verify source identity for locked package '{}' {} from {}: {}",
                        identity.name, identity.version, identity.source, error
                    ),
                    span: Span::default(),
                })?;
            if actual != expected {
                return Err(NuError::PackageError {
                    msg: format!(
                        "source identity mismatch for locked package '{}' {} from {}: expected {}, got {}",
                        identity.name, identity.version, identity.source, expected, actual
                    ),
                    span: Span::default(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::lockfile::{LockedPackage, Lockfile};

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_lockfile_identity_verify_{name}_{}",
            std::process::id()
        ))
    }

    fn package(root: &Path) {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Nulang.toml"),
            "[package]\nname = \"util\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/main.nula"), "fn main() { 1 }").unwrap();
    }

    fn lockfile_for(path: &Path) -> Lockfile {
        let mut lockfile = Lockfile::new();
        lockfile.package.push(LockedPackage {
            name: "util".into(),
            version: "0.1.0".into(),
            source: format!("path+{}", path.display()),
            content_hash: "legacy-hash".into(),
            commit: String::new(),
        });
        lockfile
    }

    #[test]
    fn available_source_identity_verifies_then_detects_drift() {
        let root = scratch("drift");
        let dependency = root.join("util");
        let _ = std::fs::remove_dir_all(&root);
        package(&dependency);

        let lockfile = lockfile_for(&dependency)
            .with_available_source_identities()
            .unwrap();
        lockfile.verify_available_source_identities().unwrap();

        std::fs::write(dependency.join("src/main.nula"), "fn main() { 2 }").unwrap();
        let error = lockfile
            .verify_available_source_identities()
            .expect_err("changed source must not match its locked SourceId");
        match error {
            NuError::PackageError { msg, .. } => {
                assert!(msg.contains("source identity mismatch"));
                assert!(msg.contains("util"));
            }
            other => panic!("expected PackageError, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unavailable_path_source_is_not_an_implicit_load_failure() {
        let missing = scratch("missing").join("does-not-exist");
        let mut lockfile = lockfile_for(&missing);
        let source_id = crate::content_identity::SourceId::from_bytes(b"unavailable");
        let source = lockfile.package[0].source.clone();
        lockfile
            .set_package_identity("util", "0.1.0", &source, Some(source_id), None)
            .unwrap();

        lockfile.verify_available_source_identities().unwrap();
    }
}
