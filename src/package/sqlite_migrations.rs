//! SQLite-only transactional application of reviewed SQL migrations.
//! No remote DB/Cloud support. No SQL runs outside explicit opt-in.
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::time::Duration;

use crate::package::migrations::{read_migration_plan, MigrationAudit, SqlMigration};
use crate::types::{NuError, NuResult, Span};

// This table is independent of Nulang actor/workflow durability metadata.
const LEDGER_TABLE: &str = "__nulang_schema_migrations";
const LEDGER_DDL: &str = "CREATE TABLE IF NOT EXISTS __nulang_schema_migrations (\
    version INTEGER PRIMARY KEY CHECK(version > 0), \
    file TEXT NOT NULL, \
    checksum_blake3 TEXT NOT NULL, \
    applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\
)";

fn package_error(message: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: message.into(),
        span: Span::default(),
    }
}

fn sqlite_error(context: &str, error: impl std::fmt::Display) -> NuError {
    package_error(format!("SQLite {context}: {error}"))
}

/// Only one non-transactional DDL statement per file is admitted for now.
/// This is deliberately more restrictive than SQLite syntax: scripts containing
/// semicolons in string literals or SQL block comments must be split/reworked.
/// No transaction-control SQL or references to the ledger are allowed.
fn single_sql_ddl(bytes: &[u8], file: &str) -> NuResult<String> {
    let source = std::str::from_utf8(bytes)
        .map_err(|_| package_error(format!("{file} must contain UTF-8 SQL")))?;
    let sql = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = sql.trim();
    let command = trimmed.strip_suffix(';').unwrap_or(trimmed).trim();
    let invalid = command.is_empty()
        || command.contains(';')
        || command.contains("--")
        || command.contains("/*")
        || command.contains("*/")
        || command.to_ascii_lowercase().contains(LEDGER_TABLE);
    let words = command
        .split_ascii_whitespace()
        .take(3)
        .map(str::to_ascii_uppercase)
        .collect::<Vec<_>>();
    let allowed = matches!(
        words.as_slice(),
        [a, b, ..] if a == "CREATE" && (b == "TABLE" || b == "INDEX")
    ) || matches!(
        words.as_slice(),
        [a, b, c] if a == "CREATE" && b == "UNIQUE" && c == "INDEX"
    ) || matches!(
        words.as_slice(),
        [a, b, ..] if a == "ALTER" && b == "TABLE"
    );
    if invalid || !allowed {
        return Err(package_error(format!(
            "{file} must contain exactly one supported, single DDL statement \
             (CREATE TABLE/INDEX or ALTER TABLE); transaction control and multi-statement \
             scripts are not permitted"
        )));
    }
    Ok(command.to_owned())
}

fn preflight_sql(root: &Path, plan: &[SqlMigration]) -> NuResult<Vec<String>> {
    let mut total_bytes = 0usize;
    let mut sql = Vec::with_capacity(plan.len());
    for migration in plan {
        let path = root.join("db/migrations").join(&migration.file);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|err| package_error(format!("cannot inspect {}: {err}", path.display())))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(package_error(format!("{} is not a regular SQL file", path.display())));
        }
        if metadata.len() > 1024 * 1024 {
            return Err(package_error(format!("{} exceeds the 1 MiB DDL limit", path.display())));
        }
        let mut data = Vec::new();
        fs::File::open(&path)
            .map_err(|err| package_error(format!("cannot open {}: {err}", path.display())))?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut data)
            .map_err(|err| package_error(format!("cannot read {}: {err}", path.display())))?;
        total_bytes = total_bytes.saturating_add(data.len());
        if total_bytes > 16 * 1024 * 1024 {
            return Err(package_error("total SQL migration source exceeds 16 MiB"));
        }
        if blake3::hash(&data).to_hex().to_string() != migration.checksum_blake3 {
            return Err(package_error(format!(
                "migration checksum changed while reading {}",
                migration.file
            )));
        }
        sql.push(single_sql_ddl(&data, &migration.file)?);
    }
    Ok(sql)
}

fn check_database_path(path: &Path) -> NuResult<()> {
    if path.as_os_str().is_empty() || path == Path::new(":memory:") {
        return Err(package_error("SQLite migration requires an explicit local database file"));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let meta = fs::symlink_metadata(parent)
                .map_err(|err| package_error(format!("invalid database directory: {err}")))?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(package_error("SQLite database parent must be a real directory"));
            }
        }
    }
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err(package_error("SQLite database must be a regular file, not a symlink"))
        }
        Ok(_) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(package_error(format!("cannot inspect SQLite database: {err}"))),
    }
}

/// Explicitly apply local SQLite DDL with an atomic database-side ledger.
///
/// An IMMEDIATE transaction serializes migrations with other SQLite writers.
/// The ledger is read and compared while holding that lock; all pending DDL and
/// ledger inserts commit together or roll back together. This is local embedded
/// SQLite only, not libSQL remote replication, PostgreSQL or Cloud deployment.
pub async fn apply_sqlite_migrations(
    root: &Path,
    database_path: &Path,
) -> NuResult<MigrationAudit> {
    let plan = read_migration_plan(root)?;
    // Preflight the entire source plan before opening the database, so a
    // rejected unsafe statement cannot create an empty local DB file.
    let sql = preflight_sql(root, &plan)?;
    check_database_path(database_path)?;
    if plan.is_empty() {
        return Err(package_error(
            "no SQL migration source files; refusing to assume an empty database ledger",
        ));
    }

    let db = libsql::Builder::new_local(database_path)
        .build()
        .await
        .map_err(|err| sqlite_error("open failed", err))?;
    let conn = db.connect().map_err(|err| sqlite_error("connect failed", err))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|err| sqlite_error("busy timeout configuration failed", err))?;

    let tx = conn
        .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
        .await
        .map_err(|err| sqlite_error("begin IMMEDIATE failed", err))?;
    let attempt: NuResult<MigrationAudit> = async {
        tx.execute(LEDGER_DDL, ())
            .await
            .map_err(|err| sqlite_error("ledger initialization failed", err))?;
        let mut rows = tx
            .query(
                "SELECT version, file, checksum_blake3 \
                 FROM __nulang_schema_migrations ORDER BY version",
                (),
            )
            .await
            .map_err(|err| sqlite_error("ledger query failed", err))?;
        let mut applied = 0usize;
        while let Some(row) = rows
            .next()
            .await
            .map_err(|err| sqlite_error("ledger read failed", err))?
        {
            let version: i64 = row
                .get(0)
                .map_err(|err| sqlite_error("ledger version invalid", err))?;
            let file: String = row
                .get(1)
                .map_err(|err| sqlite_error("ledger filename invalid", err))?;
            let digest: String = row
                .get(2)
                .map_err(|err| sqlite_error("ledger checksum invalid", err))?;
            let expected = plan.get(applied).ok_or_else(|| {
                package_error(
                    "SQLite ledger contains applied versions missing from the source plan",
                )
            })?;
            if version != expected.version as i64 {
                return Err(package_error(format!(
                    "SQLite ledger is missing/out of order at version {:06}",
                    expected.version
                )));
            }
            if file != expected.file {
                return Err(package_error(format!(
                    "SQLite ledger filename mismatch at version {:06}",
                    expected.version
                )));
            }
            if digest != expected.checksum_blake3 {
                return Err(package_error(format!(
                    "SQLite ledger checksum mismatch at version {:06}",
                    expected.version
                )));
            }
            applied += 1;
        }
        drop(rows);

        for (migration, statement) in plan.iter().zip(sql.iter()).skip(applied) {
            tx.execute(statement, ())
                .await
                .map_err(|err| sqlite_error(&format!("applying {}", migration.file), err))?;
            tx.execute(
                "INSERT INTO __nulang_schema_migrations(version, file, checksum_blake3) \
                 VALUES (?1, ?2, ?3)",
                libsql::params![
                    migration.version as i64,
                    migration.file.as_str(),
                    migration.checksum_blake3.as_str()
                ],
            )
            .await
            .map_err(|err| sqlite_error(&format!("recording {}", migration.file), err))?;
        }
        Ok(MigrationAudit {
            applied: plan.len(),
            pending: Vec::new(),
        })
    }
    .await;
    match attempt {
        Ok(audit) => {
            tx.commit()
                .await
                .map_err(|err| sqlite_error("commit failed", err))?;
            Ok(audit)
        }
        Err(error) => {
            tx.rollback()
                .await
                .map_err(|rollback| sqlite_error("rollback failed", rollback))?;
            Err(error)
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::migrations::{generate_migration, read_migration_plan};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn fixture() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "nulang_sqlite_apply_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("Nulang.toml"),
            "[package]\nname = \"sqlite-apply-tests\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let db = root.join("app.sqlite");
        (root, db)
    }

    #[tokio::test]
    async fn sqlite_apply_rejects_empty_plan_without_creating_database() {
        let (root, db_path) = fixture();
        let failure = apply_sqlite_migrations(&root, &db_path).await.unwrap_err();
        assert!(
            failure.to_string().contains("no SQL migration source files"),
            "{failure}"
        );
        assert!(!db_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn commits_schema_and_database_ledger_then_is_idempotent() {
        let (root, db_path) = fixture();
        let first = generate_migration(&root, "create_customers").unwrap();
        let second = generate_migration(&root, "add_email_index").unwrap();
        fs::write(&first, "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT);").unwrap();
        fs::write(&second, "CREATE INDEX customers_name ON customers(name);").unwrap();

        let run = apply_sqlite_migrations(&root, &db_path).await.unwrap();
        assert_eq!(run.applied, 2);
        assert!(run.pending.is_empty());
        let again = apply_sqlite_migrations(&root, &db_path).await.unwrap();
        assert_eq!(again.applied, 2);

        let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn
            .query(
                "SELECT version, file, checksum_blake3 \
                 FROM __nulang_schema_migrations ORDER BY version",
                (),
            )
            .await
            .unwrap();
        for record in read_migration_plan(&root).unwrap() {
            let row = rows.next().await.unwrap().unwrap();
            let version: i64 = row.get(0).unwrap();
            let file: String = row.get(1).unwrap();
            let checksum: String = row.get(2).unwrap();
            assert_eq!(version, record.version as i64);
            assert_eq!(file, record.file);
            assert_eq!(checksum, record.checksum_blake3);
        }
        assert!(rows.next().await.unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rolls_back_all_pending_ddl_and_ledger_rows_when_one_statement_fails() {
        let (root, db_path) = fixture();
        let first = generate_migration(&root, "create_customers").unwrap();
        let second = generate_migration(&root, "broken_table").unwrap();
        fs::write(&first, "CREATE TABLE customers (id INTEGER PRIMARY KEY);").unwrap();
        fs::write(&second, "CREATE TABLE broken (").unwrap();

        assert!(apply_sqlite_migrations(&root, &db_path).await.is_err());
        let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
        let conn = db.connect().unwrap();
        let mut tables = conn
            .query(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' \
                 AND name IN ('customers', '__nulang_schema_migrations')",
                (),
            )
            .await
            .unwrap();
        assert!(
            tables.next().await.unwrap().is_none(),
            "failed batch must leave no schema or ledger"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn changed_applied_sql_blocks_later_migrations_without_side_effects() {
        let (root, db_path) = fixture();
        let first = generate_migration(&root, "create_customers").unwrap();
        fs::write(&first, "CREATE TABLE customers (id INTEGER PRIMARY KEY);").unwrap();
        apply_sqlite_migrations(&root, &db_path).await.unwrap();

        fs::write(
            &first,
            "CREATE TABLE customers (id INTEGER PRIMARY KEY, edited TEXT);",
        )
        .unwrap();
        let second = generate_migration(&root, "create_orders").unwrap();
        fs::write(&second, "CREATE TABLE orders (id INTEGER PRIMARY KEY);").unwrap();
        let result = apply_sqlite_migrations(&root, &db_path).await.unwrap_err();
        assert!(result.to_string().contains("checksum mismatch"));

        let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn.query(
            "SELECT name FROM sqlite_master WHERE name = 'orders'", ()
        ).await.unwrap();
        assert!(rows.next().await.unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rejects_multi_statement_and_transaction_control_before_connecting() {
        let (root, db_path) = fixture();
        let first = generate_migration(&root, "bad_sql").unwrap();
        fs::write(
            &first,
            "CREATE TABLE customers (id INTEGER); DROP TABLE __nulang_schema_migrations;",
        ).unwrap();
        let result = apply_sqlite_migrations(&root, &db_path).await.unwrap_err();
        assert!(result.to_string().contains("single DDL statement"));
        assert!(!db_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_database_file() {
        let (root, db_path) = fixture();
        let first = generate_migration(&root, "create_customers").unwrap();
        fs::write(&first, "CREATE TABLE customers (id INTEGER);").unwrap();
        let external = root.join("external.sqlite");
        fs::write(&external, b"").unwrap();
        std::os::unix::fs::symlink(&external, &db_path).unwrap();
        assert!(apply_sqlite_migrations(&root, &db_path).await.is_err());
        assert_eq!(fs::metadata(external).unwrap().len(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn retry_after_a_failed_batch_commits_exactly_once_after_correction() {
        let (root, db_path) = fixture();
        let first = generate_migration(&root, "create_customers").unwrap();
        let second = generate_migration(&root, "create_orders").unwrap();
        fs::write(&first, "CREATE TABLE customers (id INTEGER PRIMARY KEY);").unwrap();
        fs::write(&second, "CREATE TABLE orders (").unwrap();

        assert!(apply_sqlite_migrations(&root, &db_path).await.is_err());

        // Reopening and retrying must not see stale ledger state after failure.
        fs::write(&second, "CREATE TABLE orders (id INTEGER PRIMARY KEY);").unwrap();
        let recovered = apply_sqlite_migrations(&root, &db_path).await.unwrap();
        assert_eq!(recovered.applied, 2);
        let replayed = apply_sqlite_migrations(&root, &db_path).await.unwrap();
        assert_eq!(replayed.applied, 2);

        let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
        let conn = db.connect().unwrap();
        let mut ledger = conn
            .query(
                "SELECT COUNT(*) FROM __nulang_schema_migrations",
                (),
            )
            .await
            .unwrap();
        let row = ledger.next().await.unwrap().unwrap();
        let count: i64 = row.get(0).unwrap();
        assert_eq!(count, 2);
        drop(ledger);
        drop(conn);
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_runners_do_not_double_apply_or_skip_ledger_versions() {
        let (root, db_path) = fixture();
        let first = generate_migration(&root, "create_customers").unwrap();
        fs::write(
            &first,
            "CREATE TABLE customers (id INTEGER PRIMARY KEY);",
        )
        .unwrap();

        // Both runners begin against the same empty local database.
        // Serialized ledger checks under BEGIN IMMEDIATE must prevent duplicate DDL.
        let (left, right) = tokio::join!(
            apply_sqlite_migrations(&root, &db_path),
            apply_sqlite_migrations(&root, &db_path)
        );
        assert!(left.is_ok(), "first runner failed: {left:?}");
        assert!(right.is_ok(), "second runner failed: {right:?}");

        let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn
            .query(
                "SELECT version, file FROM __nulang_schema_migrations ORDER BY version",
                (),
            )
            .await
            .unwrap();
        let first = rows.next().await.unwrap().unwrap();
        let version: i64 = first.get(0).unwrap();
        assert_eq!(version, 1);
        assert!(rows.next().await.unwrap().is_none());
        drop(rows);
        drop(conn);
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn sqlite_runner_preserves_other_database_contents_across_migrations() {
        let (root, db_path) = fixture();
        let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE existing (id INTEGER PRIMARY KEY)", ())
            .await
            .unwrap();
        conn.execute("INSERT INTO existing(id) VALUES(7)", ())
            .await
            .unwrap();
        drop(conn);
        drop(db);

        let first = generate_migration(&root, "create_customers").unwrap();
        fs::write(
            &first,
            "CREATE TABLE customers (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        apply_sqlite_migrations(&root, &db_path).await.unwrap();

        let db = libsql::Builder::new_local(&db_path).build().await.unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn.query("SELECT id FROM existing", ()).await.unwrap();
        let id: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(id, 7);
        drop(rows);
        drop(conn);
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }

}
