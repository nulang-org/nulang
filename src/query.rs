//! Experimental, effect-free relational Query IR for host-side planning.
//!
//! This module is intentionally independent of SQL clients, the bytecode ABI,
//! the Nulang source grammar, and the persistence runtime. It offers a small
//! typed reference executor and safe optimizer rules for future adapters.
//! It is not an SQL parser, a distributed executor, or a source-level feature.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataType {
    Int,
    Bool,
    Text,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Scalar {
    Int(i64),
    Bool(bool),
    Text(String),
}

impl Scalar {
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int(_) => DataType::Int,
            Self::Bool(_) => DataType::Bool,
            Self::Text(_) => DataType::Text,
        }
    }
}

/// A missing field is an error, not an implicit SQL NULL.
pub type Row = BTreeMap<String, Scalar>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: DataType,
}

impl Column {
    pub fn new(name: impl Into<String>, ty: DataType) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryError(pub String);

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for QueryError {}

#[derive(Clone, Debug)]
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Row>,
}

impl Table {
    pub fn new(columns: Vec<Column>, rows: Vec<Row>) -> Result<Self, QueryError> {
        validate_columns(&columns)?;
        for (index, row) in rows.iter().enumerate() {
            if row.len() != columns.len() {
                return Err(QueryError(format!(
                    "row {index}: missing or unexpected columns"
                )));
            }
            for column in &columns {
                let actual = row.get(&column.name).ok_or_else(|| {
                    QueryError(format!("row {index}: missing column '{}'", column.name))
                })?;
                if actual.data_type() != column.ty {
                    return Err(QueryError(format!(
                        "row {index}: column '{}' expects {:?}, found {:?}",
                        column.name,
                        column.ty,
                        actual.data_type()
                    )));
                }
            }
        }
        Ok(Self { columns, rows })
    }
}

fn validate_columns(columns: &[Column]) -> Result<(), QueryError> {
    let mut seen = BTreeSet::new();
    for column in columns {
        if column.name.is_empty() || !seen.insert(column.name.as_str()) {
            return Err(QueryError(format!(
                "empty or duplicate column name '{}'",
                column.name
            )));
        }
    }
    Ok(())
}

#[derive(Default, Clone, Debug)]
pub struct Catalog {
    tables: BTreeMap<String, Table>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, table: Table) -> Result<(), QueryError> {
        let name = name.into();
        if name.is_empty() || self.tables.contains_key(&name) {
            return Err(QueryError(format!(
                "empty or duplicate table name '{name}'"
            )));
        }
        self.tables.insert(name, table);
        Ok(())
    }

    fn table(&self, name: &str) -> Result<&Table, QueryError> {
        self.tables
            .get(name)
            .ok_or_else(|| QueryError(format!("unknown table '{name}'")))
    }
}

/// Pure expressions only: no arbitrary closures, I/O or actor effects.
/// This restriction is necessary for optimizer reordering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Expr {
    Field(String),
    Literal(Scalar),
    Eq(Box<Expr>, Box<Expr>),
    Gt(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
}

impl Expr {
    pub fn field(name: impl Into<String>) -> Self {
        Self::Field(name.into())
    }

    pub fn int(value: i64) -> Self {
        Self::Literal(Scalar::Int(value))
    }

    pub fn text(value: impl Into<String>) -> Self {
        Self::Literal(Scalar::Text(value.into()))
    }

    pub fn bool(value: bool) -> Self {
        Self::Literal(Scalar::Bool(value))
    }

    pub fn eq(left: Self, right: Self) -> Self {
        Self::Eq(Box::new(left), Box::new(right))
    }

    pub fn gt(left: Self, right: Self) -> Self {
        Self::Gt(Box::new(left), Box::new(right))
    }

    pub fn and(left: Self, right: Self) -> Self {
        Self::And(Box::new(left), Box::new(right))
    }

    fn ty(&self, schema: &[Column]) -> Result<DataType, QueryError> {
        match self {
            Self::Field(name) => field_type(schema, name),
            Self::Literal(value) => Ok(value.data_type()),
            Self::Eq(a, b) | Self::Gt(a, b) => {
                let left = a.ty(schema)?;
                let right = b.ty(schema)?;
                if left != right {
                    return Err(QueryError(format!(
                        "comparison type mismatch: {left:?} versus {right:?}"
                    )));
                }
                if matches!(self, Self::Gt(_, _)) && left == DataType::Bool {
                    return Err(QueryError("greater-than requires Int or Text".into()));
                }
                Ok(DataType::Bool)
            }
            Self::And(a, b) => {
                if a.ty(schema)? != DataType::Bool || b.ty(schema)? != DataType::Bool {
                    return Err(QueryError("AND requires Bool operands".into()));
                }
                Ok(DataType::Bool)
            }
        }
    }

    fn eval(&self, row: &Row) -> Result<Scalar, QueryError> {
        match self {
            Self::Field(name) => row
                .get(name)
                .cloned()
                .ok_or_else(|| QueryError(format!("missing field '{name}'"))),
            Self::Literal(value) => Ok(value.clone()),
            Self::Eq(a, b) => Ok(Scalar::Bool(a.eval(row)? == b.eval(row)?)),
            Self::Gt(a, b) => Ok(Scalar::Bool(a.eval(row)? > b.eval(row)?)),
            Self::And(a, b) => match a.eval(row)? {
                Scalar::Bool(false) => Ok(Scalar::Bool(false)),
                Scalar::Bool(true) => b.eval(row),
                _ => Err(QueryError("AND requires Bool operands".into())),
            },
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Field(name) => name.clone(),
            Self::Literal(Scalar::Int(value)) => value.to_string(),
            Self::Literal(Scalar::Bool(value)) => value.to_string(),
            Self::Literal(Scalar::Text(value)) => format!("{value:?}"),
            Self::Eq(a, b) => format!("{} == {}", a.display(), b.display()),
            Self::Gt(a, b) => format!("{} > {}", a.display(), b.display()),
            Self::And(a, b) => format!("({}) AND ({})", a.display(), b.display()),
        }
    }
}

fn field_type(schema: &[Column], name: &str) -> Result<DataType, QueryError> {
    schema
        .iter()
        .find(|field| field.name == name)
        .map(|field| field.ty)
        .ok_or_else(|| QueryError(format!("unknown field '{name}'")))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Plan {
    Scan {
        table: String,
    },
    Filter {
        input: Box<Plan>,
        predicate: Expr,
    },
    Project {
        input: Box<Plan>,
        columns: Vec<(String, Expr)>,
    },
    Sort {
        input: Box<Plan>,
        by: String,
        descending: bool,
    },
    Limit {
        input: Box<Plan>,
        count: usize,
    },
}

impl Plan {
    pub fn scan(table: impl Into<String>) -> Self {
        Self::Scan {
            table: table.into(),
        }
    }

    pub fn filter(self, predicate: Expr) -> Self {
        Self::Filter {
            input: Box::new(self),
            predicate,
        }
    }

    pub fn project<N: Into<String>>(self, columns: Vec<(N, Expr)>) -> Self {
        Self::Project {
            input: Box::new(self),
            columns: columns
                .into_iter()
                .map(|(name, expr)| (name.into(), expr))
                .collect(),
        }
    }

    pub fn sort(self, by: impl Into<String>, descending: bool) -> Self {
        Self::Sort {
            input: Box::new(self),
            by: by.into(),
            descending,
        }
    }

    pub fn limit(self, count: usize) -> Self {
        Self::Limit {
            input: Box::new(self),
            count,
        }
    }
}

/// Typecheck the entire logical plan before interpreting or optimizing it.
pub fn infer_schema(plan: &Plan, catalog: &Catalog) -> Result<Vec<Column>, QueryError> {
    match plan {
        Plan::Scan { table } => Ok(catalog.table(table)?.columns.clone()),
        Plan::Filter { input, predicate } => {
            let schema = infer_schema(input, catalog)?;
            if predicate.ty(&schema)? != DataType::Bool {
                return Err(QueryError("filter predicate must return Bool".into()));
            }
            Ok(schema)
        }
        Plan::Project { input, columns } => {
            let input_schema = infer_schema(input, catalog)?;
            let projected = columns
                .iter()
                .map(|(name, expr)| Ok(Column::new(name, expr.ty(&input_schema)?)))
                .collect::<Result<Vec<_>, QueryError>>()?;
            validate_columns(&projected)?;
            Ok(projected)
        }
        Plan::Sort { input, by, .. } => {
            let schema = infer_schema(input, catalog)?;
            field_type(&schema, by)?;
            Ok(schema)
        }
        Plan::Limit { input, .. } => infer_schema(input, catalog),
    }
}

/// Local reference executor. This intentionally materializes intermediate rows,
/// and is not suitable as a general-purpose database query engine.
pub fn execute(plan: &Plan, catalog: &Catalog) -> Result<Vec<Row>, QueryError> {
    infer_schema(plan, catalog)?;
    execute_validated(plan, catalog)
}

fn execute_validated(plan: &Plan, catalog: &Catalog) -> Result<Vec<Row>, QueryError> {
    match plan {
        Plan::Scan { table } => Ok(catalog.table(table)?.rows.clone()),
        Plan::Filter { input, predicate } => {
            let mut output = Vec::new();
            for row in execute_validated(input, catalog)? {
                match predicate.eval(&row)? {
                    Scalar::Bool(true) => output.push(row),
                    Scalar::Bool(false) => {}
                    _ => return Err(QueryError("filter predicate must return Bool".into())),
                }
            }
            Ok(output)
        }
        Plan::Project { input, columns } => execute_validated(input, catalog)?
            .into_iter()
            .map(|row| {
                let mut projected = Row::new();
                for (name, expression) in columns {
                    projected.insert(name.clone(), expression.eval(&row)?);
                }
                Ok(projected)
            })
            .collect(),
        Plan::Sort {
            input,
            by,
            descending,
        } => {
            let mut rows = execute_validated(input, catalog)?;
            rows.sort_by(|a, b| {
                // Schema and table invariants were checked before execution.
                let comparison = a.get(by).cmp(&b.get(by));
                if *descending {
                    comparison.reverse()
                } else {
                    comparison
                }
            });
            Ok(rows)
        }
        Plan::Limit { input, count } => {
            let mut rows = execute_validated(input, catalog)?;
            rows.truncate(*count);
            Ok(rows)
        }
    }
}

/// Apply only equivalence-preserving rewrites over the pure, validated IR.
///
/// In particular, LIMIT and PROJECT are not filter-pushdown boundaries:
/// changing their order can change result cardinality or field availability.
pub fn optimize(plan: &Plan, catalog: &Catalog) -> Result<Plan, QueryError> {
    infer_schema(plan, catalog)?;
    Ok(optimize_validated(plan.clone()))
}

fn optimize_validated(plan: Plan) -> Plan {
    match plan {
        Plan::Scan { .. } => plan,
        Plan::Filter { input, predicate } => {
            let optimized_input = optimize_validated(*input);
            match optimized_input {
                Plan::Sort {
                    input,
                    by,
                    descending,
                } => Plan::Sort {
                    input: Box::new(optimize_validated(Plan::Filter { input, predicate })),
                    by,
                    descending,
                },
                other => Plan::Filter {
                    input: Box::new(other),
                    predicate,
                },
            }
        }
        Plan::Project { input, columns } => Plan::Project {
            input: Box::new(optimize_validated(*input)),
            columns,
        },
        Plan::Sort {
            input,
            by,
            descending,
        } => Plan::Sort {
            input: Box::new(optimize_validated(*input)),
            by,
            descending,
        },
        Plan::Limit { input, count } => Plan::Limit {
            input: Box::new(optimize_validated(*input)),
            count,
        },
    }
}

/// Stable, text-only logical plans, without executing queries or opening stores.
pub fn explain(plan: &Plan) -> String {
    let mut output = String::new();
    explain_into(plan, 0, &mut output);
    output.trim_end_matches('\n').to_string()
}

fn explain_into(plan: &Plan, depth: usize, output: &mut String) {
    output.push_str(&"  ".repeat(depth));
    match plan {
        Plan::Scan { table } => output.push_str(&format!("Scan {table}\n")),
        Plan::Filter { input, predicate } => {
            output.push_str(&format!("Filter ({})\n", predicate.display()));
            explain_into(input, depth + 1, output);
        }
        Plan::Project { input, columns } => {
            let names = columns
                .iter()
                .map(|(name, expression)| format!("{name}={}", expression.display()))
                .collect::<Vec<_>>()
                .join(", ");
            output.push_str(&format!("Project {names}\n"));
            explain_into(input, depth + 1, output);
        }
        Plan::Sort {
            input,
            by,
            descending,
        } => {
            let direction = if *descending { "DESC" } else { "ASC" };
            output.push_str(&format!("Sort {by} {direction}\n"));
            explain_into(input, depth + 1, output);
        }
        Plan::Limit { input, count } => {
            output.push_str(&format!("Limit {count}\n"));
            explain_into(input, depth + 1, output);
        }
    }
}
