//! Opt-in, read-only SQLite executor for the experimental relational Query IR.
//!
//! Uses Nulang's existing sqlite feature (libsql). This is a local reference
//! adapter, not a new SQL effect, transaction boundary or authorization layer.
//! Callers own the connection, credentials, tenancy, isolation and lifecycle.
//!
//! The executor validates the registered schema against a live SQLite table,
//! binds all literal values, bounds result materialization, and rejects SQL
//! NULL or dynamic storage values inconsistent with the closed Query IR types.

use std::collections::BTreeMap;

use crate::query::{infer_schema, Catalog, Column, DataType, Plan, QueryError, Row, Scalar};
use crate::query_sql::{compile_prepared, SqlDialect};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryLimits {
    /// Maximum rows materialized (checked before decoding the next row).
    pub max_rows: usize,
    /// Maximum aggregate UTF-8 bytes in the returned Text values.
    pub max_text_bytes: usize,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_rows: 1_000,
            max_text_bytes: 1_048_576,
        }
    }
}

fn sqlite_error(context: &str, err: impl std::fmt::Display) -> QueryError {
    QueryError(format!("SQLite {context}: {err}"))
}

fn declared_type(declaration: &str) -> Option<DataType> {
    let upper = declaration.trim().to_ascii_uppercase();
    if upper == "BOOLEAN" || upper == "BOOL" {
        Some(DataType::Bool)
    } else if upper.contains("INT") {
        Some(DataType::Int)
    } else if upper.contains("CHAR") || upper.contains("TEXT") || upper.contains("CLOB") {
        Some(DataType::Text)
    } else {
        None
    }
}

fn sql_value(value: &Scalar) -> libsql::Value {
    match value {
        Scalar::Int(n) => libsql::Value::Integer(*n),
        Scalar::Bool(b) => libsql::Value::Integer(i64::from(*b)),
        Scalar::Text(s) => libsql::Value::Text(s.clone()),
    }
}

fn typed_value(
    value: libsql::Value,
    column: &Column,
    text_bytes: &mut usize,
    limits: QueryLimits,
) -> Result<Scalar, QueryError> {
    match (column.ty, value) {
        (DataType::Int, libsql::Value::Integer(n)) => {
            const MIN: i64 = -(1_i64 << 47);
            const MAX: i64 = (1_i64 << 47) - 1;
            if !(MIN..=MAX).contains(&n) {
                return Err(QueryError(format!(
                    "column '{}' SQLite integer exceeds Nulang Int48",
                    column.name
                )));
            }
            Ok(Scalar::Int(n))
        }
        (DataType::Bool, libsql::Value::Integer(0)) => Ok(Scalar::Bool(false)),
        (DataType::Bool, libsql::Value::Integer(1)) => Ok(Scalar::Bool(true)),
        (DataType::Text, libsql::Value::Text(s)) => {
            *text_bytes = text_bytes
                .checked_add(s.len())
                .ok_or_else(|| QueryError("SQLite text budget overflow".into()))?;
            if *text_bytes > limits.max_text_bytes {
                return Err(QueryError("SQLite result text budget exceeded".into()));
            }
            Ok(Scalar::Text(s))
        }
        (_, libsql::Value::Null) => Err(QueryError(format!(
            "unexpected SQL NULL in column '{}' (Query IR has no NULL type)",
            column.name
        ))),
        _ => Err(QueryError(format!(
            "SQLite column '{}' does not contain a valid {:?} value",
            column.name, column.ty
        ))),
    }
}

/// Confirm that the named main-database source is a real SQLite table whose
/// column names and declared types exactly match the registered query schema.
/// This is deliberately stricter than SQLite's dynamic type affinity.
async fn verify_source_schema(
    conn: &libsql::Connection,
    source: &str,
    expected: &[Column],
) -> Result<(), QueryError> {
    let mut source_rows = conn
        .query(
            "SELECT type FROM sqlite_master WHERE name = ? AND type = 'table'",
            libsql::params![source],
        )
        .await
        .map_err(|e| sqlite_error("table metadata lookup", e))?;
    if source_rows
        .next()
        .await
        .map_err(|e| sqlite_error("table lookup", e))?
        .is_none()
    {
        return Err(QueryError(format!("SQLite table '{source}' is missing")));
    }
    let mut declared = conn
        .query(
            "SELECT name, type, \"notnull\", hidden FROM pragma_table_xinfo(?)",
            libsql::params![source],
        )
        .await
        .map_err(|e| sqlite_error("schema lookup", e))?;
    let mut actual = BTreeMap::new();
    while let Some(row) = declared
        .next()
        .await
        .map_err(|e| sqlite_error("schema rows", e))?
    {
        let name = row
            .get::<String>(0)
            .map_err(|e| sqlite_error("column name", e))?;
        let ty = row
            .get::<String>(1)
            .map_err(|e| sqlite_error("column type", e))?;
        let not_null = row
            .get::<i64>(2)
            .map_err(|e| sqlite_error("nullability", e))?;
        let hidden = row
            .get::<i64>(3)
            .map_err(|e| sqlite_error("hidden column", e))?;
        if hidden != 0 {
            return Err(QueryError(format!(
                "SQLite hidden/generated schema column '{name}' unsupported"
            )));
        }
        // Explicit NOT NULL is required: the pragma's pk flag alone cannot
        // prove it (SQLite permits NULL for INTEGER PRIMARY KEY DESC).
        // Otherwise a WHERE filter may silently discard a NULL row before
        // the result decoder has a chance to reject it.
        if not_null == 0 {
            return Err(QueryError(format!(
                "SQLite schema column '{name}' is nullable; Query IR has no SQL NULL semantics"
            )));
        }
        let mapped = declared_type(&ty).ok_or_else(|| {
            QueryError(format!(
                "SQLite schema column '{name}' has unsupported type '{ty}'"
            ))
        })?;
        if actual.insert(name, mapped).is_some() {
            return Err(QueryError("duplicate SQLite schema column".into()));
        }
    }
    let expected: BTreeMap<_, _> = expected
        .iter()
        .map(|col| (col.name.clone(), col.ty))
        .collect();
    if actual != expected {
        return Err(QueryError(format!(
            "SQLite schema drift: expected {:?}, actual {:?}",
            expected, actual
        )));
    }
    Ok(())
}

/// Execute a validated read-only Query IR plan using a caller-owned connection.
///
/// Does not expose arbitrary SQL execution, mutate storage, grant access to
/// tables, create an explicit transaction, or guarantee a query timeout.
/// A caller must supply a connection scoped to their authorization context.
/// Result limits protect materialization size, not SQLite CPU/scan cost.
pub async fn execute_sqlite(
    conn: &libsql::Connection,
    plan: &Plan,
    catalog: &Catalog,
    limits: QueryLimits,
) -> Result<Vec<Row>, QueryError> {
    let prepared = compile_prepared(plan, catalog, SqlDialect::Sqlite)?;

    // Find the exact source, without using free-form SQL input.
    let mut source = plan;
    let table = loop {
        match source {
            Plan::Scan { table } => break table,
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. } => source = input,
        }
    };
    let schema = infer_schema(&Plan::scan(table), catalog)?;
    verify_source_schema(conn, table, &schema).await?;

    let parameters = prepared
        .parameters
        .iter()
        .map(sql_value)
        .collect::<Vec<_>>();
    let mut rows = conn
        .query(&prepared.sql, libsql::params_from_iter(parameters))
        .await
        .map_err(|e| sqlite_error("query", e))?;
    let actual_count = usize::try_from(rows.column_count())
        .map_err(|_| QueryError("negative SQLite column count".into()))?;
    if actual_count != prepared.output_columns.len() {
        return Err(QueryError(
            "SQLite result column count does not match query schema".into(),
        ));
    }
    for (index, expected) in prepared.output_columns.iter().enumerate() {
        let name = rows
            .column_name(index as i32)
            .ok_or_else(|| QueryError(format!("SQLite result has no name at column {index}")))?;
        if name != expected.name {
            return Err(QueryError(format!(
                "SQLite result column mismatch at {index}: expected '{}', got '{name}'",
                expected.name
            )));
        }
    }

    let mut result = Vec::new();
    let mut text_bytes = 0usize;
    while let Some(record) = rows.next().await.map_err(|e| sqlite_error("next row", e))? {
        if result.len() >= limits.max_rows {
            return Err(QueryError("SQLite result row limit exceeded".into()));
        }
        let mut result_row = Row::new();
        for (index, column) in prepared.output_columns.iter().enumerate() {
            let raw = record
                .get_value(index as i32)
                .map_err(|e| sqlite_error("row column", e))?;
            let decoded = typed_value(raw, column, &mut text_bytes, limits)?;
            result_row.insert(column.name.clone(), decoded);
        }
        result.push(result_row);
    }
    Ok(result)
}
