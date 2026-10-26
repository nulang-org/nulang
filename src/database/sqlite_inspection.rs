//! Read-only, fail-closed SQLite catalog inspection for the relational
//! schema v0alpha1 contract. Available only with the existing `sqlite` feature.
//!
//! This reader cannot certify full drift freedom: v0alpha1 has no representation
//! for CHECK constraints, collations on columns, conflict clauses, rowid modes,
//! policies, triggers or several other SQLite DDL semantics. The result is
//! permanently marked partial until a richer contract is implemented.

use std::collections::BTreeMap;

use super::schema_contract::{
    ForeignKey, Index, RelationalSchema, SchemaColumn, SchemaContractError, SchemaDialect,
    SchemaTable, RELATIONAL_SCHEMA_VERSION,
};

/// A safe-to-inspect metadata subset, NOT a full-schema attestation.
#[derive(Debug, Clone)]
pub struct SqliteInspection {
    pub schema: RelationalSchema,
}

impl SqliteInspection {
    pub fn coverage_is_complete(&self) -> bool {
        false
    }

    /// Fail closed. This intentionally cannot pass in v0alpha1.
    pub fn require_complete(&self) -> Result<&RelationalSchema, SchemaContractError> {
        Err(error("SQLite v0alpha1 introspection is partial; CHECK constraints, column collations, conflict rules and other catalog features are not represented"))
    }
}

/// Reads SQLite catalog metadata without executing source DDL or interpolating
/// table/index names. The caller should use a read-only connection where
/// possible. A `schema_version` recheck detects DDL during inspection.
pub async fn inspect_sqlite(
    conn: &libsql::Connection,
) -> Result<SqliteInspection, SchemaContractError> {
    let version_before = schema_version(conn).await?;

    let mut tables = Vec::new();
    let mut catalog = conn
        .query(
            "SELECT name, type, coalesce(sql, '') FROM sqlite_schema \
         WHERE substr(lower(name), 1, 7) != 'sqlite_' ORDER BY name",
            (),
        )
        .await
        .map_err(db_error)?;

    let mut table_names = Vec::new();
    while let Some(row) = catalog.next().await.map_err(db_error)? {
        let name: String = row.get(0).map_err(db_error)?;
        let kind: String = row.get(1).map_err(db_error)?;
        let ddl: String = row.get(2).map_err(db_error)?;
        match kind.as_str() {
            "table" => {
                // SQLite represents virtual tables and their shadow tables as
                // catalog tables, but they cannot be safely represented here.
                if ddl
                    .trim_start()
                    .to_ascii_uppercase()
                    .starts_with("CREATE VIRTUAL TABLE")
                {
                    return Err(error(format!("virtual table '{name}' is not supported")));
                }
                table_names.push(name);
            }
            "index" => {}
            _ => {
                return Err(error(format!(
                    "SQLite {kind} '{name}' is not represented by v0alpha1"
                )))
            }
        }
    }
    // Release the catalog cursor before making nested catalog queries.
    drop(catalog);

    for table_name in &table_names {
        // sqlite's table-valued pragma functions support bound argument values,
        // unlike constructing PRAGMA strings from untrusted identifiers.
        let mut rows = conn
            .query(
                "SELECT name, type, \"notnull\", coalesce(dflt_value, ''), \
                    dflt_value IS NULL, pk, hidden \
             FROM pragma_table_xinfo(?1)",
                libsql::params![table_name.as_str()],
            )
            .await
            .map_err(db_error)?;

        let mut columns = Vec::new();
        let mut primary_key_columns = BTreeMap::new();
        while let Some(row) = rows.next().await.map_err(db_error)? {
            let name: String = row.get(0).map_err(db_error)?;
            let sql_type: String = row.get(1).map_err(db_error)?;
            let notnull: i64 = row.get(2).map_err(db_error)?;
            let default: String = row.get(3).map_err(db_error)?;
            let default_is_null: i64 = row.get(4).map_err(db_error)?;
            let primary_key_position: i64 = row.get(5).map_err(db_error)?;
            let hidden: i64 = row.get(6).map_err(db_error)?;

            if hidden != 0 {
                return Err(error(format!(
                    "SQLite table '{table_name}' has hidden or generated column '{name}' (hidden={hidden})"
                )));
            }
            if sql_type.trim().is_empty() {
                return Err(error(format!(
                    "SQLite table '{table_name}' column '{name}' lacks a declared SQL type"
                )));
            }
            if primary_key_position > 0 {
                if primary_key_columns
                    .insert(primary_key_position, name.clone())
                    .is_some()
                {
                    return Err(error(format!(
                        "SQLite table '{table_name}' has duplicate PK ordinal"
                    )));
                }
            }
            columns.push(SchemaColumn {
                name,
                sql_type,
                nullable: notnull == 0,
                default_sql: if default_is_null != 0 {
                    None
                } else {
                    Some(default)
                },
            });
        }
        drop(rows);

        // PRAGMA table_list exposes WITHOUT ROWID, STRICT, virtual/shadow.
        let mut table_meta = conn
            .query(
                "SELECT type, wr, strict FROM pragma_table_list(?1)",
                libsql::params![table_name.as_str()],
            )
            .await
            .map_err(db_error)?;
        let meta = table_meta.next().await.map_err(db_error)?.ok_or_else(|| {
            error(format!(
                "missing pragma_table_list metadata for '{table_name}'"
            ))
        })?;
        let table_type: String = meta.get(0).map_err(db_error)?;
        let without_rowid: i64 = meta.get(1).map_err(db_error)?;
        let strict: i64 = meta.get(2).map_err(db_error)?;
        if table_type != "table" || without_rowid != 0 || strict != 0 {
            return Err(error(format!(
                "SQLite table '{table_name}' uses unsupported table type '{table_type}', WITHOUT ROWID or STRICT"
            )));
        }
        drop(table_meta);

        let mut indexes = Vec::new();
        let mut index_rows = conn
            .query(
                "SELECT name, \"unique\", origin, partial \
             FROM pragma_index_list(?1)",
                libsql::params![table_name.as_str()],
            )
            .await
            .map_err(db_error)?;

        let mut indexed = Vec::new();
        while let Some(row) = index_rows.next().await.map_err(db_error)? {
            let name: String = row.get(0).map_err(db_error)?;
            let unique: i64 = row.get(1).map_err(db_error)?;
            let origin: String = row.get(2).map_err(db_error)?;
            let partial: i64 = row.get(3).map_err(db_error)?;
            if partial != 0 {
                return Err(error(format!(
                    "partial SQLite index '{name}' is not supported"
                )));
            }
            if origin != "pk" {
                // primary-key indexes already represented by primary_key
                indexed.push((name, unique != 0));
            }
        }
        drop(index_rows);

        for (name, unique) in indexed {
            let mut key_columns = BTreeMap::new();
            let mut index_meta = conn
                .query(
                    "SELECT seqno, cid, coalesce(name, ''), \"desc\", coll, \"key\" \
                 FROM pragma_index_xinfo(?1)",
                    libsql::params![name.as_str()],
                )
                .await
                .map_err(db_error)?;
            while let Some(row) = index_meta.next().await.map_err(db_error)? {
                let seq: i64 = row.get(0).map_err(db_error)?;
                let cid: i64 = row.get(1).map_err(db_error)?;
                let column: String = row.get(2).map_err(db_error)?;
                let descending: i64 = row.get(3).map_err(db_error)?;
                let coll: String = row.get(4).map_err(db_error)?;
                let key: i64 = row.get(5).map_err(db_error)?;
                if key == 0 {
                    continue; // SQLite automatically includes rowid as non-key auxiliary metadata.
                }
                if cid < 0 {
                    return Err(error(format!(
                        "expression or rowid SQLite index '{name}' is not supported"
                    )));
                }
                if descending != 0 || !coll.eq_ignore_ascii_case("BINARY") {
                    return Err(error(format!(
                        "SQLite index '{name}' uses unsupported descending order or collation"
                    )));
                }
                if key_columns.insert(seq, column).is_some() {
                    return Err(error(format!("SQLite index '{name}' repeats key ordinal")));
                }
            }
            indexes.push(Index {
                name,
                columns: key_columns.into_values().collect(),
                unique,
            });
        }

        let mut fks: BTreeMap<i64, (String, BTreeMap<i64, (String, String)>)> = BTreeMap::new();
        let mut fk_rows = conn
            .query(
                "SELECT id, seq, \"table\", \"from\", coalesce(\"to\", ''), \
                    on_update, on_delete, \"match\" \
             FROM pragma_foreign_key_list(?1)",
                libsql::params![table_name.as_str()],
            )
            .await
            .map_err(db_error)?;
        while let Some(row) = fk_rows.next().await.map_err(db_error)? {
            let id: i64 = row.get(0).map_err(db_error)?;
            let seq: i64 = row.get(1).map_err(db_error)?;
            let target: String = row.get(2).map_err(db_error)?;
            let from: String = row.get(3).map_err(db_error)?;
            let to: String = row.get(4).map_err(db_error)?;
            let on_update: String = row.get(5).map_err(db_error)?;
            let on_delete: String = row.get(6).map_err(db_error)?;
            let match_policy: String = row.get(7).map_err(db_error)?;
            if !on_update.eq_ignore_ascii_case("NO ACTION")
                || !on_delete.eq_ignore_ascii_case("NO ACTION")
                || !match_policy.eq_ignore_ascii_case("NONE")
            {
                return Err(error(format!(
                    "SQLite foreign key on '{table_name}' has unsupported action/match semantics"
                )));
            }
            if to.is_empty() {
                return Err(error(format!(
                    "SQLite foreign key on '{table_name}' uses implicit target PK; not represented in v0alpha1"
                )));
            }
            let group = fks
                .entry(id)
                .or_insert_with(|| (target.clone(), BTreeMap::new()));
            if group.0 != target || group.1.insert(seq, (from, to)).is_some() {
                return Err(error(format!(
                    "inconsistent SQLite foreign key metadata on '{table_name}'"
                )));
            }
        }
        drop(fk_rows);

        let foreign_keys = fks
            .into_iter()
            .map(|(id, (target, cols))| ForeignKey {
                name: format!("__sqlite_fk_{id}"),
                columns: cols.values().map(|(from, _)| from.clone()).collect(),
                references_table: target,
                references_columns: cols.values().map(|(_, to)| to.clone()).collect(),
            })
            .collect();

        tables.push(SchemaTable {
            name: table_name.clone(),
            columns,
            primary_key: primary_key_columns.into_values().collect(),
            indexes,
            foreign_keys,
        });
    }

    let schema = RelationalSchema {
        version: RELATIONAL_SCHEMA_VERSION.to_string(),
        dialect: SchemaDialect::Sqlite,
        tables,
    };
    schema.validate()?;
    let version_after = schema_version(conn).await?;
    if version_before != version_after {
        return Err(error(
            "SQLite schema changed during catalog inspection; retry using a stable snapshot",
        ));
    }
    Ok(SqliteInspection { schema })
}

async fn schema_version(conn: &libsql::Connection) -> Result<i64, SchemaContractError> {
    let mut rows = conn
        .query("PRAGMA schema_version", ())
        .await
        .map_err(db_error)?;
    let row = rows
        .next()
        .await
        .map_err(db_error)?
        .ok_or_else(|| error("SQLite schema_version returned no row"))?;
    row.get(0).map_err(db_error)
}

fn db_error(err: impl std::fmt::Display) -> SchemaContractError {
    error(format!("SQLite introspection query failed: {err}"))
}

fn error(err: impl Into<String>) -> SchemaContractError {
    SchemaContractError(err.into())
}
