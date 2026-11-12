//! Parameterized SQL lowering for a deliberately restricted pure Query IR.
//!
//! This generates a prepared-statement contract, not an executable DB client.
//! The eventual adapter must verify authority, schema drift and decoding.
//! Ambiguous plan shapes fail closed rather than silently changing semantics.

use crate::query::{infer_schema, Catalog, Column, DataType, Expr, Plan, QueryError, Scalar};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SqlDialect {
    Sqlite,
    Postgres,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedQuery {
    pub sql: String,
    pub parameters: Vec<Scalar>,
    pub output_columns: Vec<Column>,
}

#[derive(Debug)]
struct SqlEmitter {
    dialect: SqlDialect,
    parameters: Vec<Scalar>,
}

impl SqlEmitter {
    fn new(dialect: SqlDialect) -> Self {
        Self {
            dialect,
            parameters: Vec::new(),
        }
    }

    fn marker(&self) -> String {
        match self.dialect {
            SqlDialect::Sqlite => "?".into(),
            SqlDialect::Postgres => format!("{}{}", '$', self.parameters.len()),
        }
    }

    fn parameter(&mut self, value: Scalar) -> Result<String, QueryError> {
        const MIN_INT48: i64 = -(1_i64 << 47);
        const MAX_INT48: i64 = (1_i64 << 47) - 1;
        if let Scalar::Int(n) = &value {
            if !(MIN_INT48..=MAX_INT48).contains(&n) {
                return Err(QueryError(
                    "integer parameter exceeds Nulang Int48 range".into(),
                ));
            }
        }
        let ty = match (self.dialect, value.data_type()) {
            (SqlDialect::Sqlite, DataType::Int | DataType::Bool) => "INTEGER",
            (SqlDialect::Sqlite, DataType::Text) => "TEXT",
            (SqlDialect::Postgres, DataType::Int) => "BIGINT",
            (SqlDialect::Postgres, DataType::Bool) => "BOOLEAN",
            (SqlDialect::Postgres, DataType::Text) => "TEXT",
        };
        self.parameters.push(value);
        Ok(format!("CAST({} AS {ty})", self.marker()))
    }

    fn expression(&mut self, expression: &Expr, schema: &[Column]) -> Result<String, QueryError> {
        match expression {
            Expr::Field(name) => quote_identifier(name),
            Expr::Literal(value) => self.parameter(value.clone()),
            Expr::Eq(a, b) => Ok(format!(
                "({} = {})",
                self.expression(a, schema)?,
                self.expression(b, schema)?
            )),
            Expr::Gt(a, b) => {
                if operand_type(a, schema) == Some(DataType::Text) {
                    return Err(QueryError(
                        "text greater-than requires explicit SQL collation".into(),
                    ));
                }
                Ok(format!(
                    "({} > {})",
                    self.expression(a, schema)?,
                    self.expression(b, schema)?
                ))
            }
            Expr::And(a, b) => Ok(format!(
                "({} AND {})",
                self.expression(a, schema)?,
                self.expression(b, schema)?
            )),
        }
    }
}

fn operand_type(expr: &Expr, schema: &[Column]) -> Option<DataType> {
    match expr {
        Expr::Field(name) => schema.iter().find(|c| c.name == *name).map(|c| c.ty),
        Expr::Literal(value) => Some(value.data_type()),
        Expr::Eq(..) | Expr::Gt(..) | Expr::And(..) => Some(DataType::Bool),
    }
}

/// Quote exactly one identifier; dot paths are not interpreted as schemas.
fn quote_identifier(name: &str) -> Result<String, QueryError> {
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(QueryError(
            "empty or control-containing SQL identifier".into(),
        ));
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

/// Compile only the SQL-compatible subset of the logical plan:
/// Scan -> Filter* -> Project?/Sort? (either order) -> Limit? (requires sort).
/// Filters after projection/sort/limit and conflicting projection sort aliases
/// are rejected. Text sort/comparison needs an explicit collation contract.
pub fn compile_prepared(
    plan: &Plan,
    catalog: &Catalog,
    dialect: SqlDialect,
) -> Result<PreparedQuery, QueryError> {
    let output_columns = infer_schema(plan, catalog)?;
    let mut steps = Vec::new();
    let mut node = plan;
    let source = loop {
        match node {
            Plan::Scan { table } => break table.as_str(),
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. } => {
                steps.push(node);
                node = input;
            }
        }
    };
    let source_schema = infer_schema(&Plan::scan(source), catalog)?;
    let mut filters = Vec::new();
    let mut projection: Option<&[(String, Expr)]> = None;
    let mut sorting: Option<(&str, bool)> = None;
    let mut limit: Option<usize> = None;

    for step in steps.into_iter().rev() {
        match step {
            Plan::Scan { .. } => unreachable!(),
            Plan::Filter { predicate, .. } => {
                if projection.is_some() || sorting.is_some() || limit.is_some() {
                    return Err(QueryError(
                        "SQL filter must precede projection, sort and LIMIT".into(),
                    ));
                }
                filters.push(predicate);
            }
            Plan::Project { columns, .. } => {
                if projection.is_some() || limit.is_some() || columns.is_empty() {
                    return Err(QueryError(
                        "unsupported SQL projection order or empty projection".into(),
                    ));
                }
                if let Some((sort_by, _)) = sorting {
                    if columns.iter().any(|(name, _)| name == sort_by) {
                        return Err(QueryError("projection shadows earlier sort field".into()));
                    }
                }
                projection = Some(columns);
            }
            Plan::Sort { by, descending, .. } => {
                if sorting.is_some() || limit.is_some() {
                    return Err(QueryError("repeated or post-LIMIT sort unsupported".into()));
                }
                let schema = if projection.is_some() {
                    &output_columns
                } else {
                    &source_schema
                };
                let field = schema
                    .iter()
                    .find(|c| c.name == *by)
                    .ok_or_else(|| QueryError(format!("unknown sort field '{by}'")))?;
                if field.ty == DataType::Text {
                    return Err(QueryError(
                        "SQL text ORDER BY requires explicit collation".into(),
                    ));
                }
                sorting = Some((by.as_str(), *descending));
            }
            Plan::Limit { count, .. } => {
                if limit.is_some() || sorting.is_none() {
                    return Err(QueryError("SQL LIMIT requires a preceding ORDER BY".into()));
                }
                limit = Some(*count);
            }
        }
    }

    let mut emitter = SqlEmitter::new(dialect);
    // SQLite anonymous ? markers bind left-to-right, so visit SELECT before WHERE.
    let selection = match projection {
        Some(columns) => {
            let mut selected = Vec::new();
            for (name, expr) in columns {
                selected.push(format!(
                    "{} AS {}",
                    emitter.expression(expr, &source_schema)?,
                    quote_identifier(name)?
                ));
            }
            selected.join(", ")
        }
        None => source_schema
            .iter()
            .map(|col| quote_identifier(&col.name))
            .collect::<Result<Vec<_>, _>>()?
            .join(", "),
    };
    if selection.is_empty() {
        return Err(QueryError("empty SQL schema not supported".into()));
    }
    let mut sql = format!("SELECT {selection} FROM {}", quote_identifier(source)?);
    if !filters.is_empty() {
        let predicates = filters
            .into_iter()
            .map(|p| emitter.expression(p, &source_schema))
            .collect::<Result<Vec<_>, _>>()?;
        sql.push_str(" WHERE ");
        sql.push_str(&predicates.join(" AND "));
    }
    if let Some((by, descending)) = sorting {
        sql.push_str(&format!(
            " ORDER BY {} {}",
            quote_identifier(by)?,
            if descending { "DESC" } else { "ASC" }
        ));
    }
    if let Some(count) = limit {
        let n = i64::try_from(count).map_err(|_| QueryError("LIMIT exceeds i64 maximum".into()))?;
        if n >= (1_i64 << 47) {
            return Err(QueryError("LIMIT exceeds Nulang Int48 range".into()));
        }
        emitter.parameters.push(Scalar::Int(n));
        sql.push_str(&format!(" LIMIT {}", emitter.marker()));
    }
    Ok(PreparedQuery {
        sql,
        parameters: emitter.parameters,
        output_columns,
    })
}
