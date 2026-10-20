//! SQLite-only transactional application of reviewed SQL migrations.
//! No remote DB/Cloud support. No SQL runs outside explicit opt-in.
use std::path::Path;

// Implementation added after regression tests.
pub async fn apply_sqlite_migrations(
    root: &Path,
    database_path: &Path,
) -> crate::types::NuResult<crate::package::migrations::MigrationAudit> {
    let _ = (root, database_path);
    unimplemented!("implement database-side transactional migration ledger")
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
        let mut rows = conn.query(
            "SELECT version, file, checksum_blake3 FROM __nulang_schema_migrations ORDER BY version",
            (),
        ).await.unwrap();
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
        let mut tables = conn.query(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('customers', '__nulang_schema_migrations')",
            (),
        ).await.unwrap();
        assert!(tables.next().await.unwrap().is_none(), "failed batch must leave no schema or ledger");
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn changed_applied_sql_blocks_later_migrations_without_side_effects() {
        let (root, db_path) = fixture();
        let first = generate_migration(&root, "create_customers").unwrap();
        fs::write(&first, "CREATE TABLE customers (id INTEGER PRIMARY KEY);").unwrap();
        apply_sqlite_migrations(&root, &db_path).await.unwrap();

        fs::write(&first, "CREATE TABLE customers (id INTEGER PRIMARY KEY, edited TEXT);").unwrap();
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
}
