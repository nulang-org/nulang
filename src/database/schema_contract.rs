//! Portable, versioned relational schema contracts for typed query tooling.
//!
//! This module does not execute SQL, introspect a database, or claim to
//! represent every database object. Only the explicitly modelled table,
//! column, primary-key, index and foreign-key metadata contributes to the
//! fingerprint. The format is intentionally separate from actor-state schema
//! identity (which describes durability/replay semantics).
//!
//! Consumers MUST reject unsupported catalog objects before treating a
//! fingerprint as evidence of complete database drift freedom.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

pub const RELATIONAL_SCHEMA_VERSION: &str = "nulang.relational-schema/v0alpha1";
const FINGERPRINT_DOMAIN: &[u8] = b"nulang.relational-schema-fingerprint.v0alpha1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SchemaDialect {
    Postgres,
    Sqlite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationalSchema {
    pub version: String,
    pub dialect: SchemaDialect,
    pub tables: Vec<SchemaTable>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaTable {
    /// Resolved catalog identity (e.g. public.users), NOT an SQL fragment.
    pub name: String,
    pub columns: Vec<SchemaColumn>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primary_key: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<Index>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaColumn {
    pub name: String,
    /// Catalog type name retained verbatim. Do not interpolate into SQL.
    pub sql_type: String,
    pub nullable: bool,
    /// Catalog default expression retained verbatim. Do not execute it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_sql: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub name: String,
    /// Column order is semantically significant for composite indexes.
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub references_table: String,
    /// Column order corresponds positionally to `columns`.
    pub references_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaContractError(pub String);

impl fmt::Display for SchemaContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for SchemaContractError {}

impl RelationalSchema {
    /// Validate the subset of relational catalog metadata defined by v0alpha1.
    ///
    /// This is structural validation, not SQL parsing or compatibility analysis.
    pub fn validate(&self) -> Result<(), SchemaContractError> {
        if self.version != RELATIONAL_SCHEMA_VERSION {
            return Err(invalid(format!("unsupported relational schema contract '{}'", self.version)));
        }

        let mut tables = BTreeMap::new();
        for table in &self.tables {
            check_name(&table.name, "table")?;
            if tables.insert(identifier_key(self.dialect, &table.name), table).is_some() {
                return Err(invalid(format!("duplicate table '{}'", table.name)));
            }
        }

        for table in &self.tables {
            if table.columns.is_empty() {
                return Err(invalid(format!("table '{}' has no columns", table.name)));
            }
            let mut columns = BTreeSet::new();
            for column in &table.columns {
                check_name(&column.name, "column")?;
                if !columns.insert(identifier_key(self.dialect, &column.name)) {
                    return Err(invalid(format!("duplicate column '{}' on '{}'", column.name, table.name)));
                }
                check_text(&column.sql_type, "SQL type")?;
                if let Some(default_sql) = &column.default_sql {
                    check_text(default_sql, "column default")?;
                }
            }

            check_columns(&table.primary_key, &columns, self.dialect, false, &table.name)?;
            let mut index_names = BTreeSet::new();
            for index in &table.indexes {
                check_name(&index.name, "index")?;
                if !index_names.insert(identifier_key(self.dialect, &index.name)) {
                    return Err(invalid(format!("duplicate index '{}' on '{}'", index.name, table.name)));
                }
                check_columns(&index.columns, &columns, self.dialect, true, &index.name)?;
            }

            let mut foreign_key_names = BTreeSet::new();
            for foreign_key in &table.foreign_keys {
                check_name(&foreign_key.name, "foreign key")?;
                if !foreign_key_names.insert(identifier_key(self.dialect, &foreign_key.name)) {
                    return Err(invalid(format!("duplicate foreign key '{}' on '{}'", foreign_key.name, table.name)));
                }
                check_columns(&foreign_key.columns, &columns, self.dialect, true, &foreign_key.name)?;
                check_name(&foreign_key.references_table, "referenced table")?;
                if foreign_key.columns.len() != foreign_key.references_columns.len() {
                    return Err(invalid(format!("foreign key '{}' has mismatched column counts", foreign_key.name)));
                }
                let target = tables.get(&identifier_key(self.dialect, &foreign_key.references_table))
                    .ok_or_else(|| invalid(format!("foreign key '{}' references missing table '{}'",
                        foreign_key.name, foreign_key.references_table)))?;
                let target_columns: BTreeSet<String> = target.columns.iter()
                    .map(|column| identifier_key(self.dialect, &column.name))
                    .collect();
                check_columns(&foreign_key.references_columns, &target_columns,
                    self.dialect, true, &foreign_key.name)?;
            }
        }
        Ok(())
    }

    /// Deterministic, pretty-printed manifest for CLI/Cloud interchange.
    pub fn to_json(&self) -> Result<Vec<u8>, SchemaContractError> {
        serde_json::to_vec_pretty(&self.normalized()?)
            .map_err(|err| invalid(format!("cannot serialize schema contract: {err}")))
    }

    /// Fail closed on unknown fields, unknown versions and invalid references.
    pub fn from_json(bytes: &[u8]) -> Result<Self, SchemaContractError> {
        let parsed: Self = serde_json::from_slice(bytes)
            .map_err(|err| invalid(format!("invalid schema contract JSON: {err}")))?;
        parsed.normalized()
    }

    /// Domain-separated BLAKE3 digest of compact, order-normalized JSON.
    pub fn fingerprint(&self) -> Result<String, SchemaContractError> {
        let canonical = serde_json::to_vec(&self.normalized()?)
            .map_err(|err| invalid(format!("cannot serialize schema contract: {err}")))?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(FINGERPRINT_DOMAIN);
        hasher.update(&canonical);
        Ok(format!("blake3:{}", hasher.finalize().to_hex()))
    }

    /// Resolve a named column. For trusted metadata only: callers should
    /// validate untrusted schemas before calling this convenience method.
    pub fn column(&self, table: &str, column: &str) -> Option<&SchemaColumn> {
        self.tables.iter()
            .find(|item| identifier_key(self.dialect, &item.name) == identifier_key(self.dialect, table))
            .and_then(|item| item.columns.iter().find(|field|
                identifier_key(self.dialect, &field.name) == identifier_key(self.dialect, column)))
    }

    fn normalized(&self) -> Result<Self, SchemaContractError> {
        self.validate()?;
        let mut result = self.clone();
        result.tables.sort_by(|a, b| a.name.cmp(&b.name));
        for table in &mut result.tables {
            table.columns.sort_by(|a, b| a.name.cmp(&b.name));
            table.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            table.foreign_keys.sort_by(|a, b| a.name.cmp(&b.name));
        }
        Ok(result)
    }
}

fn identifier_key(dialect: SchemaDialect, name: &str) -> String {
    match dialect {
        SchemaDialect::Postgres => name.to_string(),
        SchemaDialect::Sqlite => name.to_ascii_lowercase(),
    }
}

fn check_name(name: &str, kind: &str) -> Result<(), SchemaContractError> {
    if name.is_empty() || name.trim() != name || name.contains('\0') {
        return Err(invalid(format!("invalid {kind} name '{name}'")));
    }
    Ok(())
}

fn check_text(value: &str, kind: &str) -> Result<(), SchemaContractError> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err(invalid(format!("invalid {kind}")));
    }
    Ok(())
}

fn check_columns(
    names: &[String],
    known: &BTreeSet<String>,
    dialect: SchemaDialect,
    required: bool,
    owner: &str,
) -> Result<(), SchemaContractError> {
    if required && names.is_empty() {
        return Err(invalid(format!("'{owner}' requires at least one column")));
    }
    let mut seen = BTreeSet::new();
    for name in names {
        check_name(name, "column reference")?;
        let key = identifier_key(dialect, name);
        if !known.contains(&key) {
            return Err(invalid(format!("'{owner}' references unknown column '{name}'")));
        }
        if !seen.insert(key) {
            return Err(invalid(format!("'{owner}' repeats column '{name}'")));
        }
    }
    Ok(())
}

fn invalid(message: String) -> SchemaContractError {
    SchemaContractError(message)
}
