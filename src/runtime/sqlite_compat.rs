//! Local SQLite compatibility adapter for the historic LibsqlStore interface.
//!
//! Nulang's actor persistence keeps its existing libSQL-shaped operations
//! while both the actor and AI stores share ONE patched rusqlite/SQLite
//! provider. This adapter is deliberately local-only: it never opens remote
//! connections or enables libSQL's legacy TLS transport.
//!
//! All SQL operations below are synchronous inside immediately-ready futures.
//! The caller already serializes access with a Mutex, so the SQLite transaction
//! is held for the full atomic transition. We buffer owned result rows to
//! avoid exposing statement/connection lifetimes across async boundaries.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

pub use rusqlite::types::Value;
pub use rusqlite::{params, params_from_iter, TransactionBehavior};

pub struct Builder {
    path: PathBuf,
}

impl Builder {
    pub fn new_local<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub async fn build(self) -> rusqlite::Result<Database> {
        Ok(Database { path: self.path })
    }
}

pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn connect(&self) -> rusqlite::Result<Connection> {
        Ok(Connection {
            inner: rusqlite::Connection::open(&self.path)?,
        })
    }
}

pub struct Connection {
    inner: rusqlite::Connection,
}

impl Connection {
    pub async fn execute<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> rusqlite::Result<usize> {
        self.inner.execute(sql, params)
    }

    pub async fn query<P: rusqlite::Params>(&self, sql: &str, params: P) -> rusqlite::Result<Rows> {
        collect_rows(&self.inner, sql, params)
    }

    pub async fn transaction_with_behavior(
        &self,
        behavior: TransactionBehavior,
    ) -> rusqlite::Result<Transaction<'_>> {
        // Mutex in LibsqlStore gives this transaction exclusive write access.
        let inner = rusqlite::Transaction::new_unchecked(&self.inner, behavior)?;
        Ok(Transaction { inner })
    }
}

pub struct Transaction<'conn> {
    inner: rusqlite::Transaction<'conn>,
}

impl Transaction<'_> {
    pub async fn execute<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> rusqlite::Result<usize> {
        self.inner.execute(sql, params)
    }

    pub async fn query<P: rusqlite::Params>(&self, sql: &str, params: P) -> rusqlite::Result<Rows> {
        collect_rows(&self.inner, sql, params)
    }

    pub async fn commit(self) -> rusqlite::Result<()> {
        self.inner.commit()
    }

    pub async fn rollback(self) -> rusqlite::Result<()> {
        self.inner.rollback()
    }
}

fn collect_rows<P: rusqlite::Params>(
    conn: &rusqlite::Connection,
    sql: &str,
    params: P,
) -> rusqlite::Result<Rows> {
    let mut stmt = conn.prepare(sql)?;
    let width = stmt.column_count();
    let mut source = stmt.query(params)?;
    let mut result = VecDeque::new();

    while let Some(row) = source.next()? {
        let cells = (0..width)
            .map(|index| row.get::<usize, Value>(index))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        result.push_back(Row { cells });
    }
    Ok(Rows { rows: result })
}

pub struct Rows {
    rows: VecDeque<Row>,
}

impl Rows {
    pub async fn next(&mut self) -> rusqlite::Result<Option<Row>> {
        Ok(self.rows.pop_front())
    }
}

pub struct Row {
    cells: Vec<Value>,
}

impl Row {
    pub fn column_count(&self) -> usize {
        self.cells.len()
    }

    pub fn get<T: rusqlite::types::FromSql>(&self, index: usize) -> rusqlite::Result<T> {
        let value = self
            .cells
            .get(index)
            .ok_or(rusqlite::Error::InvalidColumnIndex(index))?;
        let borrowed = match value {
            Value::Null => rusqlite::types::ValueRef::Null,
            Value::Integer(n) => rusqlite::types::ValueRef::Integer(*n),
            Value::Real(f) => rusqlite::types::ValueRef::Real(*f),
            Value::Text(text) => rusqlite::types::ValueRef::Text(text.as_bytes()),
            Value::Blob(bytes) => rusqlite::types::ValueRef::Blob(bytes),
        };
        T::column_result(borrowed).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(index, value.data_type(), Box::new(error))
        })
    }

    pub fn get_value(&self, index: usize) -> rusqlite::Result<Value> {
        self.cells
            .get(index)
            .cloned()
            .ok_or(rusqlite::Error::InvalidColumnIndex(index))
    }
}
