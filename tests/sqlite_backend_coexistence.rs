// Regression: the full CLI must not link two bundled sqlite3 C definitions.
// The local AI agent store uses system SQLite while LibsqlStore bundles libsql.
#![cfg(all(feature = "sqlite", feature = "ai-runtime"))]

use nulang_ai_local::SqliteStore;
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::test]
async fn ai_sqlite_store_and_libsql_can_operate_in_the_same_process() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-sqlite-coexist-{}-{unique}",
        std::process::id()
    ));
    let store = SqliteStore::open(&dir).expect("AI agent store must initialize its schema");
    assert_eq!(store.db_path(), dir.join("agents.db"));

    let db = libsql::Builder::new_local(":memory:")
        .build()
        .await
        .expect("embedded libsql must initialize");
    let conn = db.connect().expect("libsql connection");
    conn.execute("CREATE TABLE coexistence(id INTEGER PRIMARY KEY)", ())
        .await
        .unwrap();
    conn.execute("INSERT INTO coexistence(id) VALUES (7)", ())
        .await
        .unwrap();
    let mut rows = conn.query("SELECT id FROM coexistence", ()).await.unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(row.get::<i64>(0).unwrap(), 7);
    drop(rows);
    drop(conn);
    drop(db);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
