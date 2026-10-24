//! SQLite-backed web data foundation: explicit tenant scoping and restart durability.
//! This tests the Rust host storage API, not authenticated HTTP exposure.

#![cfg(feature = "sqlite")]

use nulang::runtime::WebSqliteStore;
use std::path::PathBuf;

fn fresh_db(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "nulang_web_sqlite_{}_{}_{}.db",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    path
}

fn remove_db(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}

#[test]
fn web_sqlite_crud_survives_close_and_reopen() {
    let path = fresh_db("reopen");
    {
        let store = WebSqliteStore::open(&path).unwrap();
        assert_eq!(store.get("account-a", "item:1").unwrap(), None);
        store.put("account-a", "item:1", "first").unwrap();
        assert_eq!(
            store.get("account-a", "item:1").unwrap(),
            Some("first".into())
        );
        store.put("account-a", "item:1", "updated").unwrap();
        store.put("account-a", "item:2", "").unwrap();
        assert_eq!(
            store.list("account-a", 10).unwrap(),
            vec![
                ("item:1".into(), "updated".into()),
                ("item:2".into(), "".into())
            ]
        );
    }
    {
        let store = WebSqliteStore::open(&path).unwrap();
        assert_eq!(
            store.get("account-a", "item:1").unwrap(),
            Some("updated".into())
        );
        assert_eq!(store.get("account-a", "item:2").unwrap(), Some("".into()));
        assert!(store.delete("account-a", "item:1").unwrap());
        assert!(!store.delete("account-a", "item:1").unwrap());
    }
    {
        let store = WebSqliteStore::open(&path).unwrap();
        assert_eq!(store.get("account-a", "item:1").unwrap(), None);
        assert_eq!(store.get("account-a", "item:2").unwrap(), Some("".into()));
    }
    remove_db(&path);
}

#[test]
fn web_sqlite_enforces_tenant_key_isolation_and_parameterized_literals() {
    let path = fresh_db("isolation");
    let store = WebSqliteStore::open(&path).unwrap();
    let dangerous_key = "x' OR 1=1 --";
    store.put("account-a", "item:1", "A").unwrap();
    store.put("account-b", "item:1", "B").unwrap();
    store.put("account-a", dangerous_key, "literal").unwrap();

    assert_eq!(store.get("account-a", "item:1").unwrap(), Some("A".into()));
    assert_eq!(store.get("account-b", "item:1").unwrap(), Some("B".into()));
    assert_eq!(store.get("account-b", dangerous_key).unwrap(), None);
    assert_eq!(
        store.get("account-a", dangerous_key).unwrap(),
        Some("literal".into())
    );
    assert!(store.delete("account-b", "item:1").unwrap());
    assert_eq!(store.get("account-a", "item:1").unwrap(), Some("A".into()));
    assert_eq!(store.list("account-b", 10).unwrap(), vec![]);
    assert_eq!(store.list("account-a", 1).unwrap().len(), 1);
    drop(store);
    remove_db(&path);
}

#[test]
fn web_sqlite_rejects_empty_scope_and_unbounded_list() {
    let path = fresh_db("guard");
    let store = WebSqliteStore::open(&path).unwrap();
    assert!(store.get("", "item").is_err());
    assert!(store.get("  ", "item").is_err());
    assert!(store.put("", "item", "value").is_err());
    assert!(store.put("tenant", "", "value").is_err());
    assert!(store.delete("", "item").is_err());
    assert!(store.list("", 1).is_err());
    assert!(store.list("tenant", 0).is_err());
    assert!(store.list("tenant", 1001).is_err());
    drop(store);
    remove_db(&path);
}

#[test]
fn web_sqlite_refuses_ephemeral_sqlite_uri_and_oversized_values() {
    assert!(WebSqliteStore::open(":memory:").is_err());
    assert!(WebSqliteStore::open("file::memory:?cache=shared").is_err());

    let path = fresh_db("bounds");
    let store = WebSqliteStore::open(&path).unwrap();
    assert!(store.put("tenant", "item", &"x".repeat(1_048_577)).is_err());
    assert!(store.put("tenant", &"k".repeat(1025), "value").is_err());
    assert!(store.put(&"t".repeat(257), "item", "value").is_err());
    assert!(store.list("tenant", 1).unwrap().is_empty());
    drop(store);
    remove_db(&path);
}
