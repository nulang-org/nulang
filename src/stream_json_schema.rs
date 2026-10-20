//! Strict, schema-ordered JSONL object validation for trusted Rust hosts.
//!
//! Operates on decoded values from JsonlRecords; the upstream reader enforces
//! raw-record byte caps. This adapter is synchronous, lazy, and fail-closed.
//! It adds no Nulang language syntax, VM effects, actor dispatch, or replay.

use crate::stream_records::JsonlError;
use crate::stream_schema::{ColumnType, Scalar};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

pub const MAX_JSON_SCHEMA_FIELDS: usize = 1024;
pub const MAX_JSON_SCHEMA_KEY_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonField {
    pub name: String,
    pub kind: ColumnType,
}

impl JsonField {
    pub fn new(name: impl Into<String>, kind: ColumnType) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonSchemaError {
    InvalidSchema,
    DuplicateField(String),
    NotObject { row: usize },
    MissingField { row: usize, column: usize },
    UnexpectedField { row: usize },
    InvalidFieldType { row: usize, column: usize },
}

impl fmt::Display for JsonSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSchema => write!(f, "JSON object schema is invalid"),
            Self::DuplicateField(name) => write!(f, "duplicate schema field: {name}"),
            Self::NotObject { row } => write!(f, "JSONL row {row} must be an object"),
            Self::MissingField { row, column } => {
                write!(f, "JSONL row {row} is missing schema column {column}")
            }
            Self::UnexpectedField { row } => write!(f, "JSONL row {row} has unknown fields"),
            Self::InvalidFieldType { row, column } => {
                write!(f, "JSONL row {row}, column {column} has an invalid type")
            }
        }
    }
}

impl std::error::Error for JsonSchemaError {}

/// Every configured field is required, non-null, and precisely typed.
/// Unexpected keys are rejected. Names are compared case-sensitively.
/// A parsed serde_json object has already lost information on duplicate keys;
/// this adapter does not detect duplicate keys in the original JSON text.
#[derive(Debug, Clone)]
pub struct JsonSchema {
    fields: Vec<JsonField>,
}

impl JsonSchema {
    pub fn new(fields: Vec<JsonField>) -> Result<Self, JsonSchemaError> {
        if fields.is_empty() || fields.len() > MAX_JSON_SCHEMA_FIELDS {
            return Err(JsonSchemaError::InvalidSchema);
        }

        let mut seen = HashSet::with_capacity(fields.len());
        for field in &fields {
            if field.name.trim().is_empty() || field.name.len() > MAX_JSON_SCHEMA_KEY_BYTES {
                return Err(JsonSchemaError::InvalidSchema);
            }
            if !seen.insert(field.name.clone()) {
                return Err(JsonSchemaError::DuplicateField(field.name.clone()));
            }
        }
        Ok(Self { fields })
    }

    fn decode(&self, row: usize, json: Value) -> Result<JsonRow, JsonSchemaError> {
        let Value::Object(mut object) = json else {
            return Err(JsonSchemaError::NotObject { row });
        };

        let mut fields = Vec::with_capacity(self.fields.len());
        for (column, field) in self.fields.iter().enumerate() {
            let value = object
                .remove(&field.name)
                .ok_or(JsonSchemaError::MissingField { row, column })?;
            let scalar = match (field.kind, value) {
                (ColumnType::Text, Value::String(value)) => Scalar::Text(value),
                (ColumnType::Integer, Value::Number(value)) => {
                    match value.as_i64() {
                        Some(number) => Scalar::Integer(number),
                        None => return Err(JsonSchemaError::InvalidFieldType { row, column }),
                    }
                }
                (ColumnType::Boolean, Value::Bool(value)) => Scalar::Boolean(value),
                _ => return Err(JsonSchemaError::InvalidFieldType { row, column }),
            };
            fields.push((field.name.clone(), scalar));
        }

        if !object.is_empty() {
            return Err(JsonSchemaError::UnexpectedField { row });
        }
        Ok(JsonRow { fields })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonRow {
    /// Ordered to match the configured schema, not JSON input key order.
    pub fields: Vec<(String, Scalar)>,
}

impl JsonRow {
    /// Logical unescaped UTF-8 keys + scalar values, not serialized JSON
    /// length, heap allocation, original record size, or full peak memory.
    pub fn payload_bytes(&self) -> usize {
        self.fields.iter().fold(0usize, |total, (name, scalar)| {
            total
                .saturating_add(name.len())
                .saturating_add(scalar.payload_bytes())
        })
    }
}

#[derive(Debug)]
pub enum TypedJsonlError {
    Source(JsonlError),
    Schema(JsonSchemaError),
}

impl fmt::Display for TypedJsonlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(f, "JSONL record failed: {error}"),
            Self::Schema(error) => write!(f, "JSONL schema validation failed: {error}"),
        }
    }
}

impl std::error::Error for TypedJsonlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Schema(error) => Some(error),
        }
    }
}

/// Strict schema adapter for fallible JSONL-value streams.
///
/// Source and schema errors are terminal: once an error is returned, all
/// subsequent calls return None. No additional source records are prefetched.
pub struct TypedJsonl<I> {
    source: I,
    schema: JsonSchema,
    finished: bool,
    row_index: usize,
}

impl<I: Iterator<Item = Result<Value, JsonlError>>> TypedJsonl<I> {
    pub fn new(source: I, schema: JsonSchema) -> Self {
        Self {
            source,
            schema,
            finished: false,
            row_index: 0,
        }
    }
}

impl<I: Iterator<Item = Result<Value, JsonlError>>> Iterator for TypedJsonl<I> {
    type Item = Result<JsonRow, TypedJsonlError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let json = match self.source.next() {
            None => {
                self.finished = true;
                return None;
            }
            Some(Err(error)) => {
                self.finished = true;
                return Some(Err(TypedJsonlError::Source(error)));
            }
            Some(Ok(value)) => value,
        };

        self.row_index = self.row_index.saturating_add(1);
        match self.schema.decode(self.row_index, json) {
            Ok(row) => Some(Ok(row)),
            Err(error) => {
                self.finished = true;
                Some(Err(TypedJsonlError::Schema(error)))
            }
        }
    }
}
