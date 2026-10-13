//! SQLite-backed web state contract. These tests precede the implementation.
#![cfg(feature = "sqlite")]

use nulang::runtime::web_state::SqliteWebStore;
use std::path::PathBuf;

fn fresh_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_web_sqlite_{}_{}_{}.db",
        label,
        std::process::id(),
        std::thread::current().name().unwrap_or("test").replace([':', '/'], "_")
    ))
}

fn cleanup(path: &PathBuf) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}

#[test]
fn acknowledged_web_state_survives_close_and_reopen() {
    let path = fresh_path("restart");
    cleanup(&path);
    {
        let store = SqliteWebStore::open(&path).expect("sqlite opens");
        assert_eq!(store.get("customer").unwrap(), None);
        store.set("customer", "alpha").expect("committed write");
        assert_eq!(store.get("customer").unwrap().as_deref(), Some("alpha"));
    }
    {
        let store = SqliteWebStore::open(&path).expect("sqlite reopens");
        assert_eq!(store.get("customer").unwrap().as_deref(), Some("alpha"));
        store.delete("customer").expect("committed delete");
    }
    {
        let store = SqliteWebStore::open(&path).expect("sqlite reopens");
        assert_eq!(store.get("customer").unwrap(), None);
    }
    cleanup(&path);
}

#[test]
fn state_keys_are_parameters_not_sql_fragments() {
    let path = fresh_path("params");
    cleanup(&path);
    let store = SqliteWebStore::open(&path).unwrap();
    let dangerous = "key'); DROP TABLE web_kv;--";
    store.set(dangerous, "quote' \" & <").unwrap();
    assert_eq!(store.get(dangerous).unwrap(), Some("quote' \" & <".into()));
    store.set("other", "untouched").unwrap();
    assert_eq!(store.all().unwrap().len(), 2);
    drop(store);
    cleanup(&path);
}

#[test]
fn open_failure_is_explicit_and_never_creates_in_memory_fallback() {
    let dir = std::env::temp_dir().join(format!("nulang_web_missing_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let result = SqliteWebStore::open(dir.join("does-not-exist").join("web.db"));
    assert!(result.is_err(), "missing parent directory must be an error");
}
