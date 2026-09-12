//! Canonical strong identities for Nulang packages.
//!
//! Legacy lockfile `content_hash` concatenates a subset of source files and is
//! intentionally preserved for compatibility. Semantic closure needs a
//! separate source identity with unambiguous framing, stable relative paths,
//! and package metadata included in the hash. This module defines that
//! canonicalization without changing legacy pin semantics.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path};

use crate::content_identity::{SemanticId, SourceId};

const PACKAGE_SOURCE_CANONICAL_VERSION: &[u8] = b"nulang.package-source.v1\0";
const PACKAGE_SEMANTIC_CANONICAL_VERSION: &[u8] = b"nulang.package-semantic.v1\0";

/// Compute the strong source identity for one package directory.
///
/// The canonical input contains `Nulang.toml` plus every regular `.nula` file
/// below the package root. Inputs are sorted by normalized relative path and
/// length-framed, so path renames, file-boundary changes, manifest changes,
/// and byte changes all invalidate the identity. Build/cache/VCS directories
/// are excluded. Symlinks are rejected rather than followed outside the
/// package boundary.
pub fn source_id_for_package_dir(root: &Path) -> io::Result<SourceId> {
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("package source directory does not exist: {}", root.display()),
        ));
    }

    let manifest = root.join("Nulang.toml");
    if !manifest.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("package manifest does not exist: {}", manifest.display()),
        ));
    }

    let mut paths = vec![manifest];
    collect_nula_files(root, &mut paths)?;
    let mut files = paths
        .into_iter()
        .map(|path| {
            let relative = canonical_relative_path(root, &path)?;
            Ok((relative, path))
        })
        .collect::<io::Result<Vec<_>>>()?;
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut canonical = Vec::new();
    canonical.extend_from_slice(PACKAGE_SOURCE_CANONICAL_VERSION);
    put_u32(&mut canonical, files.len() as u32);
    for (relative, file) in files {
        let contents = fs::read(&file)?;
        put_bytes(&mut canonical, relative.as_bytes());
        put_bytes(&mut canonical, &contents);
    }
    Ok(SourceId::from_bytes(&canonical))
}

/// Derive a package semantic identity from canonical per-module semantic IDs.
///
/// Module traversal order is irrelevant, but module paths are semantic: moving
/// a module can change import/module resolution and therefore changes package
/// identity. Dependency semantic IDs are passed through to [`SemanticId`],
/// which canonicalizes their ordering and duplicates.
pub fn semantic_id_for_package<I, P, D>(
    modules: I,
    dependency_semantic_ids: D,
) -> Result<SemanticId, PackageIdentityError>
where
    I: IntoIterator<Item = (P, SemanticId)>,
    P: AsRef<str>,
    D: IntoIterator<Item = SemanticId>,
{
    let mut canonical_modules = BTreeMap::new();
    for (path, semantic_id) in modules {
        let path = normalize_logical_path(path.as_ref())?;
        if canonical_modules.insert(path.clone(), semantic_id).is_some() {
            return Err(PackageIdentityError::DuplicateModulePath(path));
        }
    }

    let mut canonical = Vec::new();
    canonical.extend_from_slice(PACKAGE_SEMANTIC_CANONICAL_VERSION);
    put_u32(&mut canonical, canonical_modules.len() as u32);
    for (path, semantic_id) in canonical_modules {
        put_bytes(&mut canonical, path.as_bytes());
        canonical.extend_from_slice(semantic_id.as_bytes());
    }

    Ok(SemanticId::from_canonical_bytes(
        &canonical,
        dependency_semantic_ids,
    ))
}

fn collect_nula_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "package source identity does not follow symlink: {}",
                    path.display()
                ),
            ));
        }
        if file_type.is_dir() {
            let name = entry.file_name();
            if name == ".git" || name == ".nula" || name == "target" {
                continue;
            }
            collect_nula_files(&path, files)?;
        } else if file_type.is_file()
            && path.extension().and_then(|extension| extension.to_str()) == Some("nula")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn canonical_relative_path(root: &Path, path: &Path) -> io::Result<String> {
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "package identity input {} is outside package root {}",
                path.display(),
                root.display()
            ),
        )
    })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("package path is not valid UTF-8: {}", relative.display()),
                    )
                })?;
                if part.contains('\\') {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "package path component contains ambiguous separator: {}",
                            relative.display()
                        ),
                    ));
                }
                parts.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("non-canonical package path: {}", relative.display()),
                ));
            }
        }
    }
    normalize_logical_path(&parts.join("/"))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

fn normalize_logical_path(path: &str) -> Result<String, PackageIdentityError> {
    if path.is_empty() {
        return Err(PackageIdentityError::InvalidModulePath(path.to_string()));
    }
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/')
        || normalized
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(PackageIdentityError::InvalidModulePath(path.to_string()));
    }
    Ok(normalized)
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageIdentityError {
    InvalidModulePath(String),
    DuplicateModulePath(String),
}

impl std::fmt::Display for PackageIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidModulePath(path) => write!(f, "invalid canonical module path '{path}'"),
            Self::DuplicateModulePath(path) => {
                write!(f, "duplicate canonical module path '{path}'")
            }
        }
    }
}

impl std::error::Error for PackageIdentityError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_package_identity_{name}_{}",
            std::process::id()
        ))
    }

    fn write_package(root: &Path, manifest: &str, files: &[(&str, &str)]) {
        let _ = fs::remove_dir_all(root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Nulang.toml"), manifest).unwrap();
        for (path, contents) in files {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
    }

    #[test]
    fn source_identity_is_deterministic_and_tracks_manifest_source_and_paths() {
        let root = scratch("source");
        write_package(
            &root,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            &[
                ("src/main.nula", "fn main() { 1 }"),
                ("src/util.nula", "fn util() { 2 }"),
            ],
        );
        let first = source_id_for_package_dir(&root).unwrap();
        let same = source_id_for_package_dir(&root).unwrap();
        assert_eq!(first, same);

        fs::write(root.join("src/util.nula"), "fn util() { 3 }").unwrap();
        let changed_source = source_id_for_package_dir(&root).unwrap();
        assert_ne!(first, changed_source);

        fs::write(root.join("src/util.nula"), "fn util() { 2 }").unwrap();
        fs::write(
            root.join("Nulang.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
        assert_ne!(first, source_id_for_package_dir(&root).unwrap());

        fs::write(
            root.join("Nulang.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::rename(root.join("src/util.nula"), root.join("src/helper.nula")).unwrap();
        assert_ne!(first, source_id_for_package_dir(&root).unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn package_cache_and_build_outputs_do_not_change_source_identity() {
        let root = scratch("excluded");
        write_package(
            &root,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            &[("src/main.nula", "fn main() { 1 }")],
        );
        let first = source_id_for_package_dir(&root).unwrap();
        fs::create_dir_all(root.join(".nula/git/dependency")).unwrap();
        fs::write(root.join(".nula/git/dependency/cache.nula"), "cache").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/generated.nula"), "generated").unwrap();
        assert_eq!(first, source_id_for_package_dir(&root).unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn semantic_identity_canonicalizes_module_and_dependency_order() {
        let module_a = SemanticId::from_canonical_bytes(b"module-a", []);
        let module_b = SemanticId::from_canonical_bytes(b"module-b", []);
        let dep_a = SemanticId::from_canonical_bytes(b"dep-a", []);
        let dep_b = SemanticId::from_canonical_bytes(b"dep-b", []);

        let first = semantic_id_for_package(
            [("src/a.nula", module_a), ("src/b.nula", module_b)],
            [dep_a, dep_b],
        )
        .unwrap();
        let reordered = semantic_id_for_package(
            [("src/b.nula", module_b), ("src/a.nula", module_a)],
            [dep_b, dep_a, dep_b],
        )
        .unwrap();
        assert_eq!(first, reordered);
    }

    #[test]
    fn duplicate_or_noncanonical_module_paths_fail_closed() {
        let module = SemanticId::from_canonical_bytes(b"module", []);
        assert!(matches!(
            semantic_id_for_package([("src/a.nula", module), ("src/a.nula", module)], []),
            Err(PackageIdentityError::DuplicateModulePath(_))
        ));
        assert!(matches!(
            semantic_id_for_package([("../src/a.nula", module)], []),
            Err(PackageIdentityError::InvalidModulePath(_))
        ));
    }
}