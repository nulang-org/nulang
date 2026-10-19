//! Read-only, pre-compilation package inspection.
//!
//! This is deliberately NOT a compiler-produced artifact identity or a Cloud
//! admission document. It reports exactly what can be established from the
//! local package and its existing lockfile without resolving, building, or
//! changing any files.

use std::collections::BTreeSet;
use std::path::{Path, Component};

use crate::package::identity::source_id_for_package_dir;
use crate::package::lockfile::{Lockfile, LOCKFILE_FILE};
use crate::package::manifest::Manifest;
use crate::types::{NuError, NuResult, Span};

pub const BUILD_PLAN_SCHEMA: &str = "nulang.build-plan/v0alpha1";

fn package_error(message: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: message.into(),
        span: Span::default(),
    }
}

/// Inspect an existing lockfile and canonical source inputs without invoking
/// the dependency resolver or compiler. Source identities are verified only
/// for locally available path dependencies; git commit pins are *not* proofs
/// of fetched content and must not be treated as such by downstream tools.
pub fn inspect(root: &Path) -> NuResult<serde_json::Value> {
    let manifest = Manifest::load(root)?;
    let entry = Path::new(&manifest.package.entry);
    if entry.components().any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        || entry.extension().and_then(|ext| ext.to_str()) != Some("nula")
        || !root.join(entry).is_file()
    {
        return Err(package_error(format!(
            "package entry '{}' must be an existing relative .nula file",
            manifest.package.entry
        )));
    }

    let root_id = source_id_for_package_dir(root)
        .map_err(|error| package_error(format!("cannot identify root package sources: {error}")))?;
    let lockfile = if root.join(LOCKFILE_FILE).exists() {
        Lockfile::load(root)?
    } else if manifest.dependencies.is_empty() {
        Lockfile::new()
    } else {
        return Err(package_error(
            "Nulang.lock is required to inspect a package with dependencies; run nula build first",
        ));
    };

    // A stale lockfile must not silently masquerade as covering direct inputs.
    for name in manifest.dependencies.keys() {
        if !lockfile.package.iter().any(|dependency| &dependency.name == name) {
            return Err(package_error(format!(
                "Nulang.lock does not contain direct dependency '{name}'; rebuild the lockfile"
            )));
        }
    }

    let mut packages = lockfile.package.iter().collect::<Vec<_>>();
    packages.sort_by(|a, b| (&a.name, &a.version, &a.source).cmp(&(&b.name, &b.version, &b.source)));
    let mut seen = BTreeSet::new();
    let mut dependencies = Vec::with_capacity(packages.len());

    for package in packages {
        if !seen.insert((&package.name, &package.version, &package.source)) {
            return Err(package_error(format!(
                "duplicate locked dependency '{} {}' from {}",
                package.name, package.version, package.source
            )));
        }
        let metadata = lockfile.package_identity(
            &package.name,
            &package.version,
            &package.source,
        );
        let (source_id, verification) = if let Some(path) = package.source.strip_prefix("path+") {
            let expected = metadata
                .ok_or_else(|| package_error(format!(
                    "missing SourceId for locked path dependency '{}'",
                    package.name
                )))?
                .parsed_source_id()?
                .ok_or_else(|| package_error(format!(
                    "missing SourceId for locked path dependency '{}'",
                    package.name
                )))?;
            let actual = source_id_for_package_dir(Path::new(path))
                .map_err(|error| package_error(format!(
                    "cannot verify locked path dependency '{}': {error}",
                    package.name
                )))?;
            if expected != actual {
                return Err(package_error(format!(
                    "source identity mismatch for locked dependency '{}': expected {expected}, got {actual}",
                    package.name
                )));
            }
            (Some(actual.to_string()), "local-source-verified")
        } else if package.source.starts_with("git+") {
            let pinned = package.commit.len() == 40 || package.commit.len() == 64;
            if !pinned || !package.commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(package_error(format!(
                    "locked git dependency '{}' has no complete commit pin",
                    package.name
                )));
            }
            (None, "commit-pinned-content-unverified")
        } else {
            return Err(package_error(format!(
                "unsupported lockfile source '{}' for dependency '{}'",
                package.source, package.name
            )));
        };

        let semantic_id = metadata
            .map(|record| record.parsed_semantic_id())
            .transpose()?
            .flatten()
            .map(|id| id.to_string());
        dependencies.push(serde_json::json!({
            "name": package.name,
            "version": package.version,
            "source": package.source,
            "source_id": source_id,
            "semantic_id": semantic_id,
            "verification": verification,
        }));
    }

    Ok(serde_json::json!({
        "schema": BUILD_PLAN_SCHEMA,
        "status": "prebuild-inspection-only",
        "package": {
            "name": manifest.package.name,
            "version": manifest.package.version,
            "entry": manifest.package.entry,
            "source_id": root_id.to_string(),
        },
        "dependencies": dependencies,
        "artifact": {
            "semantic_id": null,
            "artifact_id": null,
            "digest": null,
        },
        "cloud_admissible": false,
        "limitations": [
            "does-not-compile-or-typecheck",
            "does-not-verify-git-content",
            "does-not-prove-transitive-resolution-or-all-external-inputs",
            "not-an-artifact-or-behavior-manifest",
        ],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::lockfile::LockedPackage;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn scratch(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_build_plan_{label}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn make_package(root: &Path, dependencies: &str) {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Nulang.toml"),
            format!(
                "[package]\nname = \"test-pkg\"\nversion = \"0.1.0\"\n\n{dependencies}"
            ),
        )
        .unwrap();
        std::fs::write(root.join("src/main.nula"), "fn main() { 42 }").unwrap();
    }

    fn locked_dependency(name: &str, source: String, commit: &str) -> LockedPackage {
        LockedPackage {
            name: name.into(),
            version: "0.1.0".into(),
            source,
            content_hash: String::new(),
            commit: commit.into(),
        }
    }

    #[test]
    fn dependency_free_inspection_is_stable_and_read_only() {
        let root = scratch("empty");
        make_package(&root, "");
        let first = inspect(&root).unwrap();
        let second = inspect(&root).unwrap();
        assert_eq!(first, second);
        assert_eq!(first["schema"], BUILD_PLAN_SCHEMA);
        assert_eq!(first["status"], "prebuild-inspection-only");
        assert_eq!(first["cloud_admissible"], false);
        assert_eq!(first["artifact"]["digest"], serde_json::Value::Null);
        assert!(first["package"]["source_id"].as_str().unwrap().len() == 64);
        assert!(!root.join(LOCKFILE_FILE).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn local_source_drift_fails_closed_instead_of_repinning() {
        let root = scratch("drift");
        let dependency = root.join("util");
        make_package(&root, "[dependencies]\nutil = { path = \"util\" }\n");
        make_package(&dependency, "");
        let mut lock = Lockfile::new();
        lock.package.push(locked_dependency(
            "util",
            format!("path+{}", dependency.display()),
            "",
        ));
        lock.save(&root).unwrap();

        let first = inspect(&root).unwrap();
        assert_eq!(first["dependencies"][0]["verification"], "local-source-verified");
        std::fs::write(dependency.join("src/main.nula"), "fn main() { 43 }").unwrap();
        assert!(inspect(&root).unwrap_err().to_string().contains("source identity mismatch"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_locked_path_dependency_fails_closed() {
        let root = scratch("missing-path");
        let dependency = root.join("util");
        make_package(&root, "[dependencies]\nutil = { path = \"util\" }\n");
        make_package(&dependency, "");
        let mut lock = Lockfile::new();
        lock.package.push(locked_dependency("util", format!("path+{}", dependency.display()), ""));
        lock.save(&root).unwrap();
        std::fs::remove_dir_all(&dependency).unwrap();

        assert!(inspect(&root).unwrap_err().to_string().contains("cannot verify locked path"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dependency_requires_existing_lockfile() {
        let root = scratch("unlocked");
        make_package(&root, "[dependencies]\nutil = { path = \"util\" }\n");
        assert!(inspect(&root).unwrap_err().to_string().contains("Nulang.lock is required"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_pin_is_not_misrepresented_as_verified_content() {
        let root = scratch("git");
        make_package(&root, "[dependencies]\nutil = { git = \"https://example.org/util.git\" }\n");
        let mut lock = Lockfile::new();
        lock.package.push(locked_dependency(
            "util",
            "git+https://example.org/util.git".into(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ));
        lock.save(&root).unwrap();
        let plan = inspect(&root).unwrap();
        assert_eq!(plan["dependencies"][0]["verification"], "commit-pinned-content-unverified");
        assert_eq!(plan["dependencies"][0]["source_id"], serde_json::Value::Null);
        assert_eq!(plan["cloud_admissible"], false);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unpinned_git_dependency_fails_closed() {
        let root = scratch("unpinned");
        make_package(&root, "[dependencies]\nutil = { git = \"https://example.org/util.git\" }\n");
        let mut lock = Lockfile::new();
        lock.package.push(locked_dependency("util", "git+https://example.org/util.git".into(), ""));
        lock.save(&root).unwrap();
        assert!(inspect(&root).unwrap_err().to_string().contains("complete commit pin"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn root_inputs_change_source_id_without_producing_an_artifact() {
        let root = scratch("root-change");
        make_package(&root, "");
        let before = inspect(&root).unwrap();
        std::fs::write(root.join("src/main.nula"), "fn main() { 43 }").unwrap();
        let after = inspect(&root).unwrap();
        assert_ne!(before["package"]["source_id"], after["package"]["source_id"]);
        assert_eq!(after["artifact"]["artifact_id"], serde_json::Value::Null);
        let _ = std::fs::remove_dir_all(root);
    }
}
