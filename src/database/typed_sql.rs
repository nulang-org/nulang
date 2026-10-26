//! Experimental typed, bounded read plans from relational schema contracts.
//! This is Rust tooling, not compiler-integrated Nulang syntax.
//! Values are never interpolated into SQL. No database calls are made.

use std::collections::BTreeSet;
use std::fmt;

use super::schema_contract::{
    RelationalSchema, SchemaColumn, SchemaDialect, SchemaTable,
};

pub const MAX_SELECT_LIMIT: u32 = 1000;

#[derive(Debug, Clone, PartialEq)]
pub enum SqlParameter {
    Text(String),
    Integer(i64),
    Boolean(bool),
    Real(f64),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SqlPredicate {
    Eq { column: String, value: SqlParameter },
    IsNull { column: String },
    IsNotNull { column: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectSpec {
    pub table: String,
    pub columns: Vec<String>,
    pub predicate: Option<SqlPredicate>,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedColumn {
    pub name: String,
    pub sql_type: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSelect {
    pub sql: String,
    pub parameters: Vec<SqlParameter>,
    pub projection: Vec<ProjectedColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedSqlError(pub String);

impl fmt::Display for TypedSqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for TypedSqlError {}

/// Prepare one SELECT with explicit columns, one optional predicate and a
/// mandatory bounded LIMIT. Fails closed on unknown schema, columns or types.
pub fn prepare_select(
    schema: &RelationalSchema,
    spec: &SelectSpec,
) -> Result<PreparedSelect, TypedSqlError> {
    schema.validate().map_err(|err| fail(err.to_string()))?;
    if spec.limit == 0 || spec.limit > MAX_SELECT_LIMIT {
        return Err(fail(format!("SELECT LIMIT must be 1..={MAX_SELECT_LIMIT}")));
    }
    if spec.columns.is_empty() {
        return Err(fail("SELECT needs an explicit projection"));
    }
    let table = find_table(schema, &spec.table)
        .ok_or_else(|| fail(format!("unknown table '{}'", spec.table)))?;

    let mut projection = Vec::new();
    let mut columns_sql = Vec::new();
    let mut seen = BTreeSet::new();
    for name in &spec.columns {
        let column = find_column(schema.dialect, table, name)
            .ok_or_else(|| fail(format!("unknown selected column '{name}'")))?;
        let key = match schema.dialect {
            SchemaDialect::Postgres => column.name.clone(),
            SchemaDialect::Sqlite => column.name.to_ascii_lowercase(),
        };
        if !seen.insert(key) {
            return Err(fail(format!("duplicate selected column '{name}'")));
        }
        columns_sql.push(quote_identifier(&column.name)?);
        projection.push(ProjectedColumn {
            name: column.name.clone(),
            sql_type: column.sql_type.clone(),
            nullable: column.nullable,
        });
    }

    let mut sql = format!(
        "SELECT {} FROM {}",
        columns_sql.join(", "),
        quote_table(&table.name)?
    );
    let mut parameters = Vec::new();
    if let Some(predicate) = &spec.predicate {
        match predicate {
            SqlPredicate::Eq { column, value } => {
                let field = find_column(schema.dialect, table, column)
                    .ok_or_else(|| fail(format!("unknown filter column '{column}'")))?;
                verify_value(schema.dialect, field, value)?;
                sql.push_str(&format!(
                    " WHERE {} = {}",
                    quote_identifier(&field.name)?,
                    placeholder(schema.dialect, 1)
                ));
                parameters.push(value.clone());
            }
            SqlPredicate::IsNull { column } | SqlPredicate::IsNotNull { column } => {
                let field = find_column(schema.dialect, table, column)
                    .ok_or_else(|| fail(format!("unknown filter column '{column}'")))?;
                let op = if matches!(predicate, SqlPredicate::IsNull { .. }) {
                    "IS NULL"
                } else {
                    "IS NOT NULL"
                };
                sql.push_str(&format!(
                    " WHERE {} {op}",
                    quote_identifier(&field.name)?
                ));
            }
        }
    }
    sql.push_str(&format!(
        " LIMIT {}",
        placeholder(schema.dialect, parameters.len() + 1)
    ));
    parameters.push(SqlParameter::Integer(i64::from(spec.limit)));
    Ok(PreparedSelect { sql, parameters, projection })
}

fn find_table<'a>(schema: &'a RelationalSchema, name: &str) -> Option<&'a SchemaTable> {
    schema.tables.iter().find(|t| names_match(schema.dialect, &t.name, name))
}

fn find_column<'a>(
    dialect: SchemaDialect,
    table: &'a SchemaTable,
    name: &str,
) -> Option<&'a SchemaColumn> {
    table.columns.iter().find(|c| names_match(dialect, &c.name, name))
}

fn names_match(dialect: SchemaDialect, actual: &str, requested: &str) -> bool {
    match dialect {
        SchemaDialect::Postgres => actual == requested,
        SchemaDialect::Sqlite => actual.eq_ignore_ascii_case(requested),
    }
}

fn quote_table(table: &str) -> Result<String, TypedSqlError> {
    // v0alpha1 table names use at most one dot as a schema separator.
    // An identifier containing a literal dot requires a structured catalog ID.
    let parts: Vec<_> = table.split('.').collect();
    if parts.len() > 2 {
        return Err(fail("ambiguous table identity: more than two segments"));
    }
    parts.into_iter()
        .map(quote_identifier)
        .collect::<Result<Vec<_>, _>>()
        .map(|names| names.join("."))
}

fn quote_identifier(name: &str) -> Result<String, TypedSqlError> {
    if name.is_empty() || name.contains('\0') {
        return Err(fail("invalid SQL identifier"));
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

fn placeholder(dialect: SchemaDialect, index: usize) -> String {
    match dialect {
        SchemaDialect::Postgres => format!("{}{}", '$', index),
        SchemaDialect::Sqlite => format!("?{index}"),
    }
}

fn verify_value(
    dialect: SchemaDialect,
    column: &SchemaColumn,
    value: &SqlParameter,
) -> Result<(), TypedSqlError> {
    let ty = column.sql_type.trim().to_ascii_lowercase();
    let valid = match (dialect, ty.as_str(), value) {
        (SchemaDialect::Postgres, "integer" | "int" | "int4" | "bigint" | "int8", SqlParameter::Integer(_)) => true,
        (SchemaDialect::Postgres, "smallint" | "int2", SqlParameter::Integer(v)) => i16::try_from(*v).is_ok(),
        (SchemaDialect::Postgres, "text" | "character varying" | "varchar", SqlParameter::Text(_)) => true,
        (SchemaDialect::Postgres, "boolean" | "bool", SqlParameter::Boolean(_)) => true,
        (SchemaDialect::Postgres, "double precision" | "float8" | "real" | "float4", SqlParameter::Real(v)) => v.is_finite(),
        (SchemaDialect::Postgres, "bytea", SqlParameter::Bytes(_)) => true,
        (SchemaDialect::Sqlite, "integer" | "int", SqlParameter::Integer(_)) => true,
        (SchemaDialect::Sqlite, "text", SqlParameter::Text(_)) => true,
        (SchemaDialect::Sqlite, "real", SqlParameter::Real(v)) => v.is_finite(),
        (SchemaDialect::Sqlite, "blob", SqlParameter::Bytes(_)) => true,
        _ => false,
    };
    if !valid {
        return Err(fail(format!(
            "unsupported parameter type for column '{}' (SQL type '{}')",
            column.name, column.sql_type
        )));
    }
    Ok(())
}

fn fail(message: impl Into<String>) -> TypedSqlError {
    TypedSqlError(message.into())
}
