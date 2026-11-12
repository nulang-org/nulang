//! Database-backed conformance for the opt-in SQLite query executor.
#![cfg(feature = "sqlite")]

use nulang::query::{Catalog, Column, DataType, Expr, Plan, Row, Scalar};
use nulang::query_sqlite::{execute_sqlite, QueryLimits};

fn catalog() -> Catalog {
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

async fn database() -> (libsql::Database, libsql::Connection) {
    let db = libsql::Builder::new_local(":memory:")
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE \"u\"\"sers\" (age INTEGER NOT NULL, name TEXT NOT NULL, active BOOLEAN NOT NULL)",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO \"u\"\"sers\" VALUES (?, ?, ?)",
        libsql::params![20, "Robert'); DROP TABLE users; --", 1],
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO \"u\"\"sers\" VALUES (?, ?, ?)",
        libsql::params![25, "Ada", 0],
    )
    .await
    .unwrap();
    (db, conn)
}

fn limits() -> QueryLimits {
    QueryLimits {
        max_rows: 10,
        max_text_bytes: 4096,
    }
}

#[tokio::test]
async fn executes_bound_select_and_decodes_real_sqlite_values() {
    let (_db, conn) = database().await;
    let query = Plan::scan("u\"sers")
        .filter(Expr::eq(
            Expr::field("name"),
            Expr::text("Robert'); DROP TABLE users; --"),
        ))
        .sort("age", false)
        .project(vec![
            ("name", Expr::field("name")),
            ("adult", Expr::gt(Expr::field("age"), Expr::int(18))),
        ])
        .limit(2);
    let rows = execute_sqlite(&conn, &query, &catalog(), limits())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let expected: Row = [
        (
            "name".into(),
            Scalar::Text("Robert'); DROP TABLE users; --".into()),
        ),
        ("adult".into(), Scalar::Bool(true)),
    ]
    .into();
    assert_eq!(rows[0], expected);
    let mut check = conn
        .query("SELECT count(*) FROM \"u\"\"sers\"", ())
        .await
        .unwrap();
    assert_eq!(
        check.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        2
    );
}

#[tokio::test]
async fn enforces_max_rows_without_returning_partial_results() {
    let (_db, conn) = database().await;
    let error = execute_sqlite(
        &conn,
        &Plan::scan("u\"sers").sort("age", false),
        &catalog(),
        QueryLimits {
            max_rows: 1,
            max_text_bytes: 1000,
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("row limit"), "{error}");
}

#[tokio::test]
async fn enforces_total_text_budget() {
    let (_db, conn) = database().await;
    let error = execute_sqlite(
        &conn,
        &Plan::scan("u\"sers"),
        &catalog(),
        QueryLimits {
            max_rows: 10,
            max_text_bytes: 3,
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("text"), "{error}");
}

#[tokio::test]
async fn rejects_schema_drift_before_query() {
    let (_db, conn) = database().await;
    let mut wrong = Catalog::new();
    wrong
        .register_schema(
            "u\"sers",
            vec![
                Column::new("age", DataType::Text),
                Column::new("name", DataType::Text),
                Column::new("active", DataType::Bool),
            ],
        )
        .unwrap();
    let error = execute_sqlite(&conn, &Plan::scan("u\"sers"), &wrong, limits())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("schema"), "{error}");

    let mut extra = catalog();
    extra
        .register_schema("not_the_table", vec![Column::new("id", DataType::Int)])
        .unwrap();
    let error = execute_sqlite(&conn, &Plan::scan("not_the_table"), &extra, limits())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("schema") || error.to_string().contains("table"),
        "{error}"
    );
}

#[tokio::test]
async fn rejects_nullable_columns_and_invalid_boolean() {
    let db = libsql::Builder::new_local(":memory:")
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE events (age INTEGER, name TEXT, active BOOLEAN)",
        (),
    )
    .await
    .unwrap();
    let mut catalog = Catalog::new();
    catalog
        .register_schema(
            "events",
            vec![
                Column::new("age", DataType::Int),
                Column::new("name", DataType::Text),
                Column::new("active", DataType::Bool),
            ],
        )
        .unwrap();
    conn.execute("INSERT INTO events VALUES (1, NULL, 1)", ())
        .await
        .unwrap();
    let error = execute_sqlite(&conn, &Plan::scan("events"), &catalog, limits())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("nullable"), "{error}");
    conn.execute("DROP TABLE events", ()).await.unwrap();
    conn.execute(
        "CREATE TABLE events (age INTEGER NOT NULL, name TEXT NOT NULL, active BOOLEAN NOT NULL)",
        (),
    )
    .await
    .unwrap();
    conn.execute("INSERT INTO events VALUES (1, 'x', 2)", ())
        .await
        .unwrap();
    let error = execute_sqlite(&conn, &Plan::scan("events"), &catalog, limits())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Bool"), "{error}");
}

#[tokio::test]
async fn rejects_sqlite_integer_outside_nulang_int48_range() {
    let db = libsql::Builder::new_local(":memory:")
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE values_table (v INTEGER NOT NULL)", ())
        .await
        .unwrap();
    conn.execute("INSERT INTO values_table VALUES (9223372036854775807)", ())
        .await
        .unwrap();
    let mut catalog = Catalog::new();
    catalog
        .register_schema("values_table", vec![Column::new("v", DataType::Int)])
        .unwrap();
    let error = execute_sqlite(&conn, &Plan::scan("values_table"), &catalog, limits())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Int48"), "{error}");
}

#[tokio::test]
async fn default_nocase_collation_cannot_change_query_ir_text_equality() {
    let db = libsql::Builder::new_local(":memory:")
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE people (name TEXT COLLATE NOCASE NOT NULL)",
        (),
    )
    .await
    .unwrap();
    conn.execute("INSERT INTO people VALUES ('MiXeD')", ())
        .await
        .unwrap();
    let mut catalog = Catalog::new();
    catalog
        .register_schema("people", vec![Column::new("name", DataType::Text)])
        .unwrap();
    let plan = Plan::scan("people").filter(Expr::eq(Expr::field("name"), Expr::text("mixed")));
    let rows = execute_sqlite(&conn, &plan, &catalog, limits())
        .await
        .unwrap();
    assert!(rows.is_empty(), "Rust String equality is case-sensitive");
}

#[tokio::test]
async fn rejects_nullable_source_columns_even_when_null_rows_would_be_filtered_out() {
    let db = libsql::Builder::new_local(":memory:")
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE nullable_events (age INTEGER, name TEXT)", ())
        .await
        .unwrap();
    conn.execute("INSERT INTO nullable_events VALUES (NULL, 'hidden')", ())
        .await
        .unwrap();
    let mut catalog = Catalog::new();
    catalog
        .register_schema(
            "nullable_events",
            vec![
                Column::new("age", DataType::Int),
                Column::new("name", DataType::Text),
            ],
        )
        .unwrap();
    let plan = Plan::scan("nullable_events").filter(Expr::gt(Expr::field("age"), Expr::int(18)));
    let err = execute_sqlite(&conn, &plan, &catalog, limits())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("nullable"), "{err}");
}

#[tokio::test]
async fn desc_integer_primary_key_is_not_proven_non_null_by_pragma_pk_flag() {
    // SQLite's historical "INTEGER PRIMARY KEY DESC" syntax is not a
    // rowid alias and permits NULL despite pragma_table_xinfo.pk == 1.
    let db = libsql::Builder::new_local(":memory:")
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "CREATE TABLE strange (id INTEGER PRIMARY KEY DESC, label TEXT NOT NULL)",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO strange (id, label) VALUES (NULL, 'hidden')",
        (),
    )
    .await
    .unwrap();
    let mut catalog = Catalog::new();
    catalog
        .register_schema(
            "strange",
            vec![
                Column::new("id", DataType::Int),
                Column::new("label", DataType::Text),
            ],
        )
        .unwrap();
    let filtered = Plan::scan("strange").filter(Expr::gt(Expr::field("id"), Expr::int(0)));
    let error = execute_sqlite(&conn, &filtered, &catalog, limits())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("nullable"), "{error}");
}
