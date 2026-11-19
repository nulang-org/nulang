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

fn collect_rows<P: rusqlite::Params>(
    stmt: &mut rusqlite::Statement<'_>,
    params: P,
    width: usize,
    dest: &mut VecDeque<Row>,
) -> Result<()> {
    let mut rows = stmt.query(params).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let cells = (0..width)
            .map(|index| row.get::<_, rusqlite::types::Value>(index).map(from_local))
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?;
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
