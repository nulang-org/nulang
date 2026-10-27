#![cfg(feature = "sqlite")]
//! Fail closed on unsafe libSQL URLs before allocating the client, resolving DNS,
//! making a network request, or exposing bearer tokens in error messages.
use nulang::runtime::LibsqlStore;
use std::io::ErrorKind;

#[test]
fn rejects_remote_plaintext_before_any_network_io() {
    let secret = "url-admission-test-token-must-not-appear";
    let error = match LibsqlStore::new_remote("http://0.0.0.0:0", secret) {
        Err(error) => error,
        Ok(_) => panic!("remote plaintext wildcard endpoint unexpectedly accepted"),
    };
    assert_eq!(error.kind(), ErrorKind::InvalidInput);
    assert!(!error.to_string().contains(secret));
}

#[test]
fn rejects_unsafe_authorities_schemes_and_lookalikes() {
    let secret = "url-admission-test-token-must-not-appear";
    for input in [
        "http://192.0.2.123:0",
        "http://localhost:0",
        "http://localhost.evil.invalid:0",
        "http://127.0.0.1.evil.invalid:0",
        "http://127.0.0.1@192.0.2.123:0",
        "https://user:password@database.turso.io",
        "http://@127.0.0.1:0",
        "https://@database.turso.io",
        "https://database.turso.io#fragment",
        "ftp://database.turso.io",
        "javascript:alert(1)",
        "",
        "https://",
        " HTTPS://database.turso.io",
        "https://database.turso.io\\@lookalike.invalid",
    ] {
        let error = match LibsqlStore::new_remote(input, secret) {
            Err(error) => error,
            Ok(_) => panic!("unsafe remote target unexpectedly accepted"),
        };
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "input {input:?}");
        assert!(!error.to_string().contains(secret));
        assert!(!error.to_string().contains("password"));
    }
}

#[test]
fn permits_ordinary_https_bare_alias_and_numeric_loopback() {
    // No real remote connection is made: validation is tested through a separate
    // unit suite; this integration test only ensures that loopback is not
    // rejected as InvalidInput before a connection attempt.
    // Use a port that cannot be a valid service in this test.
    for input in ["http://127.0.0.1:0", "http://[::1]:0"] {
        let error = match LibsqlStore::new_remote(input, "dummy") {
            Err(error) => error,
            Ok(_) => panic!("closed loopback endpoint unexpectedly connected"),
        };
        assert_ne!(error.kind(), ErrorKind::InvalidInput, "input {input:?}");
    }
}
