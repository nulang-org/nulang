use nulang::database::schema_contract::{
    RelationalSchema, SchemaColumn, SchemaDialect, SchemaTable, RELATIONAL_SCHEMA_VERSION,
};
use nulang::database::typed_sql::{prepare_select, SelectSpec, SqlParameter, SqlPredicate};

fn users(dialect: SchemaDialect) -> RelationalSchema {
    RelationalSchema {
        version: RELATIONAL_SCHEMA_VERSION.into(),
        dialect,
        tables: vec![SchemaTable {
            name: match dialect {
                SchemaDialect::Postgres => "public.users",
                SchemaDialect::Sqlite => "users",
            }
            .into(),
            columns: vec![
                SchemaColumn {
                    name: "id".into(),
                    sql_type: "integer".into(),
                    nullable: false,
                    default_sql: None,
                },
                SchemaColumn {
                    name: "email".into(),
                    sql_type: "text".into(),
                    nullable: false,
                    default_sql: None,
                },
                SchemaColumn {
                    name: "active".into(),
                    sql_type: if dialect == SchemaDialect::Postgres {
                        "boolean"
                    } else {
                        "INTEGER"
                    }
                    .into(),
                    nullable: false,
                    default_sql: None,
                },
                SchemaColumn {
                    name: "nickname".into(),
                    sql_type: "text".into(),
                    nullable: true,
                    default_sql: None,
                },
            ],
            primary_key: vec!["id".into()],
            indexes: vec![],
            foreign_keys: vec![],
        }],
    }
}

#[test]
fn postgres_select_quotes_identifiers_and_binds_filter_and_limit() {
    let schema = users(SchemaDialect::Postgres);
    let spec = SelectSpec {
        table: "public.users".into(),
        columns: vec!["id".into(), "nickname".into()],
        predicate: Some(SqlPredicate::Eq {
            column: "email".into(),
            value: SqlParameter::Text("x' OR 1=1--".into()),
        }),
        limit: 25,
    };
    let plan = prepare_select(&schema, &spec).unwrap();
    assert_eq!(
        plan.sql,
        r#"SELECT "id", "nickname" FROM "public"."users" WHERE "email" = $1 LIMIT $2"#
    );
    assert_eq!(
        plan.parameters,
        vec![
            SqlParameter::Text("x' OR 1=1--".into()),
            SqlParameter::Integer(25)
        ]
    );
    assert_eq!(plan.projection.len(), 2);
    assert!(plan.projection[1].nullable);
    assert!(!plan.sql.contains("1=1"));
}

#[test]
fn sqlite_select_uses_indexed_placeholders() {
    let schema = users(SchemaDialect::Sqlite);
    let plan = prepare_select(
        &schema,
        &SelectSpec {
            table: "users".into(),
            columns: vec!["email".into()],
            predicate: Some(SqlPredicate::Eq {
                column: "id".into(),
                value: SqlParameter::Integer(17),
            }),
            limit: 1,
        },
    )
    .unwrap();
    assert_eq!(
        plan.sql,
        r#"SELECT "email" FROM "users" WHERE "id" = ?1 LIMIT ?2"#
    );
    assert_eq!(plan.parameters.len(), 2);
}

#[test]
fn null_predicates_have_no_value_parameter_and_preserve_projection_nullability() {
    let schema = users(SchemaDialect::Postgres);
    let plan = prepare_select(
        &schema,
        &SelectSpec {
            table: "public.users".into(),
            columns: vec!["nickname".into()],
            predicate: Some(SqlPredicate::IsNull {
                column: "nickname".into(),
            }),
            limit: 5,
        },
    )
    .unwrap();
    assert_eq!(
        plan.sql,
        r#"SELECT "nickname" FROM "public"."users" WHERE "nickname" IS NULL LIMIT $1"#
    );
    assert_eq!(plan.parameters, vec![SqlParameter::Integer(5)]);
    assert!(plan.projection[0].nullable);
}

#[test]
fn unknown_tables_and_fields_fail_closed() {
    let schema = users(SchemaDialect::Postgres);
    let mut spec = SelectSpec {
        table: "public.missing".into(),
        columns: vec!["id".into()],
        predicate: None,
        limit: 10,
    };
    assert!(prepare_select(&schema, &spec).is_err());

    spec.table = "public.users".into();
    spec.columns = vec!["not_present".into()];
    assert!(prepare_select(&schema, &spec).is_err());

    spec.columns = vec!["id".into()];
    spec.predicate = Some(SqlPredicate::IsNotNull {
        column: "missing".into(),
    });
    assert!(prepare_select(&schema, &spec).is_err());
}

#[test]
fn invalid_parameter_types_and_unsupported_sql_types_are_rejected() {
    let schema = users(SchemaDialect::Postgres);
    let mut spec = SelectSpec {
        table: "public.users".into(),
        columns: vec!["id".into()],
        predicate: Some(SqlPredicate::Eq {
            column: "id".into(),
            value: SqlParameter::Text("17".into()),
        }),
        limit: 2,
    };
    assert!(prepare_select(&schema, &spec).is_err());

    spec.predicate = Some(SqlPredicate::Eq {
        column: "active".into(),
        value: SqlParameter::Integer(1),
    });
    assert!(prepare_select(&schema, &spec).is_err());

    let mut other = users(SchemaDialect::Postgres);
    other.tables[0].columns[0].sql_type = "money".into();
    spec.predicate = Some(SqlPredicate::Eq {
        column: "id".into(),
        value: SqlParameter::Integer(7),
    });
    assert!(prepare_select(&other, &spec).is_err());
}

#[test]
fn bounded_select_only_and_duplicate_projection_rejection() {
    let schema = users(SchemaDialect::Postgres);
    let mut spec = SelectSpec {
        table: "public.users".into(),
        columns: vec!["id".into()],
        predicate: None,
        limit: 0,
    };
    assert!(prepare_select(&schema, &spec).is_err());
    spec.limit = 1001;
    assert!(prepare_select(&schema, &spec).is_err());
    spec.limit = 10;
    spec.columns = vec!["id".into(), "id".into()];
    assert!(prepare_select(&schema, &spec).is_err());
    spec.columns.clear();
    assert!(prepare_select(&schema, &spec).is_err());
}

#[test]
fn sqlite_identifier_case_resolution_is_correct() {
    let schema = users(SchemaDialect::Sqlite);
    let plan = prepare_select(
        &schema,
        &SelectSpec {
            table: "USERS".into(),
            columns: vec!["EMAIL".into()],
            predicate: None,
            limit: 10,
        },
    )
    .unwrap();
    assert_eq!(plan.sql, r#"SELECT "email" FROM "users" LIMIT ?1"#);
}

#[test]
fn malicious_column_names_are_quoted_not_executed() {
    let mut schema = users(SchemaDialect::Postgres);
    schema.tables[0].columns.push(SchemaColumn {
        name: "x\"; DROP TABLE users; --".into(),
        sql_type: "text".into(),
        nullable: false,
        default_sql: None,
    });
    let plan = prepare_select(
        &schema,
        &SelectSpec {
            table: "public.users".into(),
            columns: vec!["x\"; DROP TABLE users; --".into()],
            predicate: None,
            limit: 1,
        },
    )
    .unwrap();
    assert_eq!(
        plan.sql,
        r#"SELECT "x""; DROP TABLE users; --" FROM "public"."users" LIMIT $1"#
    );
    assert_eq!(plan.parameters, vec![SqlParameter::Integer(1)]);
}

#[test]
fn schema_unknown_or_invalid_metadata_cannot_prepare_queries() {
    let mut schema = users(SchemaDialect::Postgres);
    schema.version = "nulang.relational-schema/v99".into();
    assert!(prepare_select(
        &schema,
        &SelectSpec {
            table: "public.users".into(),
            columns: vec!["id".into()],
            predicate: None,
            limit: 1,
        }
    )
    .is_err());
}
