//! Dependency resolution for `nula` packages.
//!
//! The resolver walks the dependency graph starting from a root manifest.
//! Local-path dependencies are read straight from disk; git dependencies are
//! cloned into `<root>/.nula/git/<name>` (a checkout is reused when its
//! manifest is already present). There is no network registry — a bare
//! version requirement like `foo = "0.1.0"` is a resolution error.
//!
//! Version requirements on dependencies are checked with a simple
//! semver-compatible rule: the resolved version must be at least as new as
//! the requirement and share its major version (for 0.x versions, which make
//! no stability promise, the minor version must match too). See
//! [`version_satisfies`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::package::lockfile::{LockedPackage, Lockfile};
use crate::package::manifest::{Dependency, DependencyDetail, Manifest, MANIFEST_FILE};
use crate::types::{NuError, NuResult, Span};

/// Where a resolved package came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    /// The package `nula` was invoked in; not written to the lockfile.
    Root,
    /// A local directory (canonicalized).
    Path(PathBuf),
    /// Downloaded from a registry at the given URL.
    Registry(String),
    /// A git clone cached under `<root>/.nula/git/`, with the requested
    Git {
        url: String,
        rev: Option<String>,
        /// Full SHA of the checkout's HEAD at fetch time — the dependency's
        /// cache key. A moved remote ref invalidates the cached checkout.
        commit: Option<String>,
    },
}

impl PackageSource {
    /// Lockfile string form: `path+<dir>`, `git+<url>#<rev>`, or `reg+<url>`.
    fn lockfile_source(&self) -> String {
        match self {
            PackageSource::Root => "root".to_string(),
            PackageSource::Path(dir) => format!("path+{}", dir.display()),
            PackageSource::Registry(url) => format!("reg+{}", url),
            PackageSource::Git { url, rev, .. } => match rev {
                Some(rev) => format!("git+{}#{}", url, rev),
                None => format!("git+{}", url),
            },
        }
    }

    /// The directory this package's source lives in, if it has a local
    /// path (for content hashing).
    fn dir(&self) -> Option<&Path> {
        match self {
            PackageSource::Path(dir) => Some(dir),
            // Git checkouts are keyed by their commit SHA (recorded in
            // `commit` at fetch time, see `fetch_git`), not by content hash.
            PackageSource::Git { .. } => None,
            PackageSource::Root => None,
            PackageSource::Registry(_) => None,
        }
    }

    /// Parse a lockfile source string back into a `PackageSource`.
    pub fn parse_source(s: &str) -> Option<PackageSource> {
        if s == "root" {
            return Some(PackageSource::Root);
        }
        if let Some(dir) = s.strip_prefix("path+") {
            return Some(PackageSource::Path(PathBuf::from(dir)));
        }
        if let Some(rest) = s.strip_prefix("reg+") {
            return Some(PackageSource::Registry(rest.to_string()));
        }
        if let Some(rest) = s.strip_prefix("git+") {
            if let Some((url, rev)) = rest.split_once('#') {
                return Some(PackageSource::Git {
                    url: url.to_string(),
                    rev: Some(rev.to_string()),
                    commit: None,
                });
            }
            return Some(PackageSource::Git {
                url: rest.to_string(),
                rev: None,
                commit: None,
            });
        }
        None
    }
}

/// A package in the resolution graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
    pub source: PackageSource,
    /// Names of this package's resolved direct dependencies.
    pub dependencies: Vec<String>,
}

impl ResolvedPackage {
    /// Compute the BLAKE3 hash of this package's source files (all `.nula`
    /// files in the package directory, sorted by path, concatenated). Returns
    /// hex. Empty string if the source directory is unavailable.
    pub fn content_hash_hex(&self) -> String {
        let Some(dir) = self.source.dir() else {
            return String::new();
        };
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("nula"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        let mut hasher = blake3::Hasher::new();
        for file in &files {
            if let Ok(contents) = std::fs::read(file) {
                hasher.update(&contents);
            }
        }
        hasher.finalize().to_hex().to_string()
    }
}

/// The result of resolving a root package: every package in topological
/// order (dependencies before dependents, root last).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub packages: Vec<ResolvedPackage>,
}

impl Resolution {
    /// The root package (always last in topological order).
    pub fn root(&self) -> &ResolvedPackage {
        &self.packages[self.packages.len() - 1]
    }

    /// Packages in build order, excluding the root.
    pub fn dependencies(&self) -> &[ResolvedPackage] {
        &self.packages[..self.packages.len() - 1]
    }

    /// Pin every non-root package into a lockfile, including the BLAKE3
    /// content hash of each resolved package's source and the resolved
    /// git commit of each git dependency.
    pub fn to_lockfile(&self) -> Lockfile {
        let mut lockfile = Lockfile::new();
        for package in self.dependencies() {
            lockfile.package.push(LockedPackage {
                name: package.name.clone(),
                version: package.version.clone(),
                source: package.source.lockfile_source(),
                content_hash: package.content_hash_hex(),
                commit: match &package.source {
                    PackageSource::Git { commit, .. } => commit.clone().unwrap_or_default(),
                    _ => String::new(),
                },
            });
        }
        lockfile
    }
}

/// Resolve the full dependency graph of `manifest`, rooted at `root_dir`.
pub fn resolve(root_dir: &Path, manifest: &Manifest) -> NuResult<Resolution> {
    let root_dir = std::fs::canonicalize(root_dir).map_err(|e| NuError::PackageError {
        msg: format!("cannot resolve {}: {}", root_dir.display(), e),
        span: Span::default(),
    })?;
    let registry = manifest.package.registry.clone();
    let mut resolver = Resolver::new(root_dir.clone(), registry);
    resolver.resolve_package(&root_dir, manifest, PackageSource::Root)?;
    Ok(Resolution {
        packages: resolver.order,
    })
}

/// Parse `x.y.z` into numeric components (missing components default to 0).
pub fn parse_semver(version: &str) -> NuResult<(u64, u64, u64)> {
    let mut parts = version.split('.');
    let mut next = |what: &str| -> NuResult<u64> {
        match parts.next() {
            Some(part) => part.parse::<u64>().map_err(|_| NuError::PackageError {
                msg: format!("invalid semver '{}' in version '{}'", part, version),
                span: Span::default(),
            }),
            None if what == "major" => Err(NuError::PackageError {
                msg: format!("invalid semver version '{}'", version),
                span: Span::default(),
            }),
            None => Ok(0),
        }
    };
    let major = next("major")?;
    let minor = next("minor")?;
    let patch = next("patch")?;
    if parts.next().is_some() {
        return Err(NuError::PackageError {
            msg: format!("invalid semver version '{}'", version),
            span: Span::default(),
        });
    }
    Ok((major, minor, patch))
}

/// Compare two semver versions numerically, major then minor then patch.
pub fn compare_semver(a: &str, b: &str) -> NuResult<std::cmp::Ordering> {
    Ok(parse_semver(a)?.cmp(&parse_semver(b)?))
}

/// Simple semver-compatible check: `version` satisfies `requirement` when
/// it is at least as new and shares the requirement's major version. For
/// 0.x requirements (no stability promise) the minor must match exactly,
/// mirroring Cargo's caret-requirement semantics.
pub fn version_satisfies(requirement: &str, version: &str) -> NuResult<bool> {
    let req = parse_semver(requirement)?;
    let ver = parse_semver(version)?;
    if ver < req {
        return Ok(false);
    }
    if req.0 == 0 {
        Ok(ver.0 == 0 && ver.1 == req.1)
    } else {
        Ok(ver.0 == req.0)
    }
}

struct Resolver {
    root_dir: PathBuf,
    /// Default registry URL (from root manifest's `[package] registry`).
    default_registry: Option<String>,
    /// Canonical package dir -> name, to deduplicate shared dependencies.
    by_dir: BTreeMap<PathBuf, String>,
    /// Name -> source, to detect conflicting sources for one package.
    by_name: BTreeMap<String, PackageSource>,
    /// Dirs currently on the resolution stack, for cycle detection.
    in_progress: Vec<PathBuf>,
    /// Packages in topological order (dependencies before dependents).
    order: Vec<ResolvedPackage>,
}

impl Resolver {
    fn new(root_dir: PathBuf, default_registry: Option<String>) -> Self {
        Resolver {
            root_dir,
            default_registry,
            by_dir: BTreeMap::new(),
            by_name: BTreeMap::new(),
            in_progress: Vec::new(),
            order: Vec::new(),
        }
    }

    fn resolve_package(
        &mut self,
        dir: &Path,
        manifest: &Manifest,
        source: PackageSource,
    ) -> NuResult<()> {
        let name = manifest.package.name.clone();
        self.by_dir.insert(dir.to_path_buf(), name.clone());
        self.by_name.insert(name.clone(), source.clone());
        self.in_progress.push(dir.to_path_buf());

        let mut dependencies = Vec::new();
        for (dep_name, dep) in &manifest.dependencies {
            self.resolve_dependency(dir, dep_name, dep)?;
            dependencies.push(dep_name.clone());
        }

        self.in_progress.pop();
        self.order.push(ResolvedPackage {
            name,
            version: manifest.package.version.clone(),
            source,
            dependencies,
        });
        Ok(())
    }

    fn resolve_dependency(
        &mut self,
        parent_dir: &Path,
        dep_name: &str,
        dep: &Dependency,
    ) -> NuResult<()> {
        let (dep_dir, source) = match dep {
            Dependency::Version(req) => {
                let registry_url = self.resolve_dependency_registry(parent_dir)?;
                let (dir, _ver) = self.fetch_registry(dep_name, req, &registry_url)?;
                (dir, PackageSource::Registry(registry_url))
            }
            Dependency::Detailed(detail) => {
                let (dir, commit) = self.resolve_detailed(parent_dir, dep_name, detail)?;
                let source = if detail.path.is_some() {
                    PackageSource::Path(dir.clone())
                } else {
                    PackageSource::Git {
                        url: detail.git.clone().unwrap_or_default(),
                        rev: detail
                            .rev
                            .clone()
                            .or_else(|| detail.branch.clone())
                            .or_else(|| detail.tag.clone()),
                        commit,
                    }
                };
                (dir, source)
            }
        };

        // Shared dependency already resolved: nothing more to do. A dir
        // still on the resolution stack means the dependency graph cycles.
        if self.by_dir.contains_key(&dep_dir) {
            if self.in_progress.contains(&dep_dir) {
                return Err(NuError::PackageError {
                    msg: format!("dependency cycle detected at '{}'", dep_name),
                    span: Span::default(),
                });
            }
            return Ok(());
        }
        if let Some(existing) = self.by_name.get(dep_name) {
            return Err(NuError::PackageError {
                msg: format!(
                    "conflicting sources for dependency '{}': {} and {}",
                    dep_name,
                    existing.lockfile_source(),
                    source.lockfile_source()
                ),
                span: Span::default(),
            });
        }

        let manifest = Manifest::load(&dep_dir)?;
        if manifest.package.name != dep_name {
            return Err(NuError::PackageError {
                msg: format!(
                    "dependency '{}' resolves to package '{}' at {}",
                    dep_name,
                    manifest.package.name,
                    dep_dir.join(MANIFEST_FILE).display()
                ),
                span: Span::default(),
            });
        }

        // Version check for Detailed deps; registry resolution already picked
        // a satisfying version.
        if let Dependency::Detailed(d) = dep {
            if let Some(requirement) = &d.version {
                if !version_satisfies(requirement, &manifest.package.version)? {
                    return Err(NuError::PackageError {
                        msg: format!(
                            "dependency '{}' requires version {} but {} provides {}",
                            dep_name, requirement, dep_name, manifest.package.version
                        ),
                        span: Span::default(),
                    });
                }
            }
        }

        // Recurse before pushing so dependencies land before dependents in
        // `order`; revisiting a dir still on the stack is a cycle (caught
        // above), revisiting a finished one is a shared dependency.
        self.resolve_package(&dep_dir, &manifest, source)
    }

    /// Clone a git dependency into `<root>/.nula/git/<name>` and return its
    /// directory plus the resolved commit SHA (the dependency's cache key).
    ///
    /// The commit is recorded next to the checkout in `<cache>/<name>.commit`.
    /// A cached checkout is reused only while its recorded commit still
    /// matches what the dependency spec resolves to now: branch/tag/unpinned
    /// specs are re-fetched from the remote so a moved ref invalidates the
    /// cache, while a pinned rev is compared locally. A stale checkout is
    /// wiped and re-cloned. Requires `git` on PATH.
    fn fetch_git(&self, dep_name: &str, detail: &DependencyDetail) -> NuResult<(PathBuf, String)> {
        let url = detail.git.as_ref().expect("checked by caller");
        let cache = self.root_dir.join(".nula").join("git");
        std::fs::create_dir_all(&cache).map_err(|e| NuError::PackageError {
            msg: format!("cannot create {}: {}", cache.display(), e),
            span: Span::default(),
        })?;
        let dest = cache.join(dep_name);
        let commit_file = cache.join(format!("{}.commit", dep_name));

        let mut commit: Option<String> = None;
        if dest.join(MANIFEST_FILE).exists() {
            // Cached checkout: keep it only while its recorded commit still
            // matches what the dependency spec resolves to now.
            let current = current_git_commit(&dest, detail)?;
            let recorded = std::fs::read_to_string(&commit_file)
                .ok()
                .map(|s| s.trim().to_string());
            if recorded.as_deref() == Some(current.as_str()) {
                commit = Some(current);
            } else {
                // Stale: the ref moved, the remote changed, or the checkout
                // predates commit recording. Drop it so the clone re-runs.
                std::fs::remove_dir_all(&dest).map_err(|e| NuError::PackageError {
                    msg: format!("cannot clear stale checkout {}: {}", dest.display(), e),
                    span: Span::default(),
                })?;
            }
        }

        if commit.is_none() {
            // No usable checkout yet: (re)clone.
            if dest.exists() {
                std::fs::remove_dir_all(&dest).map_err(|e| NuError::PackageError {
                    msg: format!("cannot clear stale checkout {}: {}", dest.display(), e),
                    span: Span::default(),
                })?;
            }
            let mut cmd = Command::new("git");
            cmd.arg("clone").arg("--depth").arg("1");
            if let Some(branch) = detail.branch.as_ref().or(detail.tag.as_ref()) {
                cmd.arg("--branch").arg(branch);
            }
            cmd.arg(url).arg(&dest);
            let output = cmd.output().map_err(|e| NuError::PackageError {
                msg: format!("failed to run git clone for '{}': {}", dep_name, e),
                span: Span::default(),
            })?;
            if !output.status.success() {
                return Err(NuError::PackageError {
                    msg: format!(
                        "git clone of '{}' failed: {}",
                        url,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                    span: Span::default(),
                });
            }

            if let Some(rev) = &detail.rev {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(&dest)
                    .arg("checkout")
                    .arg(rev)
                    .output()
                    .map_err(|e| NuError::PackageError {
                        msg: format!("failed to run git checkout for '{}': {}", dep_name, e),
                        span: Span::default(),
                    })?;
                if !output.status.success() {
                    return Err(NuError::PackageError {
                        msg: format!(
                            "git checkout {} of '{}' failed: {}",
                            rev,
                            url,
                            String::from_utf8_lossy(&output.stderr).trim()
                        ),
                        span: Span::default(),
                    });
                }
            }

            let head = git_rev_parse(&dest, "HEAD")?;
            std::fs::write(&commit_file, format!("{}\n", head)).map_err(|e| {
                NuError::PackageError {
                    msg: format!("cannot write {}: {}", commit_file.display(), e),
                    span: Span::default(),
                }
            })?;
            commit = Some(head);
        }

        let dir = std::fs::canonicalize(&dest).map_err(|e| NuError::PackageError {
            msg: format!("cannot resolve {}: {}", dest.display(), e),
            span: Span::default(),
        })?;
        Ok((dir, commit.expect("both branches record a commit")))
    }

    /// Resolve a path or git dependency. Returns the directory and optional git commit.
    fn resolve_detailed(
        &self,
        parent_dir: &Path,
        dep_name: &str,
        detail: &DependencyDetail,
    ) -> NuResult<(PathBuf, Option<String>)> {
        if let Some(path) = &detail.path {
            let dir = parent_dir.join(path);
            let dir = std::fs::canonicalize(&dir).map_err(|e| NuError::PackageError {
                msg: format!(
                    "cannot resolve path dependency '{}' at {}: {}",
                    dep_name,
                    dir.display(),
                    e
                ),
                span: Span::default(),
            })?;
            Ok((dir, None))
        } else if detail.git.is_some() {
            let (dir, commit) = self.fetch_git(dep_name, detail)?;
            Ok((dir, Some(commit)))
        } else {
            Err(NuError::PackageError {
                msg: format!("dependency '{}' must specify a path or git URL", dep_name),
                span: Span::default(),
            })
        }
    }

    /// Determine the registry URL for resolving version dependencies.
    /// Uses the parent package's `[package] registry`, falling back to the
    /// root manifest's default.
    fn resolve_dependency_registry(&self, parent_dir: &Path) -> NuResult<String> {
        if let Ok(manifest) = Manifest::load(parent_dir) {
            if let Some(url) = &manifest.package.registry {
                return Ok(url.clone());
            }
        }
        self.default_registry.clone().ok_or_else(|| NuError::PackageError {
            msg: "No registry configured — set `registry` in `[package]` of Nulang.toml, or use path/git dependencies".to_string(),
            span: Span::default(),
        })
    }

    /// Fetch a package from the registry, extract it to
    /// `<root>/.nula/registry/<name>-<version>/`, and return its directory.
    fn fetch_registry(
        &self,
        dep_name: &str,
        requirement: &str,
        registry_url: &str,
    ) -> NuResult<(PathBuf, String)> {
        let client = crate::registry::RegistryClient::new(registry_url.to_string(), None);
        let versions = client
            .list_versions(dep_name)
            .map_err(|e| NuError::PackageError {
                msg: format!(
                    "failed to list versions for '{}' from {}: {}",
                    dep_name, registry_url, e
                ),
                span: Span::default(),
            })?;

        // Pick the best satisfying version (latest wins when multiple satisfy).
        let best = versions
            .iter()
            .filter(|v| version_satisfies(requirement, v).unwrap_or(false))
            .max_by(|a, b| compare_semver(a, b).unwrap_or(std::cmp::Ordering::Less))
            .ok_or_else(|| NuError::PackageError {
                msg: format!(
                    "no version of '{}' satisfies requirement {} (available: {:?})",
                    dep_name, requirement, versions
                ),
                span: Span::default(),
            })?;

        let cache = self.root_dir.join(".nula").join("registry");
        let dest = cache.join(format!("{}-{}", dep_name, best));

        // Reuse cached extraction.
        if dest.join(MANIFEST_FILE).exists() {
            let dir = std::fs::canonicalize(&dest).map_err(|e| NuError::PackageError {
                msg: format!("cannot resolve {}: {}", dest.display(), e),
                span: Span::default(),
            })?;
            return Ok((dir, best.clone()));
        }

        // Download and extract.
        let tarball = client
            .fetch(dep_name, best)
            .map_err(|e| NuError::PackageError {
                msg: format!(
                    "failed to fetch '{}-{}' from {}: {}",
                    dep_name, best, registry_url, e
                ),
                span: Span::default(),
            })?;

        std::fs::create_dir_all(&dest).map_err(|e| NuError::PackageError {
            msg: format!("cannot create {}: {}", dest.display(), e),
            span: Span::default(),
        })?;

        let tar_bytes = crate::package::archive::decode_archive(&tarball).map_err(|e| {
            NuError::PackageError {
                msg: format!(
                    "cannot decode '{}-{}' registry archive from {}: {}",
                    dep_name, best, registry_url, e
                ),
                span: Span::default(),
            }
        })?;
        let cursor = std::io::Cursor::new(tar_bytes);
        let mut archive = tar::Archive::new(cursor);
        archive.unpack(&dest).map_err(|e| NuError::PackageError {
            msg: format!("cannot extract '{}' to {}: {}", dep_name, dest.display(), e),
            span: Span::default(),
        })?;

        let dir = std::fs::canonicalize(&dest).map_err(|e| NuError::PackageError {
            msg: format!("cannot resolve {}: {}", dest.display(), e),
            span: Span::default(),
        })?;
        Ok((dir, best.clone()))
    }
}

/// Run `git -C <dir> rev-parse <spec>` and return the resolved commit SHA.
fn git_rev_parse(dir: &Path, spec: &str) -> NuResult<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg(spec)
        .output()
        .map_err(|e| NuError::PackageError {
            msg: format!("failed to run git rev-parse in '{}': {}", dir.display(), e),
            span: Span::default(),
        })?;
    if !output.status.success() {
        return Err(NuError::PackageError {
            msg: format!(
                "git rev-parse {} in '{}' failed: {}",
                spec,
                dir.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            span: Span::default(),
        });
    }
    Ok(String::from_utf8(output.stdout)
        .map_err(|e| NuError::PackageError {
            msg: format!(
                "git rev-parse in '{}' returned non-UTF-8 output: {}",
                dir.display(),
                e
            ),
            span: Span::default(),
        })?
        .trim()
        .to_string())
}

/// The commit `detail` resolves to inside the existing checkout `dest`.
///
/// A pinned rev is compared locally (fetching cannot move a pin); branch,
/// tag and unpinned specs are refreshed from the remote first, so a moved
/// ref is detected and invalidates the cache.
fn current_git_commit(dest: &Path, detail: &DependencyDetail) -> NuResult<String> {
    let spec = if let Some(rev) = &detail.rev {
        rev.clone()
    } else {
        let output = Command::new("git")
            .arg("-C")
            .arg(dest)
            .arg("fetch")
            .output()
            .map_err(|e| NuError::PackageError {
                msg: format!("failed to run git fetch in '{}': {}", dest.display(), e),
                span: Span::default(),
            })?;
        if !output.status.success() {
            return Err(NuError::PackageError {
                msg: format!(
                    "git fetch in '{}' failed: {}",
                    dest.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                span: Span::default(),
            });
        }
        match (&detail.branch, &detail.tag) {
            (Some(branch), _) => format!("origin/{}", branch),
            (None, Some(tag)) => format!("refs/tags/{}", tag),
            (None, None) => "FETCH_HEAD".to_string(),
        }
    };
    git_rev_parse(dest, &spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch dir per test (the suite runs tests in parallel).
    fn fresh_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nulang_resolver_test_{}_{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn write_manifest(dir: &Path, name: &str, version: &str, deps: &str) {
        std::fs::create_dir_all(dir).expect("package dir should be created");
        std::fs::write(
            dir.join(MANIFEST_FILE),
            format!(
                "[package]\nname = \"{}\"\nversion = \"{}\"\n\n[dependencies]\n{}",
                name, version, deps
            ),
        )
        .expect("manifest should be written");
    }

    #[test]
    fn test_parse_semver_and_ordering() {
        assert_eq!(parse_semver("1.2.3").unwrap(), (1, 2, 3));
        assert_eq!(parse_semver("0.1").unwrap(), (0, 1, 0));
        assert!(parse_semver("1.x").is_err());
        assert!(parse_semver("1.2.3.4").is_err());
        assert_eq!(
            compare_semver("0.10.0", "0.9.0").unwrap(),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_semver("1.0.0", "1.0.0").unwrap(),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn test_version_satisfies() {
        assert!(version_satisfies("0.1.0", "0.1.0").unwrap());
        assert!(version_satisfies("0.1.0", "0.1.2").unwrap());
        assert!(version_satisfies("1.1.0", "1.2.0").unwrap());
        assert!(!version_satisfies("0.1.0", "0.2.0").unwrap());
        assert!(!version_satisfies("1.0.0", "2.0.0").unwrap());
        assert!(!version_satisfies("1.2.0", "1.1.0").unwrap());
    }

    #[test]
    fn test_resolve_local_path_dependencies_topological_order() {
        let dir = fresh_dir("path_topo");
        let app_dir = dir.join("app");
        let util_dir = dir.join("util");
        let base_dir = dir.join("base");
        write_manifest(&base_dir, "base", "0.1.0", "");
        write_manifest(
            &util_dir,
            "util",
            "0.1.0",
            "base = { path = \"../base\" }\n",
        );
        write_manifest(
            &app_dir,
            "app",
            "1.0.0",
            "util = { path = \"../util\", version = \"0.1.0\" }\nbase = { path = \"../base\" }\n",
        );

        let manifest = Manifest::load(&app_dir).unwrap();
        let resolution = resolve(&app_dir, &manifest).expect("path deps should resolve");

        // base is a shared dependency of both app and util and must appear
        // exactly once, before both of its dependents.
        let order: Vec<&str> = resolution
            .packages
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(order, vec!["base", "util", "app"]);
        assert_eq!(resolution.root().name, "app");
        assert_eq!(resolution.dependencies().len(), 2);

        let lockfile = resolution.to_lockfile();
        assert_eq!(lockfile.package.len(), 2);
        assert!(lockfile.package[0].source.starts_with("path+"));
        let round_tripped = Lockfile::parse(&lockfile.to_toml().unwrap()).unwrap();
        assert_eq!(lockfile, round_tripped);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_version_requirement_mismatch_fails() {
        let dir = fresh_dir("version_mismatch");
        let app_dir = dir.join("app");
        let util_dir = dir.join("util");
        write_manifest(&util_dir, "util", "0.2.0", "");
        write_manifest(
            &app_dir,
            "app",
            "1.0.0",
            "util = { path = \"../util\", version = \"0.1.0\" }\n",
        );

        let manifest = Manifest::load(&app_dir).unwrap();
        let err = resolve(&app_dir, &manifest).expect_err("incompatible version must fail");
        match err {
            NuError::PackageError { msg, .. } => assert!(msg.contains("requires version 0.1.0")),
            other => panic!("expected PackageError, got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_dependency_name_mismatch_fails() {
        let dir = fresh_dir("name_mismatch");
        let app_dir = dir.join("app");
        let util_dir = dir.join("util");
        write_manifest(&util_dir, "not-util", "0.1.0", "");
        write_manifest(&app_dir, "app", "1.0.0", "util = { path = \"../util\" }\n");

        let manifest = Manifest::load(&app_dir).unwrap();
        let err = resolve(&app_dir, &manifest).expect_err("name mismatch must fail");
        assert!(matches!(err, NuError::PackageError { msg: _, span: _ }));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_registry_dependency_fails() {
        let dir = fresh_dir("registry_dep");
        let app_dir = dir.join("app");
        write_manifest(&app_dir, "app", "1.0.0", "serde = \"1.0\"\n");

        let manifest = Manifest::load(&app_dir).unwrap();
        let err = resolve(&app_dir, &manifest).expect_err("registry deps are unsupported");
        match err {
            NuError::PackageError { msg, .. } => assert!(msg.contains("registry")),
            other => panic!("expected PackageError, got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_dependency_cycle_fails() {
        let dir = fresh_dir("cycle");
        let a_dir = dir.join("a");
        let b_dir = dir.join("b");
        write_manifest(&a_dir, "a", "0.1.0", "b = { path = \"../b\" }\n");
        write_manifest(&b_dir, "b", "0.1.0", "a = { path = \"../a\" }\n");

        let manifest = Manifest::load(&a_dir).unwrap();
        let err = resolve(&a_dir, &manifest).expect_err("circular deps must fail");
        match err {
            NuError::PackageError { msg, .. } => assert!(msg.contains("cycle")),
            other => panic!("expected PackageError, got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_conflicting_sources_fails() {
        let dir = fresh_dir("conflict");
        let app_dir = dir.join("app");
        let util_a = dir.join("util-a");
        let util_b = dir.join("util-b");
        // Two different directories both claiming to be package `util`.
        write_manifest(&util_a, "util", "0.1.0", "");
        write_manifest(&util_b, "util", "0.1.0", "");
        write_manifest(
            &app_dir,
            "app",
            "1.0.0",
            "util = { path = \"../util-a\" }\nother = { path = \"../other\" }\n",
        );
        // `other` depends on `util` from a different directory.
        write_manifest(
            &dir.join("other"),
            "other",
            "0.1.0",
            "util = { path = \"../util-b\" }\n",
        );

        let manifest = Manifest::load(&app_dir).unwrap();
        let err = resolve(&app_dir, &manifest).expect_err("conflicting sources must fail");
        match err {
            NuError::PackageError { msg, .. } => assert!(msg.contains("conflicting sources")),
            other => panic!("expected PackageError, got {:?}", other),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lockfile_includes_content_hash() {
        // A resolved local-path dependency must carry a BLAKE3 content hash
        // in the lockfile, computed from its .nula source files.
        let dir = fresh_dir("content_hash");
        let app_dir = dir.join("app");
        let util_dir = dir.join("util");
        write_manifest(&app_dir, "app", "1.0.0", "util = { path = \"../util\" }\n");
        write_manifest(&util_dir, "util", "0.1.0", "");
        // A source file whose content we can hash.
        std::fs::write(util_dir.join("lib.nula"), "fn main() { 1 }").unwrap();

        let manifest = Manifest::load(&app_dir).unwrap();
        let resolution = resolve(&app_dir, &manifest).unwrap();
        let lockfile = resolution.to_lockfile();
        let util = &lockfile.package[0];
        assert_eq!(util.name, "util");
        assert!(
            !util.content_hash.is_empty(),
            "content_hash must be computed for path dependencies"
        );
        // The hash is stable: re-resolve and compare.
        let lockfile2 = resolve(&app_dir, &manifest).unwrap().to_lockfile();
        assert_eq!(
            lockfile2.package[0].content_hash, util.content_hash,
            "content hash must be stable across resolutions of the same source"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Create a throwaway git repo at `dir` hosting package `name` v`version`
    /// with a `lib.nula` containing `content`, and return its HEAD commit SHA.
    fn git_remote(dir: &Path, name: &str, version: &str, content: &str) -> String {
        write_manifest(dir, name, version, "");
        std::fs::write(dir.join("lib.nula"), content).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git should run");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Nulang Test"]);
        git_commit_all(dir, "initial")
    }

    /// Stage everything in `dir` and commit it, returning the new HEAD SHA.
    fn git_commit_all(dir: &Path, msg: &str) -> String {
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git should run");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["add", "."]);
        run(&["commit", "-q", "-m", msg]);
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .expect("git should run");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }

    #[test]
    fn test_git_dependency_refetches_when_remote_moves() {
        // An unpinned git dependency records the resolved commit in the
        // lockfile and re-fetches (re-clones) when the remote's default
        // branch moves, so the cached checkout is never stale.
        let dir = fresh_dir("git_refetch");
        let remote = dir.join("remote");
        let app_dir = dir.join("app");
        let c1 = git_remote(&remote, "util", "0.1.0", "fn main() { 1 }");
        write_manifest(
            &app_dir,
            "app",
            "1.0.0",
            &format!("util = {{ git = \"{}\" }}\n", remote.display()),
        );

        let manifest = Manifest::load(&app_dir).unwrap();
        let lockfile = resolve(&app_dir, &manifest).unwrap().to_lockfile();
        let util = &lockfile.package[0];
        assert_eq!(util.commit, c1, "lockfile must record the fetched commit");
        assert!(
            util.content_hash.is_empty(),
            "git deps are keyed by commit, not content hash"
        );

        // The commit is also recorded next to the cached checkout, and a
        // sentinel file proves the checkout itself is reused unchanged.
        let cache = app_dir.join(".nula").join("git");
        assert_eq!(
            std::fs::read_to_string(cache.join("util.commit"))
                .unwrap()
                .trim(),
            c1
        );
        std::fs::write(cache.join("util").join("sentinel.txt"), "keep me").unwrap();

        let lockfile2 = resolve(&app_dir, &manifest).unwrap().to_lockfile();
        assert_eq!(lockfile2.package[0].commit, c1);
        assert!(
            cache.join("util").join("sentinel.txt").exists(),
            "an unchanged dependency must reuse the cached checkout"
        );

        // Move the remote's default branch: the next resolution must notice
        // the mismatch and re-fetch.
        std::fs::write(remote.join("lib.nula"), "fn main() { 2 }").unwrap();
        let c2 = git_commit_all(&remote, "second");
        let lockfile3 = resolve(&app_dir, &manifest).unwrap().to_lockfile();
        assert_eq!(
            lockfile3.package[0].commit, c2,
            "a moved branch must invalidate the cache"
        );
        assert_eq!(
            std::fs::read_to_string(cache.join("util").join("lib.nula")).unwrap(),
            "fn main() { 2 }",
            "the stale checkout must be replaced by a fresh clone"
        );
        assert!(
            !cache.join("util").join("sentinel.txt").exists(),
            "the stale checkout must be wiped, sentinel included"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_git_dependency_branch_tracks_remote() {
        // Same staleness check for an explicit branch: the cached checkout
        // is invalidated when the branch's tip moves on the remote.
        let dir = fresh_dir("git_branch");
        let remote = dir.join("remote");
        let app_dir = dir.join("app");
        let c1 = git_remote(&remote, "util", "0.1.0", "fn main() { 1 }");
        write_manifest(
            &app_dir,
            "app",
            "1.0.0",
            &format!(
                "util = {{ git = \"{}\", branch = \"main\" }}\n",
                remote.display()
            ),
        );

        let manifest = Manifest::load(&app_dir).unwrap();
        let lockfile = resolve(&app_dir, &manifest).unwrap().to_lockfile();
        assert_eq!(lockfile.package[0].commit, c1);

        std::fs::write(remote.join("lib.nula"), "fn main() { 2 }").unwrap();
        let c2 = git_commit_all(&remote, "second");
        let lockfile2 = resolve(&app_dir, &manifest).unwrap().to_lockfile();
        assert_eq!(
            lockfile2.package[0].commit, c2,
            "a moved branch must invalidate the cache"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_git_dependency_pinned_rev_is_stable() {
        // A rev-pinned git dependency must not move when the remote does:
        // the recorded commit stays the pin on every resolution.
        let dir = fresh_dir("git_pin");
        let remote = dir.join("remote");
        let app_dir = dir.join("app");
        let c1 = git_remote(&remote, "util", "0.1.0", "fn main() { 1 }");
        write_manifest(
            &app_dir,
            "app",
            "1.0.0",
            &format!(
                "util = {{ git = \"{}\", rev = \"{}\" }}\n",
                remote.display(),
                c1
            ),
        );

        let manifest = Manifest::load(&app_dir).unwrap();
        let lockfile = resolve(&app_dir, &manifest).unwrap().to_lockfile();
        assert_eq!(lockfile.package[0].commit, c1);

        // Remote moves forward; the pinned checkout must be left untouched.
        std::fs::write(remote.join("lib.nula"), "fn main() { 2 }").unwrap();
        let _c2 = git_commit_all(&remote, "second");
        let lockfile2 = resolve(&app_dir, &manifest).unwrap().to_lockfile();
        assert_eq!(
            lockfile2.package[0].commit, c1,
            "a pinned rev must not follow the remote"
        );
        let cache = app_dir.join(".nula").join("git");
        assert_eq!(
            std::fs::read_to_string(cache.join("util").join("lib.nula")).unwrap(),
            "fn main() { 1 }",
            "the pinned checkout must keep its original content"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
