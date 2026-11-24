//! Native-host SQLite key/value storage for web applications.
//!
//! This is an opt-in storage primitive, not an ambient Web effect. The caller
//! must first authorize the tenant: supplying a tenant ID is not proof of
//! identity. The default Web.kv_* effect remains deliberately in-memory.
//!
//! Every statement is parameterized and scoped by tenant_id. Writes commit as
//! individual SQLite transactions; cross-key atomic operations are not exposed.
//! Built behind the existing optional "sqlite" Cargo feature.

use std::io;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

const MAX_VALUE_BYTES: usize = 1024 * 1024;
const MAX_KEY_BYTES: usize = 1024;
const MAX_TENANT_BYTES: usize = 256;
const MAX_LIST_ROWS: usize = 1000;

/// A file-backed, tenant-partitioned KV store usable by a trusted native host.
///
/// Not an authentication implementation. A client-controlled tenant identifier
/// must *never* be passed here without an independent authorization check.
pub struct WebSqliteStore {
    conn: Mutex<libsql::Connection>,
    runtime: tokio::runtime::Runtime,
}

impl WebSqliteStore {
    /// Open a local durable database and create its private v1 schema.
    ///
    /// Full synchronous journaling is selected intentionally: writes report
    /// success only after SQLite commits them. No memory-only path is accepted.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            return Err(invalid("WebSqliteStore requires a file-backed database"));
        }
        let db_path = path.to_str().ok_or_else(|| invalid("database path must be UTF-8"))?;
        // Reject SQLite URI forms, including file::memory:?cache=shared.
        // A durable host store must never accidentally open ephemeral storage.
        if db_path.starts_with("file:") {
            return Err(invalid("SQLite URI paths are not supported by durable web storage"));
        }
        let runtime = tokio::runtime::Runtime::new().map_err(sql_error)?;
        let database = runtime.block_on(async {
            libsql::Builder::new_local(db_path)
                .build()
                .await
                .map_err(sql_error)
        })?;
        let conn = database.connect().map_err(sql_error)?;
        let store = Self {
            conn: Mutex::new(conn),
            runtime,
        };
        store.init()?;
        Ok(store)
    }

    fn lock(&self) -> io::Result<MutexGuard<'_, libsql::Connection>> {
        self.conn.lock().map_err(|_| io::Error::other("web SQLite connection lock poisoned"))
    }

    fn init(&self) -> io::Result<()> {
        let conn = self.lock()?;
        self.runtime.block_on(async {
            // Drain both PRAGMAs on the same connection used for writes.
            for pragma in ["PRAGMA journal_mode=WAL", "PRAGMA synchronous=FULL"] {
                let mut rows = conn.query(pragma, ()).await.map_err(sql_error)?;
                while rows.next().await.map_err(sql_error)?.is_some() {}
            }
            conn.execute(
                "CREATE TABLE IF NOT EXISTS web_kv_v1 (
                    tenant_id TEXT NOT NULL,
                    key TEXT NOT NULL,
                    value TEXT NOT NULL,
                    PRIMARY KEY(tenant_id, key)
                ) WITHOUT ROWID",
                (),
            ).await.map_err(sql_error)?;
            Ok(())
        })
    }

    /// Read a key in the caller-authorized tenant scope.
    pub fn get(&self, tenant_id: &str, key: &str) -> io::Result<Option<String>> {
        validate_tenant(tenant_id)?;
        validate_key(key)?;
        let conn = self.lock()?;
        self.runtime.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT value FROM web_kv_v1 WHERE tenant_id = ?1 AND key = ?2",
                    libsql::params![tenant_id, key],
                )
                .await
                .map_err(sql_error)?;
            match rows.next().await.map_err(sql_error)? {
                Some(row) => row.get(0).map(Some).map_err(sql_error),
                None => Ok(None),
            }
        })
    }

    /// Create or update exactly one key in its tenant scope.
    pub fn put(&self, tenant_id: &str, key: &str, value: &str) -> io::Result<()> {
        validate_tenant(tenant_id)?;
        validate_key(key)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(invalid("web SQLite value exceeds the 1 MiB limit"));
        }
        let conn = self.lock()?;
        self.runtime.block_on(async {
            conn.execute(
                "INSERT INTO web_kv_v1 (tenant_id, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(tenant_id, key) DO UPDATE SET value = excluded.value",
                libsql::params![tenant_id, key, value],
            ).await.map_err(sql_error)?;
            Ok(())
        })
    }

    /// Delete exactly one key; returns false if it did not exist.
    pub fn delete(&self, tenant_id: &str, key: &str) -> io::Result<bool> {
        validate_tenant(tenant_id)?;
        validate_key(key)?;
        let conn = self.lock()?;
        self.runtime.block_on(async {
            let deleted = conn
                .execute(
                    "DELETE FROM web_kv_v1 WHERE tenant_id = ?1 AND key = ?2",
                    libsql::params![tenant_id, key],
                )
                .await
                .map_err(sql_error)?;
            Ok(deleted > 0)
        })
    }

    /// Return at most limit entries in stable key order for one tenant.
    ///
    /// Explicit bounds prevent an accidental unbounded tenant-wide export.
    pub fn list(&self, tenant_id: &str, limit: usize) -> io::Result<Vec<(String, String)>> {
        validate_tenant(tenant_id)?;
        if !(1..=MAX_LIST_ROWS).contains(&limit) {
            return Err(invalid("web SQLite list limit must be between 1 and 1000"));
        }
        let conn = self.lock()?;
        self.runtime.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT key, value FROM web_kv_v1 WHERE tenant_id = ?1 ORDER BY key LIMIT ?2",
                    libsql::params![tenant_id, limit as i64],
                )
                .await
                .map_err(sql_error)?;
            let mut entries = Vec::new();
            while let Some(row) = rows.next().await.map_err(sql_error)? {
                entries.push((row.get(0).map_err(sql_error)?, row.get(1).map_err(sql_error)?));
            }
            Ok(entries)
        })
    }
}

fn validate_tenant(tenant_id: &str) -> io::Result<()> {
    if tenant_id.trim().is_empty()
        || tenant_id.len() > MAX_TENANT_BYTES
        || tenant_id.chars().any(char::is_control)
    {
        return Err(invalid("nonempty bounded tenant ID required"));
    }
    Ok(())
}

fn validate_key(key: &str) -> io::Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(invalid("nonempty web SQLite key of at most 1024 bytes required"));
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn sql_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}
