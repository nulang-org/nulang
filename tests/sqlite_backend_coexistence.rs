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
    // Run from an active Tokio runtime: the synchronous store must not nest
    // block_on on this thread or initialize a second SQLite implementation.
    let store = SqliteStore::open(&dir).expect("AI agent store must initialize its schema");
    assert_eq!(store.db_path(), dir.join("agents.db"));
    assert!(store.list_goals().unwrap().is_empty());

    // The agent schema must be ordinary SQLite and readable through the same
    // libSQL backend used by the job queue, not a separate bundled driver.
    let path = store.db_path().to_string_lossy().into_owned();
    let agent_db = libsql::Builder::new_local(&path).build().await.unwrap();
    let agent_conn = agent_db.connect().unwrap();
    let mut agent_rows = agent_conn
        .query("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('goals','tasks','conversations')", ())
        .await.unwrap();
    assert_eq!(
        agent_rows
            .next()
            .await
            .unwrap()
            .unwrap()
            .get::<i64>(0)
            .unwrap(),
        3
    );
    drop(agent_rows);
    drop(agent_conn);
    drop(agent_db);

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
