use nulang::database::schema_contract::{
    ForeignKey, Index, RelationalSchema, SchemaColumn, SchemaDialect, SchemaTable,
    RELATIONAL_SCHEMA_VERSION,
};

fn fixture() -> RelationalSchema {
    RelationalSchema {
        version: RELATIONAL_SCHEMA_VERSION.to_string(),
        dialect: SchemaDialect::Postgres,
        tables: vec![
            SchemaTable {
                name: "public.users".into(),
                columns: vec![
                    SchemaColumn { name: "id".into(), sql_type: "uuid".into(), nullable: false, default_sql: None },
                    SchemaColumn { name: "email".into(), sql_type: "text".into(), nullable: false, default_sql: None },
                ],
                primary_key: vec!["id".into()],
                indexes: vec![Index {
                    name: "users_email_unique".into(),
                    columns: vec!["email".into()],
                    unique: true,
                }],
                foreign_keys: vec![],
            },
            SchemaTable {
                name: "public.orders".into(),
                columns: vec![
                    SchemaColumn { name: "id".into(), sql_type: "uuid".into(), nullable: false, default_sql: None },
                    SchemaColumn { name: "user_id".into(), sql_type: "uuid".into(), nullable: false, default_sql: None },
                ],
                primary_key: vec!["id".into()],
                indexes: vec![],
                foreign_keys: vec![ForeignKey {
                    name: "orders_user_fk".into(),
                    columns: vec!["user_id".into()],
                    references_table: "public.users".into(),
                    references_columns: vec!["id".into()],
                }],
            },
        ],
    }
}

#[test]
fn order_insensitive_declarations_have_identical_fingerprints() {
    let original = fixture();
    let mut reordered = fixture();
    reordered.tables.reverse();
    reordered.tables[0].columns.reverse();
    assert_eq!(original.fingerprint().unwrap(), reordered.fingerprint().unwrap());
    assert_eq!(original.to_json().unwrap(), reordered.to_json().unwrap());
}

#[test]
fn schema_changes_invalidate_the_fingerprint() {
    let original = fixture();
    let mut changed = fixture();
    changed.tables[0].columns[1].nullable = true;
    assert_ne!(original.fingerprint().unwrap(), changed.fingerprint().unwrap());

    changed = fixture();
    changed.tables[0].indexes[0].unique = false;
    assert_ne!(original.fingerprint().unwrap(), changed.fingerprint().unwrap());

    changed = fixture();
    changed.tables[1].foreign_keys[0].references_columns = vec!["email".into()];
    assert_ne!(original.fingerprint().unwrap(), changed.fingerprint().unwrap());
}

#[test]
fn sqlite_identifier_collisions_are_rejected() {
    let mut schema = fixture();
    schema.dialect = SchemaDialect::Sqlite;
    schema.tables[0].columns.push(SchemaColumn {
        name: "EMAIL".into(),
        sql_type: "text".into(),
        nullable: false,
        default_sql: None,
    });
    assert!(schema.validate().is_err());
}

#[test]
fn invalid_primary_keys_and_foreign_keys_are_rejected() {
    let mut schema = fixture();
    schema.tables[0].primary_key = vec!["not_a_column".into()];
    assert!(schema.validate().is_err());

    schema = fixture();
    schema.tables[1].foreign_keys[0].references_table = "public.missing".into();
    assert!(schema.validate().is_err());

    schema = fixture();
    schema.tables[1].foreign_keys[0].references_columns.clear();
    assert!(schema.validate().is_err());
}

#[test]
fn unrecognized_contract_versions_and_unknown_fields_fail_closed() {
    let mut schema = fixture();
    schema.version = "nulang.relational-schema/v99".into();
    assert!(schema.validate().is_err());

    let mut json: serde_json::Value = serde_json::from_slice(&fixture().to_json().unwrap()).unwrap();
    json["unexpected"] = serde_json::json!(true);
    assert!(RelationalSchema::from_json(&serde_json::to_vec(&json).unwrap()).is_err());
}

#[test]
fn fingerprint_and_json_round_trip_are_stable() {
    let original = fixture();
    let encoded = original.to_json().unwrap();
    let decoded = RelationalSchema::from_json(&encoded).unwrap();
    assert_eq!(original.fingerprint().unwrap(), decoded.fingerprint().unwrap());
    assert!(decoded.fingerprint().unwrap().starts_with("blake3:"));
    assert_eq!(decoded.column("public.users", "email").unwrap().sql_type, "text");
}

#[test]
fn indexes_preserve_column_order_as_semantic() {
    let mut schema = fixture();
    schema.tables[0].indexes[0].columns = vec!["id".into(), "email".into()];
    let original = schema.fingerprint().unwrap();
    schema.tables[0].indexes[0].columns.reverse();
    assert_ne!(original, schema.fingerprint().unwrap());
}
