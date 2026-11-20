//! One patched local SQLite provider for actor and AI persistence.
//! Remote Turso/Hrana uses libsql without its core/FFI feature, preserving
//! the HTTPS-only certificate-validating connector in persistence.rs.
//! The actor store serializes local queries and transactions with its Mutex.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use ::libsql::params::{IntoParams, Params};
pub use ::libsql::{params, params_from_iter, TransactionBehavior, Value};

type Result<T> = std::result::Result<T, String>;

pub struct Builder {
    path: PathBuf,
}

impl Builder {
    pub fn new_local<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub async fn build(self) -> Result<Database> {
        Ok(Database { path: self.path })
    }
}

pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn connect(&self) -> Result<Connection> {
        Ok(Connection::Local(
            rusqlite::Connection::open(&self.path).map_err(|e| e.to_string())?,
        ))
    }
}

pub enum Connection {
    Local(rusqlite::Connection),
    Remote(::libsql::Connection),
}

impl Connection {
    pub fn remote(connection: ::libsql::Connection) -> Self {
        Self::Remote(connection)
    }

    pub async fn execute<P: IntoParams>(&self, sql: &str, params: P) -> Result<u64> {
        let params = params.into_params().map_err(|e| e.to_string())?;
        match self {
            Self::Local(conn) => Ok(local_execute(conn, sql, params)? as u64),
            Self::Remote(conn) => conn.execute(sql, params).await.map_err(|e| e.to_string()),
        }
    }

    pub async fn query<P: IntoParams>(&self, sql: &str, params: P) -> Result<Rows> {
        let params = params.into_params().map_err(|e| e.to_string())?;
        match self {
            Self::Local(conn) => Ok(Rows::Local(local_query(conn, sql, params)?)),
            Self::Remote(conn) => Ok(Rows::Remote(
                conn.query(sql, params).await.map_err(|e| e.to_string())?,
            )),
        }
    }

    pub async fn transaction_with_behavior(
        &self,
        behavior: TransactionBehavior,
    ) -> Result<Transaction<'_>> {
        match self {
            Self::Local(conn) => {
                let mode = match behavior {
                    TransactionBehavior::Deferred => rusqlite::TransactionBehavior::Deferred,
                    TransactionBehavior::Immediate => rusqlite::TransactionBehavior::Immediate,
                    TransactionBehavior::Exclusive => rusqlite::TransactionBehavior::Exclusive,
                    TransactionBehavior::ReadOnly => {
                        return Err(
                            "local SQLite does not support read-only transaction mode".into()
                        );
                    }
                };
                let inner =
                    rusqlite::Transaction::new_unchecked(conn, mode).map_err(|e| e.to_string())?;
                Ok(Transaction::Local(inner))
            }
            Self::Remote(conn) => Ok(Transaction::Remote(
                conn.transaction_with_behavior(behavior)
                    .await
                    .map_err(|e| e.to_string())?,
            )),
        }
    }
}

pub enum Transaction<'conn> {
    Local(rusqlite::Transaction<'conn>),
    Remote(::libsql::Transaction),
}

impl Transaction<'_> {
    pub async fn execute<P: IntoParams>(&self, sql: &str, params: P) -> Result<u64> {
        let params = params.into_params().map_err(|e| e.to_string())?;
        match self {
            Self::Local(tx) => Ok(local_execute(tx, sql, params)? as u64),
            Self::Remote(tx) => tx.execute(sql, params).await.map_err(|e| e.to_string()),
        }
    }

    pub async fn query<P: IntoParams>(&self, sql: &str, params: P) -> Result<Rows> {
        let params = params.into_params().map_err(|e| e.to_string())?;
        match self {
            Self::Local(tx) => Ok(Rows::Local(local_query(tx, sql, params)?)),
            Self::Remote(tx) => Ok(Rows::Remote(
                tx.query(sql, params).await.map_err(|e| e.to_string())?,
            )),
        }
    }

    pub async fn commit(self) -> Result<()> {
        match self {
            Self::Local(tx) => tx.commit().map_err(|e| e.to_string()),
            Self::Remote(tx) => tx.commit().await.map_err(|e| e.to_string()),
        }
    }

    pub async fn rollback(self) -> Result<()> {
        match self {
            Self::Local(tx) => tx.rollback().map_err(|e| e.to_string()),
            Self::Remote(tx) => tx.rollback().await.map_err(|e| e.to_string()),
        }
    }
}

fn to_local(value: Value) -> rusqlite::types::Value {
    match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::Integer(n) => rusqlite::types::Value::Integer(n),
        Value::Real(n) => rusqlite::types::Value::Real(n),
        Value::Text(s) => rusqlite::types::Value::Text(s),
        Value::Blob(b) => rusqlite::types::Value::Blob(b),
    }
}

fn from_local(value: rusqlite::types::Value) -> Value {
    match value {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(n) => Value::Integer(n),
        rusqlite::types::Value::Real(n) => Value::Real(n),
        rusqlite::types::Value::Text(s) => Value::Text(s),
        rusqlite::types::Value::Blob(b) => Value::Blob(b),
    }
}

fn local_execute(conn: &rusqlite::Connection, sql: &str, params: Params) -> Result<usize> {
    match params {
        Params::None => conn.execute(sql, ()).map_err(|e| e.to_string()),
        Params::Positional(values) => {
            let values: Vec<_> = values.into_iter().map(to_local).collect();
            conn.execute(sql, rusqlite::params_from_iter(values.iter()))
                .map_err(|e| e.to_string())
        }
        Params::Named(values) => {
            let entries: Vec<_> = values
                .into_iter()
                .map(|(name, value)| (name, to_local(value)))
                .collect();
            let bindings: Vec<(&str, &dyn rusqlite::ToSql)> = entries
                .iter()
                .map(|(name, value)| (name.as_str(), value as &dyn rusqlite::ToSql))
                .collect();
            conn.execute(sql, bindings.as_slice())
                .map_err(|e| e.to_string())
        }
    }
}

fn local_query(conn: &rusqlite::Connection, sql: &str, params: Params) -> Result<VecDeque<Row>> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let width = stmt.column_count();
    let mut collected = VecDeque::new();

    match params {
        Params::None => collect_rows(&mut stmt, (), width, &mut collected)?,
        Params::Positional(values) => {
            let values: Vec<_> = values.into_iter().map(to_local).collect();
            collect_rows(
                &mut stmt,
                rusqlite::params_from_iter(values.iter()),
                width,
                &mut collected,
            )?;
        }
        Params::Named(values) => {
            let entries: Vec<_> = values
                .into_iter()
                .map(|(name, value)| (name, to_local(value)))
                .collect();
            let bindings: Vec<(&str, &dyn rusqlite::ToSql)> = entries
                .iter()
                .map(|(name, value)| (name.as_str(), value as &dyn rusqlite::ToSql))
                .collect();
            collect_rows(&mut stmt, bindings.as_slice(), width, &mut collected)?;
        }
    }
    Ok(collected)
}

// The compatibility adapter buffers local rows before returning them.
// Bound that buffer so a query cannot silently consume unbounded memory.
// These are safety budgets, not a claim of a strict process RSS ceiling:
// SQLite itself and downstream JSON serialization allocate separately.
const MAX_LOCAL_QUERY_ROWS: usize = 100_000;
const MAX_LOCAL_QUERY_BUFFERED_BYTES: usize = 64 * 1024 * 1024;

fn collect_rows<P: rusqlite::Params>(
    stmt: &mut rusqlite::Statement<'_>,
    params: P,
    width: usize,
    dest: &mut VecDeque<Row>,
) -> Result<()> {
    collect_rows_with_limits(
        stmt,
        params,
        width,
        dest,
        MAX_LOCAL_QUERY_ROWS,
        MAX_LOCAL_QUERY_BUFFERED_BYTES,
    )
}

fn collect_rows_with_limits<P: rusqlite::Params>(
    stmt: &mut rusqlite::Statement<'_>,
    params: P,
    width: usize,
    dest: &mut VecDeque<Row>,
    max_rows: usize,
    max_buffered_bytes: usize,
) -> Result<()> {
    let mut buffered_bytes = 0usize;
    let per_row_metadata = std::mem::size_of::<Row>()
        .saturating_add(width.saturating_mul(std::mem::size_of::<Value>()));
    let mut rows = stmt.query(params).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if dest.len() >= max_rows {
            return Err(format!(
                "local SQLite query exceeds {max_rows}-row budget; paginate the result"
            ));
        }
        buffered_bytes = buffered_bytes.saturating_add(per_row_metadata);
        if buffered_bytes > max_buffered_bytes {
            return Err(format!(
                "local SQLite query exceeds {max_buffered_bytes}-byte buffer budget; paginate the result"
            ));
        }
        let mut cells = Vec::with_capacity(width);
        for index in 0..width {
            // Inspect SQLite's borrowed cell before cloning text/blob bytes.
            let value = row.get_ref(index).map_err(|e| e.to_string())?;
            let payload_bytes = match value {
                rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => {
                    bytes.len()
                }
                _ => 0,
            };
            buffered_bytes = buffered_bytes.saturating_add(payload_bytes);
            if buffered_bytes > max_buffered_bytes {
                return Err(format!(
                    "local SQLite query exceeds {max_buffered_bytes}-byte buffer budget; paginate the result"
                ));
            }
            cells.push(
                row.get::<_, rusqlite::types::Value>(index)
                    .map(from_local)
                    .map_err(|e| e.to_string())?,
            );
        }
        dest.push_back(Row { cells });
    }
    Ok(())
}

pub enum Rows {
    Local(VecDeque<Row>),
    Remote(::libsql::Rows),
}

impl Rows {
    pub async fn next(&mut self) -> Result<Option<Row>> {
        match self {
            Self::Local(rows) => Ok(rows.pop_front()),
            Self::Remote(rows) => {
                let Some(row) = rows.next().await.map_err(|e| e.to_string())? else {
                    return Ok(None);
                };
                let cells = (0..row.column_count())
                    .map(|i| row.get_value(i).map_err(|e| e.to_string()))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Some(Row { cells }))
            }
        }
    }
}

pub struct Row {
    cells: Vec<Value>,
}

pub trait FromCell: Sized {
    fn from_cell(value: Value) -> Result<Self>;
}

impl FromCell for i64 {
    fn from_cell(value: Value) -> Result<Self> {
        match value {
            Value::Integer(n) => Ok(n),
            other => Err(format!("expected SQLite integer, got {other:?}")),
        }
    }
}

impl FromCell for String {
    fn from_cell(value: Value) -> Result<Self> {
        match value {
            Value::Text(s) => Ok(s),
            other => Err(format!("expected SQLite text, got {other:?}")),
        }
    }
}

impl<T: FromCell> FromCell for Option<T> {
    fn from_cell(value: Value) -> Result<Self> {
        match value {
            Value::Null => Ok(None),
            other => T::from_cell(other).map(Some),
        }
    }
}

impl Row {
    pub fn column_count(&self) -> usize {
        self.cells.len()
    }

    pub fn get_value(&self, index: usize) -> Result<Value> {
        self.cells
            .get(index)
            .cloned()
            .ok_or_else(|| format!("no SQLite column at {index}"))
    }

    pub fn get<T: FromCell>(&self, index: usize) -> Result<T> {
        T::from_cell(self.get_value(index)?)
    }
}

#[cfg(test)]
mod local_query_budget_tests {
    use super::*;

    #[test]
    fn rejects_unbounded_local_result_before_materializing_every_row() {
        let conn = rusqlite::Connection::open_in_memory().expect("fixture connection");
        let sql = "WITH RECURSIVE counter(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM counter WHERE n < 100001) SELECT n FROM counter";
        let result = local_query(&conn, sql, Params::None);
        assert!(
            result.is_err(),
            "local queries must fail closed once buffered results exceed the documented 100000-row budget"
        );
    }

    #[test]
    fn rejects_excess_text_bytes_before_cloning_into_buffer() {
        let conn = rusqlite::Connection::open_in_memory().expect("fixture connection");
        let mut stmt = conn.prepare("SELECT '0123456789'").expect("query");
        let width = stmt.column_count();
        let overhead = std::mem::size_of::<Row>() + std::mem::size_of::<Value>();
        let mut dest = VecDeque::new();
        let error = collect_rows_with_limits(&mut stmt, (), width, &mut dest, 10, overhead + 9)
            .expect_err("10 bytes of text must exceed a 9-byte payload allowance");
        assert!(error.contains("byte buffer budget"), "{error}");
        assert!(dest.is_empty(), "no oversized row may enter the buffer");
    }

    #[test]
    fn accepts_exact_byte_budget_without_truncating_text() {
        let conn = rusqlite::Connection::open_in_memory().expect("fixture connection");
        let mut stmt = conn.prepare("SELECT '0123456789'").expect("query");
        let width = stmt.column_count();
        let overhead = std::mem::size_of::<Row>() + std::mem::size_of::<Value>();
        let mut dest = VecDeque::new();
        collect_rows_with_limits(&mut stmt, (), width, &mut dest, 10, overhead + 10)
            .expect("exact budget is allowed");
        assert_eq!(dest.len(), 1);
        assert_eq!(
            dest.pop_front().unwrap().get::<String>(0).unwrap(),
            "0123456789"
        );
    }

    #[test]
    fn small_local_result_keeps_complete_ordered_rows() {
        let conn = rusqlite::Connection::open_in_memory().expect("fixture connection");
        let mut rows = local_query(
            &conn,
            "SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3",
            Params::None,
        )
        .expect("small results must succeed");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.pop_front().unwrap().get::<i64>(0).unwrap(), 1);
        assert_eq!(rows.pop_front().unwrap().get::<i64>(0).unwrap(), 2);
        assert_eq!(rows.pop_front().unwrap().get::<i64>(0).unwrap(), 3);
    }
}
