//! PostgreSQL migration application using an existing, caller-owned Client.
//! This module never creates connections, selects TLS policy, or auto-migrates.

use std::path::Path;
use crate::package::migrations::MigrationAudit;
use crate::types::NuResult;

pub fn apply_postgres_migrations(
    root: &Path,
    client: &mut postgres::Client,
) -> NuResult<MigrationAudit> {
    let _ = (root, client);
    unimplemented!("transactionally apply checked PostgreSQL SQL and ledger entries")
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
