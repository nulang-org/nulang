//! Exact CSV header validation and conservative typed row decoding.
//!
//! All conversions are strict. No locale-dependent numbers, implicit nulls,
//! truthy strings, or partial records are accepted. This is a Rust-host
//! adapter, not a source-language or actor effect.

use crate::stream_records::MAX_CSV_FIELDS;
use std::collections::HashSet;
use std::fmt;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Text,
    Integer,
    Boolean,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub kind: ColumnType,
}

impl Column {
    pub fn new(name: impl Into<String>, kind: ColumnType) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scalar {
    Text(String),
    Integer(i64),
    Boolean(bool),
}

impl Scalar {
    /// Domain payload bytes only, not allocator overhead or serialized CSV.
    pub fn payload_bytes(&self) -> usize {
        match self {
            Self::Text(value) => value.len(),
            Self::Integer(_) => std::mem::size_of::<i64>(),
            Self::Boolean(_) => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvRow {
    pub cells: Vec<Scalar>,
}

impl CsvRow {
    /// Caller-measured payload size, not a total heap-resident memory cap.
    pub fn payload_bytes(&self) -> usize {
        self.cells
            .iter()
            .fold(0usize, |total, cell| total.saturating_add(cell.payload_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    InvalidSchema,
    DuplicateColumn(String),
    MissingHeader,
    HeaderMismatch { column: usize },
    WidthMismatch { row: usize, expected: usize, actual: usize },
    InvalidCell { row: usize, column: usize },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema => write!(f, "CSV schema has invalid column count or name"),
            Self::DuplicateColumn(name) => write!(f, "duplicate CSV schema column: {name}"),
            Self::MissingHeader => write!(f, "CSV input is missing required header"),
            Self::HeaderMismatch { column } => write!(f, "CSV header mismatch at column {column}"),
            Self::WidthMismatch { row, expected, actual } => write!(
                f,
                "CSV row {row} contains {actual} columns, expected {expected}"
            ),
            Self::InvalidCell { row, column } => {
                write!(f, "invalid CSV cell at row {row}, column {column}")
            }
        }
    }
}

impl std::error::Error for SchemaError {}

#[derive(Debug, Clone)]
pub struct CsvSchema {
    columns: Vec<Column>,
}

impl CsvSchema {
    pub fn new(columns: Vec<Column>) -> Result<Self, SchemaError> {
        if columns.is_empty() || columns.len() > MAX_CSV_FIELDS {
            return Err(SchemaError::InvalidSchema);
        }
        let mut seen = HashSet::with_capacity(columns.len());
        for col in &columns {
            if col.name.trim().is_empty() {
                return Err(SchemaError::InvalidSchema);
            }
            if !seen.insert(col.name.clone()) {
                return Err(SchemaError::DuplicateColumn(col.name.clone()));
            }
        }
        Ok(Self { columns })
    }

    fn validate_header(&self, header: &[String]) -> Result<(), SchemaError> {
        for (index, column) in self.columns.iter().enumerate() {
            if header.get(index) != Some(&column.name) {
                return Err(SchemaError::HeaderMismatch { column: index });
            }
        }
        if header.len() != self.columns.len() {
            return Err(SchemaError::HeaderMismatch {
                column: self.columns.len(),
            });
        }
        Ok(())
    }

    fn decode_row(&self, row: usize, fields: Vec<String>) -> Result<CsvRow, SchemaError> {
        if fields.len() != self.columns.len() {
            return Err(SchemaError::WidthMismatch {
                row,
                expected: self.columns.len(),
                actual: fields.len(),
            });
        }
        let mut cells = Vec::with_capacity(fields.len());
        for (column, (text, spec)) in fields.into_iter().zip(&self.columns).enumerate() {
            let value = match spec.kind {
                ColumnType::Text => Scalar::Text(text),
                ColumnType::Integer => Scalar::Integer(
                    text.parse::<i64>()
                        .map_err(|_| SchemaError::InvalidCell { row, column })?,
                ),
                ColumnType::Boolean => match text.as_str() {
                    "true" => Scalar::Boolean(true),
                    "false" => Scalar::Boolean(false),
                    _ => return Err(SchemaError::InvalidCell { row, column }),
                },
            };
            cells.push(value);
        }
        Ok(CsvRow { cells })
    }
}

#[derive(Debug)]
pub enum TypedCsvError {
    Source(io::Error),
    Schema(SchemaError),
}

impl fmt::Display for TypedCsvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(f, "CSV read failed: {error}"),
            Self::Schema(error) => write!(f, "CSV schema failed: {error}"),
        }
    }
}

impl std::error::Error for TypedCsvError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Schema(error) => Some(error),
        }
    }
}

/// Pull-driven typed CSV rows with optional exact header checking.
///
/// Errors are terminal; the next poll after any failure returns None.
/// Each data row is validated before being exposed to the caller.
pub struct TypedCsv<I> {
    source: I,
    schema: CsvSchema,
    expect_header: bool,
    started: bool,
    finished: bool,
    row_index: usize,
}

impl<I> TypedCsv<I>
where
    I: Iterator<Item = io::Result<Vec<String>>>,
{
    pub fn new(source: I, schema: CsvSchema, expect_header: bool) -> Self {
        Self {
            source,
            schema,
            expect_header,
            started: false,
            finished: false,
            row_index: 0,
        }
    }
}

impl<I> Iterator for TypedCsv<I>
where
    I: Iterator<Item = io::Result<Vec<String>>>,
{
    type Item = Result<CsvRow, TypedCsvError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        if !self.started {
            self.started = true;
            if self.expect_header {
                match self.source.next() {
                    None => {
                        self.finished = true;
                        return Some(Err(TypedCsvError::Schema(SchemaError::MissingHeader)));
                    }
                    Some(Err(error)) => {
                        self.finished = true;
                        return Some(Err(TypedCsvError::Source(error)));
                    }
                    Some(Ok(header)) => {
                        if let Err(error) = self.schema.validate_header(&header) {
                            self.finished = true;
                            return Some(Err(TypedCsvError::Schema(error)));
                        }
                    }
                }
            }
        }

        let fields = match self.source.next() {
            None => {
                self.finished = true;
                return None;
            }
            Some(Err(error)) => {
                self.finished = true;
                return Some(Err(TypedCsvError::Source(error)));
            }
            Some(Ok(fields)) => fields,
        };

        self.row_index = self.row_index.saturating_add(1);
        match self.schema.decode_row(self.row_index, fields) {
            Ok(row) => Some(Ok(row)),
            Err(error) => {
                self.finished = true;
                Some(Err(TypedCsvError::Schema(error)))
            }
        }
    }
}
