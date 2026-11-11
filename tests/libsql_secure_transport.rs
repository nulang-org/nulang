#![cfg(feature = "sqlite")]

use nulang::runtime::LibsqlStore;
use std::io::ErrorKind;

#[test]
fn remote_libsql_constructor_fails_explicitly_without_insecure_tls() {
    // This deliberately uses an invalid URI so the old implementation
    // cannot successfully reach an external database during the RED step.
    let failure = LibsqlStore::new_remote("invalid-no-scheme", "unused-test-token")
        .err()
        .expect("remote transport must not be started");
    assert_eq!(failure.kind(), ErrorKind::Unsupported);
    assert!(
        failure.to_string().contains("RUSTSEC-2026-0382"),
        "risk must be clearly documented"
    );
}

#[test]
fn local_libsql_persistence_remains_available() {
    let store = LibsqlStore::in_memory().expect("local SQLite must keep working");
    assert_eq!(store.path().to_str(), Some(":memory:"));
}
