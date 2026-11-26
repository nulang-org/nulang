#![cfg(feature = "sqlite")]

use nulang::runtime::LibsqlStore;
use std::io::ErrorKind;

#[test]
fn remote_libsql_constructor_rejects_non_https_endpoints_before_connecting() {
    // The unified candidate retains remote HTTPS. Invalid/plaintext URLs
    // must still fail before constructing a transport or exposing a token.
    for endpoint in [
        "invalid-no-scheme",
        "http://example.invalid",
        "file:///tmp/test.db",
    ] {
        let failure = LibsqlStore::new_remote(endpoint, "unused-test-token")
            .err()
            .expect("insecure remote transport must not be started");
        assert_eq!(failure.kind(), ErrorKind::InvalidInput);
        assert!(failure.to_string().contains("HTTPS"));
        assert!(!failure.to_string().contains("unused-test-token"));
    }
}

#[test]
fn local_libsql_persistence_remains_available() {
    let store = LibsqlStore::in_memory().expect("local SQLite must keep working");
    assert_eq!(store.path().to_str(), Some(":memory:"));
}
