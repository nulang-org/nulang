//! Opt-in SQLite-backed key/value state for routed web requests.
//!
//! This is a separate namespaced table in the existing libSQL driver, not a
//! new durable actor/workflow engine. Writes only return Ok after SQLite has
//! acknowledged the operation. There is no silent memory fallback.
#![cfg(feature = "sqlite")]

use std::io;
use std::path::Path;
use libsql::Connection;

/// A single-process connection serialized by the owning web request dispatcher.
///
/// Callers share this object behind an Arc<Mutex<_>>. Every write is its own
/// SQLite transaction; a sequence of web effects is NOT a multi-effect
/// transaction. Durable actor transitions still use PersistenceStore.
pub struct SqliteWebStore {
    connection: Connection,
    runtime: tokio::runtime::Runtime,
}

impl SqliteWebStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_string_lossy().into_owned();
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let db = runtime.block_on(async {
            libsql::Builder::new_local(&path)
                .build()
                .await
                .map_err(sql_error)
        })?;
        let connection = db
            .connect()
            .map_err(sql_error)?;
        runtime.block_on(async {
            let mut mode = connection.query("PRAGMA journal_mode=WAL", ())
                .await.map_err(sql_error)?;
            while mode.next().await.map_err(sql_error)?.is_some() {}
            connection.execute("PRAGMA synchronous=FULL", ())
                .await.map_err(sql_error)?;
            connection.execute(
                "CREATE TABLE IF NOT EXISTS nulang_web_kv (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                )",
                (),
            ).await.map_err(sql_error)?;
            Ok::<(), io::Error>(())
        })?;
        Ok(Self { connection, runtime })
    }

    pub fn get(&self, key: &str) -> io::Result<Option<String>> {
        self.runtime.block_on(async {
            let mut rows = self.connection.query(
                "SELECT value FROM nulang_web_kv WHERE key = ?1",
                libsql::params![key],
            ).await.map_err(sql_error)?;
            match rows.next().await.map_err(sql_error)? {
                Some(row) => row.get::<String>(0).map(Some).map_err(sql_error),
                None => Ok(None),
            }
        })
    }

    pub fn set(&self, key: &str, value: &str) -> io::Result<()> {
        self.runtime.block_on(async {
            self.connection.execute(
                "INSERT INTO nulang_web_kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                libsql::params![key, value],
            ).await.map_err(sql_error)?;
            Ok(())
        })
    }

    pub fn delete(&self, key: &str) -> io::Result<()> {
        self.runtime.block_on(async {
            self.connection.execute(
                "DELETE FROM nulang_web_kv WHERE key = ?1",
                libsql::params![key],
            ).await.map_err(sql_error)?;
            Ok(())
        })
    }

    pub fn all(&self) -> io::Result<Vec<(String, String)>> {
        self.runtime.block_on(async {
            let mut rows = self.connection.query(
                "SELECT key, value FROM nulang_web_kv ORDER BY key",
                (),
            ).await.map_err(sql_error)?;
            let mut entries = Vec::new();
            while let Some(row) = rows.next().await.map_err(sql_error)? {
                entries.push((
                    row.get::<String>(0).map_err(sql_error)?,
                    row.get::<String>(1).map_err(sql_error)?,
                ));
            }
            Ok(entries)
        })
    }
}

fn sql_error(e: libsql::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}
