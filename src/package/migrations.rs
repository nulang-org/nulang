//! Forward-only SQL migration scaffolding for application packages.
//!
//! This module creates numbered SQL source files; it deliberately does not
//! connect to a database, execute DDL, or mark migrations as applied.
//! Application/runtime state migrations are owned by separate Nulang semantics.

use crate::types::{NuError, NuResult, Span};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

fn error(message: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: message.into(),
        span: Span::default(),
    }
}

fn valid_migration_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

/// Create the next ordered SQL migration. Versions are six-digit, positive,
/// monotonically increasing integers. A concurrent creator cannot overwrite an
/// existing file, even if both processes calculate the same next version.
pub fn generate_migration(root: &Path, name: &str) -> NuResult<PathBuf> {
    if !valid_migration_name(name) {
        return Err(error(
            "migration name must start with a lowercase letter and contain only \
             lowercase letters, digits, or underscores (64 bytes maximum)",
        ));
    }
    if !root.join("Nulang.toml").is_file() {
        return Err(error("nula generate migration requires a Nulang.toml package root"));
    }

    let db_dir = root.join("db");
    let migrations_dir = db_dir.join("migrations");
    for dir in [&db_dir, &migrations_dir] {
        match fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(error(format!(
                    "migration directory {} must be a real directory, not a symlink",
                    dir.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(error(format!("cannot inspect {}: {err}", dir.display()))),
        }
    }
    fs::create_dir_all(&migrations_dir).map_err(|err| {
        error(format!("cannot create {}: {err}", migrations_dir.display()))
    })?;

    let mut maximum_version = 0u32;
    let mut seen_versions = HashSet::new();
    for entry in fs::read_dir(&migrations_dir).map_err(|err| {
        error(format!("cannot list {}: {err}", migrations_dir.display()))
    })? {
        let entry = entry.map_err(|err| error(format!("cannot read migration entry: {err}")))?;
        let filename = entry.file_name().to_string_lossy().into_owned();
        if !filename.ends_with(".sql") {
            continue;
        }
        let (version_string, suffix) = filename.split_once('_').ok_or_else(|| {
            error(format!("malformed SQL migration filename: {filename}"))
        })?;
        let stem = suffix.strip_suffix(".sql").ok_or_else(|| {
            error(format!("malformed SQL migration filename: {filename}"))
        })?;
        if version_string.len() != 6 || !valid_migration_name(stem) {
            return Err(error(format!("malformed SQL migration filename: {filename}")));
        }
        let version: u32 = version_string.parse().map_err(|_| {
            error(format!("malformed SQL migration version: {filename}"))
        })?;
        if version == 0 || !seen_versions.insert(version) {
            return Err(error(format!("duplicate or zero SQL migration version: {filename}")));
        }
        maximum_version = maximum_version.max(version);
    }

    let next_version = maximum_version
        .checked_add(1)
        .filter(|version| *version <= 999_999)
        .ok_or_else(|| error("SQL migration version sequence exhausted"))?;
    let path = migrations_dir.join(format!("{next_version:06}_{name}.sql"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|err| error(format!("cannot create {}: {err}", path.display())))?;
    let content = format!(
        "-- Nulang SQL migration {next_version:06}: {name}\n\
         -- Forward-only migration. Write idempotent, database-specific SQL below.\n\
         -- This file is NOT automatically applied by nula generate.\n\n"
    );
    if let Err(err) = file.write_all(content.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(error(format!("cannot write {}: {err}", path.display())));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "nulang_sql_migration_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("Nulang.toml"),
            "[package]\nname = \"test-migrations\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        root
    }

    #[test]
    fn creates_monotonically_numbered_sql_without_running_any_queries() {
        let root = fixture();
        let first = generate_migration(&root, "create_customers").unwrap();
        let second = generate_migration(&root, "add_email_index").unwrap();
        assert_eq!(first.file_name().unwrap().to_str().unwrap(), "000001_create_customers.sql");
        assert_eq!(second.file_name().unwrap().to_str().unwrap(), "000002_add_email_index.sql");
        assert!(fs::read_to_string(&first).unwrap().contains("NOT automatically applied"));
        assert_eq!(fs::read_dir(root.join("db/migrations")).unwrap().count(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_path_traversal_and_unsafe_migration_names() {
        let root = fixture();
        for invalid in ["../outside", "drop-table", "UPPER", "1invalid", "", "a.b"] {
            assert!(generate_migration(&root, invalid).is_err(), "{invalid}");
        }
        assert!(!root.join("db").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_ambiguous_existing_sql_versions() {
        let root = fixture();
        let dir = root.join("db/migrations");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("000001_one.sql"), "SELECT 1;").unwrap();
        fs::write(dir.join("000001_two.sql"), "SELECT 2;").unwrap();
        let err = generate_migration(&root, "third").unwrap_err();
        assert!(err.to_string().contains("duplicate"));
        assert!(!dir.join("000002_third.sql").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn never_writes_to_a_symlinked_migrations_directory() {
        let root = fixture();
        let external = root.join("external");
        fs::create_dir_all(&external).unwrap();
        fs::create_dir_all(root.join("db")).unwrap();
        std::os::unix::fs::symlink(&external, root.join("db/migrations")).unwrap();
        assert!(generate_migration(&root, "create_customers").is_err());
        assert_eq!(fs::read_dir(&external).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}
