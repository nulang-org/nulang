//! Public API regression: disallow credentialed non-loopback plaintext before I/O.
#![cfg(feature = "sqlite")]

use std::io;

use nulang::runtime::LibsqlStore;

#[test]
fn remote_urls_reject_unsafe_destinations_before_network_access() {
    const TOKEN: &str = "opaque-bearer-secret-never-display";
    for uri in [
        "http://192.0.2.10:0",
        "http://0.0.0.0:0",
        "http://localhost.evil.invalid:0",
        "http://127.0.0.1@192.0.2.10:0",
        "http://127.0.0.1.evil.invalid:0",
        "https://user:password@database.turso.io",
        "https://database.turso.io#ignored",
        "javascript:alert(1)",
        "ftp://database.turso.io",
    ] {
        let error = match LibsqlStore::new_remote(uri, TOKEN) {
            Ok(_) => panic!("rejected endpoint {uri:?} unexpectedly accepted"),
            Err(error) => error,
        };
        assert_eq!(
            error.kind(),
            io::ErrorKind::InvalidInput,
            "{uri:?} must be rejected before network activity: {error}"
        );
        assert!(!error.to_string().contains(TOKEN));
        assert!(!error.to_string().contains("password"));
    }
}
