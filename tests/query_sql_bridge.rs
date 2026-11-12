//! SQL lowering and compiler-type bridge contracts for the experimental Query IR.
//! These tests assert that SQL data is bound, identifiers are quoted, and
//! unsupported semantic rewrites fail closed instead of producing wrong SQL.
use nulang::query::{execute, infer_schema, Catalog, Column, DataType, Expr, Plan, Scalar};
use nulang::query_sql::{compile_prepared, SqlDialect};
use nulang::query_types::result_collection_type;
use nulang::types::{PrimitiveType, Type};

fn schema() -> Catalog {
    let mut catalog = Catalog::new();
    catalog
        .register_schema(
            "u\"sers",
            vec![
                Column::new("age", DataType::Int),
                Column::new("name", DataType::Text),
                Column::new("active", DataType::Bool),
            ],
        )
        .unwrap();
    catalog
}

#[test]
fn schemas_without_rows_support_typechecking_but_not_in_memory_execution() {
    let plan = Plan::scan("u\"sers");
    assert_eq!(infer_schema(&plan, &schema()).unwrap().len(), 3);
    let error = execute(&plan, &schema()).unwrap_err();
    assert!(error.to_string().contains("no in-memory rows"));
}

#[test]
fn compiler_bridge_creates_record_array_using_nulang_types() {
    let plan = Plan::scan("u\"sers").project(vec![
        ("person", Expr::field("name")),
        ("adult", Expr::gt(Expr::field("age"), Expr::int(18))),
    ]);
    assert_eq!(
        result_collection_type(&plan, &schema()).unwrap(),
        Type::Array(Box::new(Type::Record(vec![
            ("person".into(), Type::Primitive(PrimitiveType::String)),
            ("adult".into(), Type::Primitive(PrimitiveType::Bool)),
        ])))
    );
}

#[test]
fn sqlite_compiles_quoted_identifiers_and_binds_untrusted_literals() {
    let plan = Plan::scan("u\"sers")
        .filter(Expr::and(
            Expr::gt(Expr::field("age"), Expr::int(18)),
            Expr::eq(
                Expr::field("name"),
                Expr::text("Robert'); DROP TABLE users; --"),
            ),
        ))
        .sort("age", false)
        .project(vec![("person", Expr::field("name"))]);
    let statement = compile_prepared(&plan, &schema(), SqlDialect::Sqlite).unwrap();
    assert_eq!(
        statement.sql,
        "SELECT \"name\" AS \"person\" FROM \"u\"\"sers\" WHERE ((\"age\" > CAST(? AS INTEGER)) AND (\"name\" = CAST(? AS TEXT))) ORDER BY \"age\" ASC"
    );
    assert_eq!(
        statement.parameters,
        vec![
            Scalar::Int(18),
            Scalar::Text("Robert'); DROP TABLE users; --".into())
        ]
    );
    assert!(!statement.sql.contains("DROP TABLE"));
    assert_eq!(
        statement.output_columns,
        vec![Column::new("person", DataType::Text)]
    );
}

#[test]
fn postgres_uses_positioned_bind_markers_and_typed_parameters() {
    let plan = Plan::scan("u\"sers")
        .filter(Expr::eq(Expr::field("active"), Expr::bool(true)))
        .sort("age", true)
        .limit(7);
    let statement = compile_prepared(&plan, &schema(), SqlDialect::Postgres).unwrap();
    assert_eq!(
        statement.sql,
        "SELECT \"age\", \"name\", \"active\" FROM \"u\"\"sers\" WHERE (\"active\" = CAST($1 AS BOOLEAN)) ORDER BY \"age\" DESC LIMIT $2"
    );
    assert_eq!(
        statement.parameters,
        vec![Scalar::Bool(true), Scalar::Int(7)]
    );
}

#[test]
fn query_limiter_must_have_sort_for_remote_storage() {
    let err = compile_prepared(
        &Plan::scan("u\"sers").limit(1),
        &schema(),
        SqlDialect::Sqlite,
    )
    .unwrap_err();
    assert!(err.to_string().contains("ORDER BY"));
}

#[test]
fn unsafe_operator_order_and_text_collation_fail_closed() {
    let after_limit = Plan::scan("u\"sers")
        .sort("age", false)
        .limit(1)
        .filter(Expr::gt(Expr::field("age"), Expr::int(18)));
    assert!(compile_prepared(&after_limit, &schema(), SqlDialect::Postgres).is_err());

    let after_projection = Plan::scan("u\"sers")
        .project(vec![("age", Expr::field("age"))])
        .filter(Expr::gt(Expr::field("age"), Expr::int(18)));
    assert!(compile_prepared(&after_projection, &schema(), SqlDialect::Postgres).is_err());

    let text_sort = Plan::scan("u\"sers").sort("name", false);
    assert!(compile_prepared(&text_sort, &schema(), SqlDialect::Sqlite).is_err());
}

#[test]
fn invalid_expression_types_and_identifiers_never_reach_sql() {
    let bad = Plan::scan("u\"sers").filter(Expr::gt(Expr::field("age"), Expr::text("18")));
    assert!(compile_prepared(&bad, &schema(), SqlDialect::Postgres).is_err());
    let mut catalog = Catalog::new();
    catalog
        .register_schema("oops\0table", vec![Column::new("a", DataType::Int)])
        .unwrap();
    assert!(compile_prepared(&Plan::scan("oops\0table"), &catalog, SqlDialect::Postgres).is_err());
}

#[test]
fn schema_registration_rejects_duplicate_sources_and_columns() {
    let mut catalog = schema();
    assert!(catalog
        .register_schema("u\"sers", vec![Column::new("id", DataType::Int)])
        .is_err());
    assert!(catalog
        .register_schema(
            "duplicates",
            vec![
                Column::new("id", DataType::Int),
                Column::new("id", DataType::Text)
            ]
        )
        .is_err());
}

#[test]
fn select_literals_precede_where_parameters_in_both_dialects() {
    let plan = Plan::scan("u\"sers")
        .filter(Expr::gt(Expr::field("age"), Expr::int(21)))
        .sort("age", false)
        .project(vec![("label", Expr::text("hello?"))]);
    let sqlite = compile_prepared(&plan, &schema(), SqlDialect::Sqlite).unwrap();
    assert_eq!(
        sqlite.parameters,
        vec![Scalar::Text("hello?".into()), Scalar::Int(21),]
    );
    assert!(sqlite
        .sql
        .starts_with("SELECT CAST(? AS TEXT) AS \"label\""));
    assert!(sqlite.sql.contains("WHERE (\"age\" > CAST(? AS INTEGER))"));
    let postgres = compile_prepared(&plan, &schema(), SqlDialect::Postgres).unwrap();
    assert!(postgres
        .sql
        .starts_with("SELECT CAST($1 AS TEXT) AS \"label\""));
    assert!(postgres
        .sql
        .contains("WHERE (\"age\" > CAST($2 AS BIGINT))"));
}

#[test]
fn source_text_greater_than_fails_even_without_text_literal() {
    let plan = Plan::scan("u\"sers").filter(Expr::gt(Expr::field("name"), Expr::field("name")));
    assert!(compile_prepared(&plan, &schema(), SqlDialect::Sqlite).is_err());
    assert!(compile_prepared(&plan, &schema(), SqlDialect::Postgres).is_err());
}

#[test]
fn sorting_then_projecting_same_name_is_rejected_to_avoid_sql_alias_shadowing() {
    let plan = Plan::scan("u\"sers")
        .sort("age", false)
        .project(vec![("age", Expr::int(1))]);
    assert!(compile_prepared(&plan, &schema(), SqlDialect::Sqlite).is_err());
}
