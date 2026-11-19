#![cfg(all(feature = "sqlite", unix))]

//! SQLite must preserve Unix pathname bytes across the provider migration.
//! Replacing invalid UTF-8 with U+FFFD silently redirects a durable store
//! to a different file, which is unsafe even when SQLite itself succeeds.

use nulang::runtime::LibsqlStore;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn sqlite_store_preserves_non_utf8_path_bytes() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang_sqlite_path_bytes_{}_{}",
        std::process::id(),
        now
    ));
    std::fs::create_dir(&dir).expect("isolated test directory");
    let filename = OsString::from_vec(b"actor-\xff.db".to_vec());
    let actual_path = dir.join(&filename);
    let lossy_path = dir.join("actor-\u{fffd}.db");

    {
        let store = LibsqlStore::new(&actual_path).expect("open SQLite using raw Unix path bytes");
        assert_eq!(store.path(), actual_path.as_path());
        assert!(
            actual_path.exists(),
            "SQLite must open the requested byte-exact filename, not a lossy replacement"
        );
        assert!(
            !lossy_path.exists(),
            "must not create a second, lossy database file"
        );
    }

    std::fs::remove_dir_all(dir).expect("remove isolated SQLite fixture");
}
