//! Security regression for RUSTSEC-2026-0459 (bundled SQLite WAL page-cache exposure).

use nulang_ai_core::Goal;
use nulang_ai_local::SqliteStore;
use uuid::Uuid;

#[test]
fn bundled_sqlite_is_patched_and_store_remains_persistent() {
    // This crate uses rusqlite's bundled feature, so its runtime SQLite
    // version must be at least 3.54.0 (the upstream WAL-page fix).
    let conn = rusqlite::Connection::open_in_memory().expect("open bundled SQLite");
    let version: String = conn
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .expect("query linked SQLite version");
    let parts: Vec<u32> = version
        .split('.')
        .map(|part| part.parse::<u32>().expect("numeric SQLite version"))
        .collect();
    assert!(
        parts.as_slice() >= [3, 54, 0].as_slice(),
        "SQLite {version} is affected by RUSTSEC-2026-0459"
    );

    // Exercise the real local AI store rather than trusting only Cargo.lock.
    let root = std::env::temp_dir().join(format!("nulang-ai-sqlite-patched-{}", Uuid::new_v4()));
    let store = SqliteStore::open(&root).expect("migrate new database");
    let goal = Goal::new("demo", "verify patched SQLite persistence", 1.0);
    store.upsert_goal(&goal).expect("persist goal");
    drop(store);
    let reopened = SqliteStore::open(&root).expect("reopen database");
    let goals = reopened.list_goals().expect("read persisted goal");
    assert!(goals.iter().any(|stored| stored.id == goal.id));
    drop(reopened);
    std::fs::remove_dir_all(root).expect("remove disposable database");
}
