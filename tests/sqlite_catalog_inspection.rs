#![cfg(feature = "sqlite")]

use nulang::database::schema_contract::SchemaDialect;
use nulang::database::sqlite_inspection::inspect_sqlite;

async fn connection() -> libsql::Connection {
    let db = libsql::Builder::new_local(":memory:")
        .build()
        .await
        .expect("in-memory sqlite opens");
    db.connect().expect("sqlite connection")
}

#[tokio::test]
async fn inspects_columns_primary_keys_indexes_and_references() {
    let conn = connection().await;
    conn.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT NOT NULL, state TEXT DEFAULT 'new');\
         CREATE UNIQUE INDEX users_email_unique ON users(email);\
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id));"
    ).await.unwrap();

    let inspected = inspect_sqlite(&conn).await.unwrap();
    assert_eq!(inspected.schema.dialect, SchemaDialect::Sqlite);
    let users = inspected
        .schema
        .tables
        .iter()
        .find(|t| t.name == "users")
        .unwrap();
    assert_eq!(users.primary_key, ["id"]);
    assert_eq!(users.columns.len(), 3);
    assert!(!inspected.schema.column("users", "email").unwrap().nullable);
    assert_eq!(
        inspected
            .schema
            .column("users", "state")
            .unwrap()
            .default_sql
            .as_deref(),
        Some("'new'")
    );
    assert!(users
        .indexes
        .iter()
        .any(|i| i.unique && i.columns == ["email"]));
    let orders = inspected
        .schema
        .tables
        .iter()
        .find(|t| t.name == "orders")
        .unwrap();
    assert_eq!(orders.foreign_keys.len(), 1);
    assert_eq!(orders.foreign_keys[0].columns, ["user_id"]);
    assert_eq!(orders.foreign_keys[0].references_table, "users");
    assert_eq!(orders.foreign_keys[0].references_columns, ["id"]);
    assert_eq!(
        inspected.schema.to_json().unwrap(),
        inspected.schema.to_json().unwrap()
    );
    assert!(
        inspected.require_complete().is_err(),
        "v0alpha1 omits important catalog features"
    );
}

#[tokio::test]
async fn captures_composite_foreign_key_order() {
    let conn = connection().await;
    conn.execute_batch(
        "CREATE TABLE parents (a TEXT, b TEXT, PRIMARY KEY(a,b));\
         CREATE TABLE children (a TEXT, b TEXT, FOREIGN KEY(a,b) REFERENCES parents(a,b));",
    )
    .await
    .unwrap();
    let inspection = inspect_sqlite(&conn).await.unwrap();
    let children = inspection
        .schema
        .tables
        .iter()
        .find(|t| t.name == "children")
        .unwrap();
    assert_eq!(children.foreign_keys[0].columns, ["a", "b"]);
    assert_eq!(children.foreign_keys[0].references_columns, ["a", "b"]);
}

#[tokio::test]
async fn rejects_unsupported_partial_and_expression_indexes() {
    let conn = connection().await;
    conn.execute_batch(
        "CREATE TABLE users (email TEXT);\
         CREATE INDEX only_email ON users(email) WHERE email IS NOT NULL;",
    )
    .await
    .unwrap();
    let err = inspect_sqlite(&conn).await.unwrap_err();
    assert!(err.to_string().contains("partial"), "{err}");

    conn.execute("DROP INDEX only_email", ()).await.unwrap();
    conn.execute("CREATE INDEX lower_email ON users(lower(email))", ())
        .await
        .unwrap();
    let err = inspect_sqlite(&conn).await.unwrap_err();
    assert!(err.to_string().contains("expression"), "{err}");
}

#[tokio::test]
async fn rejects_unrepresented_views_triggers_and_generated_columns() {
    let conn = connection().await;
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER); CREATE VIEW user_ids AS SELECT id FROM users;",
    )
    .await
    .unwrap();
    let err = inspect_sqlite(&conn).await.unwrap_err();
    assert!(err.to_string().contains("view"), "{err}");

    conn.execute("DROP VIEW user_ids", ()).await.unwrap();
    conn.execute(
        "CREATE TRIGGER audit_insert AFTER INSERT ON users BEGIN SELECT 1; END",
        (),
    )
    .await
    .unwrap();
    let err = inspect_sqlite(&conn).await.unwrap_err();
    assert!(err.to_string().contains("trigger"), "{err}");

    conn.execute("DROP TRIGGER audit_insert", ()).await.unwrap();
    conn.execute(
        "CREATE TABLE augmented(a INT, b INT GENERATED ALWAYS AS (a+1) STORED)",
        (),
    )
    .await
    .unwrap();
    let err = inspect_sqlite(&conn).await.unwrap_err();
    assert!(err.to_string().contains("generated"), "{err}");
}

#[tokio::test]
async fn safely_reads_quoted_names_without_running_any_sql_from_names() {
    let conn = connection().await;
    conn.execute(
        "CREATE TABLE \"what's; DROP TABLE users;--\" (id TEXT NOT NULL)",
        (),
    )
    .await
    .unwrap();
    let inspected = inspect_sqlite(&conn).await.unwrap();
    assert!(inspected
        .schema
        .column("what's; DROP TABLE users;--", "id")
        .is_some());
}

#[tokio::test]
async fn empty_database_remains_partial_and_deterministic() {
    let conn = connection().await;
    let first = inspect_sqlite(&conn).await.unwrap();
    let second = inspect_sqlite(&conn).await.unwrap();
    assert!(first.schema.tables.is_empty());
    assert_eq!(
        first.schema.fingerprint().unwrap(),
        second.schema.fingerprint().unwrap()
    );
    assert!(!first.coverage_is_complete());
}

#[tokio::test]
async fn rejects_unrepresented_foreign_key_actions_and_index_ordering() {
    let conn = connection().await;
    conn.execute_batch(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, email TEXT);\
         CREATE TABLE orders(user_id INTEGER REFERENCES users(id) ON DELETE CASCADE);"
    ).await.unwrap();
    let error = inspect_sqlite(&conn).await.unwrap_err();
    assert!(error.to_string().contains("action"), "{error}");

    conn.execute("DROP TABLE orders", ()).await.unwrap();
    conn.execute("CREATE INDEX users_email_desc ON users(email DESC)", ()).await.unwrap();
    let error = inspect_sqlite(&conn).await.unwrap_err();
    assert!(error.to_string().contains("descending"), "{error}");

    conn.execute("DROP INDEX users_email_desc", ()).await.unwrap();
    conn.execute("CREATE INDEX users_email_nocase ON users(email COLLATE NOCASE)", ()).await.unwrap();
    let error = inspect_sqlite(&conn).await.unwrap_err();
    assert!(error.to_string().contains("collation"), "{error}");
}

#[tokio::test]
async fn rejects_strict_and_without_rowid_table_modes() {
    let conn = connection().await;
    conn.execute("CREATE TABLE strict_users(id INTEGER PRIMARY KEY) STRICT", ()).await.unwrap();
    let error = inspect_sqlite(&conn).await.unwrap_err();
    assert!(error.to_string().contains("STRICT"), "{error}");

    conn.execute("DROP TABLE strict_users", ()).await.unwrap();
    conn.execute("CREATE TABLE no_rowid(id TEXT PRIMARY KEY) WITHOUT ROWID", ()).await.unwrap();
    let error = inspect_sqlite(&conn).await.unwrap_err();
    assert!(error.to_string().contains("WITHOUT ROWID"), "{error}");
}

#[tokio::test]
async fn does_not_hide_user_tables_whose_names_start_with_sqlite_but_not_sqlite_underscore() {
    let conn = connection().await;
    conn.execute("CREATE TABLE sqlitearchive(id INTEGER)", ()).await.unwrap();
    let inspection = inspect_sqlite(&conn).await.unwrap();
    assert!(inspection.schema.column("sqlitearchive", "id").is_some());
}
