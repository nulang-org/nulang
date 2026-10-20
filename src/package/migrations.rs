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


/// Exact source identity; digest covers unmodified SQL bytes, not DB state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlMigration {
    pub version: u32,
    pub file: String,
    pub checksum_blake3: String,
}

/// Read-only comparison with a supplied applied-migration ledger export.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationAudit {
    pub applied: usize,
    pub pending: Vec<SqlMigration>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AppliedMigrationSnapshot {
    schema_version: u32,
    applied: Vec<SqlMigration>,
}

fn checked_migrations_dir(root: &Path) -> NuResult<PathBuf> {
    if !root.join("Nulang.toml").is_file() {
        return Err(error("migration inspection requires a Nulang.toml package root"));
    }
    let db = root.join("db");
    let migrations = db.join("migrations");
    for dir in [&db, &migrations] {
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
    Ok(migrations)
}

fn parsed_sql_filename(file: &str) -> NuResult<u32> {
    let (digits, suffix) = file
        .split_once('_')
        .ok_or_else(|| error(format!("malformed SQL migration filename: {file}")))?;
    let name = suffix
        .strip_suffix(".sql")
        .ok_or_else(|| error(format!("malformed SQL migration filename: {file}")))?;
    if digits.len() != 6
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || !valid_migration_name(name)
    {
        return Err(error(format!("malformed SQL migration filename: {file}")));
    }
    let version: u32 = digits
        .parse()
        .map_err(|_| error(format!("invalid SQL migration version: {file}")))?;
    if version == 0 {
        return Err(error(format!("zero SQL migration version: {file}")));
    }
    Ok(version)
}

fn read_limited(path: &Path, limit: usize) -> NuResult<Vec<u8>> {
    use std::io::Read;
    let meta = fs::symlink_metadata(path)
        .map_err(|err| error(format!("cannot inspect {}: {err}", path.display())))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(error(format!(
            "{} must be a regular file, not a symlink",
            path.display()
        )));
    }
    if meta.len() > limit as u64 {
        return Err(error(format!("file {} exceeds {limit} bytes", path.display())));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|err| error(format!("cannot open {}: {err}", path.display())))?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| error(format!("cannot read {}: {err}", path.display())))?;
    if bytes.len() > limit {
        return Err(error(format!("file {} exceeds {limit} bytes", path.display())));
    }
    Ok(bytes)
}

/// List and checksum exact SQL bytes without modifying filesystem or database.
/// Once any files exist, versions must form a contiguous sequence from 000001.
pub fn read_migration_plan(root: &Path) -> NuResult<Vec<SqlMigration>> {
    let dir = checked_migrations_dir(root)?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut plan = Vec::new();
    for entry in fs::read_dir(&dir)
        .map_err(|err| error(format!("cannot list {}: {err}", dir.display())))?
    {
        let entry = entry.map_err(|err| error(format!("cannot read migration entry: {err}")))?;
        let file = entry.file_name().into_string()
            .map_err(|_| error("SQL migration filenames must use UTF-8"))?;
        if !file.ends_with(".sql") {
            continue;
        }
        if plan.len() >= 10_000 {
            return Err(error("SQL migration inspection supports at most 10000 files"));
        }
        let version = parsed_sql_filename(&file)?;
        let bytes = read_limited(&entry.path(), 4 * 1024 * 1024)?;
        if std::str::from_utf8(&bytes).is_err() {
            return Err(error(format!("SQL migration {file} must contain UTF-8")));
        }
        plan.push(SqlMigration {
            version,
            file,
            checksum_blake3: blake3::hash(&bytes).to_hex().to_string(),
        });
    }
    plan.sort_unstable_by_key(|migration| migration.version);
    for (index, migration) in plan.iter().enumerate() {
        let expected = index as u32 + 1;
        if migration.version != expected {
            return Err(error(format!(
                "SQL migration sequence missing or duplicated version {expected:06}; next source: {}",
                migration.file
            )));
        }
    }
    Ok(plan)
}

/// Compare source against an external, operator-provided ledger JSON snapshot.
/// This does not authenticate its provenance or query the database.
pub fn verify_applied_snapshot(root: &Path, snapshot_path: &Path) -> NuResult<MigrationAudit> {
    let plan = read_migration_plan(root)?;
    let raw = read_limited(snapshot_path, 2 * 1024 * 1024)?;
    let snapshot: AppliedMigrationSnapshot = serde_json::from_slice(&raw)
        .map_err(|err| error(format!("invalid applied migration snapshot JSON: {err}")))?;
    if snapshot.schema_version != 1 {
        return Err(error("unsupported applied migration snapshot schema_version"));
    }
    if snapshot.applied.len() > plan.len() {
        return Err(error("applied migration ledger contains an unknown source version"));
    }
    for (index, applied) in snapshot.applied.iter().enumerate() {
        let expected = &plan[index];
        if applied.version != expected.version {
            return Err(error(format!(
                "applied migrations are out of order at version {:06}",
                expected.version
            )));
        }
        if applied.file != expected.file {
            return Err(error(format!(
                "applied migration filename mismatch at version {:06}",
                expected.version
            )));
        }
        if applied.checksum_blake3 != expected.checksum_blake3 {
            return Err(error(format!(
                "applied migration checksum mismatch at version {:06}",
                expected.version
            )));
        }
    }
    let applied = snapshot.applied.len();
    Ok(MigrationAudit {
        applied,
        pending: plan.into_iter().skip(applied).collect(),
    })
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

    #[test]
    fn migration_plan_digests_exact_sql_bytes_in_version_order() {
        let root = fixture();
        let first = generate_migration(&root, "create_customers").unwrap();
        let second = generate_migration(&root, "add_email_index").unwrap();
        fs::write(&first, "CREATE TABLE customers (id INTEGER PRIMARY KEY);\n").unwrap();
        fs::write(&second, "CREATE INDEX customers_id ON customers(id);\n").unwrap();

        let plan = read_migration_plan(&root).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].version, 1);
        assert_eq!(plan[0].file, "000001_create_customers.sql");
        assert_eq!(plan[1].version, 2);
        assert_eq!(
            plan[0].checksum_blake3,
            blake3::hash(b"CREATE TABLE customers (id INTEGER PRIMARY KEY);\n")
                .to_hex()
                .to_string()
        );
        assert_ne!(plan[0].checksum_blake3, plan[1].checksum_blake3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verifies_external_applied_ledger_prefix_and_reports_only_pending() {
        let root = fixture();
        generate_migration(&root, "first").unwrap();
        generate_migration(&root, "second").unwrap();
        let plan = read_migration_plan(&root).unwrap();
        let snapshot = root.join("trusted-ledger-export.json");
        fs::write(
            &snapshot,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "applied": [plan[0]]
            }))
            .unwrap(),
        )
        .unwrap();

        let audit = verify_applied_snapshot(&root, &snapshot).unwrap();
        assert_eq!(audit.applied, 1);
        assert_eq!(audit.pending.len(), 1);
        assert_eq!(audit.pending[0].version, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_drifted_sql_after_a_snapshot_claims_it_was_applied() {
        let root = fixture();
        let path = generate_migration(&root, "first").unwrap();
        let plan = read_migration_plan(&root).unwrap();
        let snapshot = root.join("trusted-ledger-export.json");
        fs::write(
            &snapshot,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "applied": [plan[0]]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(&path, "-- modified after being recorded\n").unwrap();

        let error = verify_applied_snapshot(&root, &snapshot).unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_gap_missing_source_and_out_of_order_ledger_entries() {
        let root = fixture();
        let one = generate_migration(&root, "one").unwrap();
        let two = generate_migration(&root, "two").unwrap();
        let plan = read_migration_plan(&root).unwrap();
        let snapshot = root.join("ledger.json");

        fs::write(
            &snapshot,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "applied": [plan[1]]
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(verify_applied_snapshot(&root, &snapshot).is_err());

        fs::remove_file(&one).unwrap();
        assert!(read_migration_plan(&root).is_err(), "missing version one");
        assert!(!verify_applied_snapshot(&root, &snapshot).is_ok());
        assert!(two.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn migration_plan_refuses_symlinked_sql_files_or_ledger_snapshots() {
        let root = fixture();
        generate_migration(&root, "first").unwrap();
        let outside = root.join("original.sql");
        fs::write(&outside, "SELECT 1;").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("db/migrations/000002_alias.sql"))
            .unwrap();
        assert!(read_migration_plan(&root).is_err());
        fs::remove_file(root.join("db/migrations/000002_alias.sql")).unwrap();

        let snapshot = root.join("snapshot.json");
        fs::write(&snapshot, r#"{"schema_version":1,"applied":[]}"#).unwrap();
        let snapshot_link = root.join("snapshot-link.json");
        std::os::unix::fs::symlink(&snapshot, &snapshot_link).unwrap();
        assert!(verify_applied_snapshot(&root, &snapshot_link).is_err());
        fs::remove_dir_all(root).unwrap();
    }

}
