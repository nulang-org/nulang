//! PostgreSQL migration application using an existing, caller-owned Client.
//! This module never creates connections, selects TLS policy, or auto-migrates.

use std::fs;
use std::io::Read;
use std::path::Path;

use crate::package::migrations::{read_migration_plan, MigrationAudit, SqlMigration};
use crate::types::{NuError, NuResult, Span};

// A transaction-scoped advisory lock serializes Nulang migration writers
// within a database. The ledger remains the authoritative record of progress.
const LOCK_ID: i64 = 0x4e55_4c41_4e47_534d;
const LEDGER: &str = "public.__nulang_schema_migrations";
const CREATE_LEDGER: &str = "CREATE TABLE IF NOT EXISTS public.__nulang_schema_migrations (\
    version BIGINT PRIMARY KEY CHECK (version > 0), \
    file TEXT NOT NULL, \
    checksum_blake3 TEXT NOT NULL, \
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()\
)";

fn error(message: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: message.into(),
        span: Span::default(),
    }
}

fn database_error(action: &str, problem: impl std::fmt::Display) -> NuError {
    error(format!("PostgreSQL {action}: {problem}"))
}

fn validate_source_bytes(record: &SqlMigration, bytes: &[u8]) -> NuResult<()> {
    if bytes.len() > 1024 * 1024 {
        return Err(error(format!(
            "{} exceeds the 1 MiB PostgreSQL DDL limit",
            record.file
        )));
    }
    if blake3::hash(bytes).to_hex().to_string() != record.checksum_blake3 {
        return Err(error(format!(
            "SQL migration checksum changed before execution: {}",
            record.file
        )));
    }
    Ok(())
}

/// A deliberately strict first-pass DDL admission policy: exactly one DDL
/// statement. PostgreSQL's prepared execution path also rejects multiple
/// statements, enforcing the limit at both the parser and database boundary.
fn single_postgres_ddl(bytes: &[u8], file: &str) -> NuResult<String> {
    let source = std::str::from_utf8(bytes)
        .map_err(|_| error(format!("{file} must contain UTF-8 SQL")))?;
    let meaningful = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = meaningful.trim();
    let ddl = trimmed.strip_suffix(';').unwrap_or(trimmed).trim();
    let lower = ddl.to_ascii_lowercase();
    if ddl.is_empty()
        || ddl.contains(';')
        || ddl.contains("--")
        || ddl.contains("/*")
        || ddl.contains("*/")
        || lower.contains("__nulang_schema_migrations")
        || lower.contains("concurrently")
    {
        return Err(error(format!(
            "{file} must contain a single supported DDL statement without \
             transaction control, comments, or reserved ledger references"
        )));
    }

    let upper = ddl.to_ascii_uppercase();
    let mut words = upper.split_ascii_whitespace();
    let permitted = match (words.next(), words.next(), words.next()) {
        (Some("CREATE"), Some("TABLE"), _) => true,
        (Some("CREATE"), Some("INDEX"), _) => true,
        (Some("CREATE"), Some("UNIQUE"), Some("INDEX")) => true,
        (Some("ALTER"), Some("TABLE"), _) => true,
        _ => false,
    };
    if !permitted {
        return Err(error(format!("{file} must contain CREATE TABLE/INDEX or ALTER TABLE")));
    }
    Ok(ddl.to_owned())
}

fn preflight(root: &Path, plan: &[SqlMigration]) -> NuResult<Vec<String>> {
    let mut total = 0usize;
    let mut statements = Vec::with_capacity(plan.len());
    for migration in plan {
        let path = root.join("db/migrations").join(&migration.file);
        let meta = fs::symlink_metadata(&path)
            .map_err(|err| error(format!("cannot inspect {}: {err}", path.display())))?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(error(format!("{} must be a regular SQL file", path.display())));
        }
        if meta.len() > 1024 * 1024 {
            return Err(error(format!("{} exceeds the 1 MiB DDL limit", path.display())));
        }
        let mut bytes = Vec::new();
        fs::File::open(&path)
            .map_err(|err| error(format!("cannot open {}: {err}", path.display())))?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|err| error(format!("cannot read {}: {err}", path.display())))?;
        total = total.saturating_add(bytes.len());
        if total > 16 * 1024 * 1024 {
            return Err(error("SQL migration plan exceeds 16 MiB total"));
        }
        validate_source_bytes(migration, &bytes)?;
        statements.push(single_postgres_ddl(&bytes, &migration.file)?);
    }
    Ok(statements)
}

/// Apply a reviewed local migration plan to the database already connected
/// through a caller-supplied, appropriately authenticated PostgreSQL client.
///
/// We do not establish a network connection or decide TLS/credential policy.
/// All applied-version checks, DDL, and ledger inserts occur inside one SQL
/// transaction after a transaction-scoped advisory lock. Only explicit callers
/// invoke this function: nula build/deploy must never call it automatically.
pub fn apply_postgres_migrations(
    root: &Path,
    client: &mut postgres::Client,
) -> NuResult<MigrationAudit> {
    let plan = read_migration_plan(root)?;
    let statements = preflight(root, &plan)?;
    if plan.is_empty() {
        return Ok(MigrationAudit {
            applied: 0,
            pending: Vec::new(),
        });
    }

    let mut tx = client
        .transaction()
        .map_err(|err| database_error("begin transaction failed", err))?;
    let attempt = (|| -> NuResult<MigrationAudit> {
        tx.batch_execute(
            "SET LOCAL lock_timeout = '5s'; \
             SET LOCAL statement_timeout = '30s';",
        )
        .map_err(|err| database_error("timeout configuration failed", err))?;
        tx.query_one("SELECT pg_advisory_xact_lock($1)", &[&LOCK_ID])
            .map_err(|err| database_error("acquiring migration lock failed", err))?;

        tx.batch_execute(CREATE_LEDGER)
            .map_err(|err| database_error("ledger initialization failed", err))?;
        let rows = tx
            .query(
                "SELECT version, file, checksum_blake3 \
                 FROM public.__nulang_schema_migrations ORDER BY version",
                &[],
            )
            .map_err(|err| database_error("ledger query failed", err))?;

        for (index, row) in rows.iter().enumerate() {
            let expected = plan.get(index).ok_or_else(|| {
                error("PostgreSQL ledger contains an applied version absent from source")
            })?;
            let version: i64 = row
                .try_get(0)
                .map_err(|err| database_error("invalid ledger version", err))?;
            let file: String = row
                .try_get(1)
                .map_err(|err| database_error("invalid ledger filename", err))?;
            let checksum: String = row
                .try_get(2)
                .map_err(|err| database_error("invalid ledger checksum", err))?;
            if version != expected.version as i64 {
                return Err(error(format!(
                    "PostgreSQL ledger version gap/out-of-order at {:06}",
                    expected.version
                )));
            }
            if file != expected.file {
                return Err(error(format!(
                    "PostgreSQL ledger filename mismatch at version {:06}",
                    expected.version
                )));
            }
            if checksum != expected.checksum_blake3 {
                return Err(error(format!(
                    "PostgreSQL ledger checksum mismatch at version {:06}",
                    expected.version
                )));
            }
        }

        for (migration, ddl) in plan.iter().zip(statements.iter()).skip(rows.len()) {
            // Extended-protocol prepared execution enforces one statement.
            tx.execute(ddl, &[])
                .map_err(|err| database_error(&format!("applying {}", migration.file), err))?;
            let version = migration.version as i64;
            tx.execute(
                "INSERT INTO public.__nulang_schema_migrations \
                 (version, file, checksum_blake3) VALUES ($1, $2, $3)",
                &[&version, &migration.file, &migration.checksum_blake3],
            )
            .map_err(|err| database_error(&format!("recording {}", migration.file), err))?;
        }
        Ok(MigrationAudit {
            applied: plan.len(),
            pending: Vec::new(),
        })
    })();

    match attempt {
        Ok(outcome) => {
            tx.commit()
                .map_err(|err| database_error("commit failed", err))?;
            Ok(outcome)
        }
        Err(problem) => {
            tx.rollback()
                .map_err(|err| database_error("rollback failed", err))?;
            Err(problem)
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_one_reviewed_postgres_ddl_statement() {
        assert!(single_postgres_ddl(
            b"CREATE TABLE customers (id BIGSERIAL PRIMARY KEY);",
            "000001_create_customers.sql",
        ).is_ok());
        assert!(single_postgres_ddl(
            b"-- Safe leading comment\nALTER TABLE customers ADD COLUMN name TEXT;",
            "000002_add_name.sql",
        ).is_ok());
        assert!(single_postgres_ddl(
            b"CREATE UNIQUE INDEX customers_id_idx ON customers(id);",
            "000003_add_index.sql",
        ).is_ok());
    }

    #[test]
    fn rejects_multi_statement_scripts_and_tx_control() {
        for sql in [
            "CREATE TABLE customers (id BIGINT); DROP TABLE orders;",
            "BEGIN",
            "COMMIT",
            "CREATE INDEX CONCURRENTLY idx ON customers(id)",
            "SELECT 1",
        ] {
            assert!(
                single_postgres_ddl(sql.as_bytes(), "000001_unsafe.sql").is_err(),
                "should reject: {sql}"
            );
        }
    }

    #[test]
    fn rejects_ledger_mutation_and_obfuscated_extra_sql() {
        for sql in [
            "ALTER TABLE public.__nulang_schema_migrations ADD COLUMN attacker TEXT",
            "CREATE TABLE example (id BIGINT) /* hidden */",
            "CREATE TABLE example (id BIGINT); -- hidden",
            "CREATE TABLE example (id BIGINT);\\nDROP TABLE other",
        ] {
            assert!(
                single_postgres_ddl(sql.as_bytes(), "000001_unsafe.sql").is_err(),
                "should reject: {sql}"
            );
        }
    }

    #[test]
    fn validates_exact_source_checksums_before_database_use() {
        let bytes = b"CREATE TABLE customers (id BIGINT PRIMARY KEY);";
        let record = crate::package::migrations::SqlMigration {
            version: 1,
            file: "000001_customers.sql".to_string(),
            checksum_blake3: blake3::hash(bytes).to_hex().to_string(),
        };
        assert!(validate_source_bytes(&record, bytes).is_ok());
        assert!(validate_source_bytes(&record, b"CREATE TABLE customers(id TEXT);").is_err());
    }
}
