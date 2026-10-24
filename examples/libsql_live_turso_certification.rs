//! Guarded disposable Turso certification. The regular test suite only checks
//! configuration safety, without making remote requests. To run actual SQL:
//! NULANG_TURSO_TEST_IS_DISPOSABLE=I_CONFIRM_THIS_DB_MAY_BE_MODIFIED \
//! NULANG_TURSO_TEST_URL=libsql://<disposable-db>.turso.io \
//! NULANG_TURSO_TEST_TOKEN=<short-lived-scoped-token> \
//! cargo run --locked --features sqlite --example libsql_live_turso_certification
//! The constructor creates schema tables: never point this at production.
#![cfg(feature = "sqlite")]

#[cfg(not(test))]
use nulang::runtime::LibsqlStore;

const ACKNOWLEDGEMENT: &str = "I_CONFIRM_THIS_DB_MAY_BE_MODIFIED";

fn validate_target(ack: &str, raw_url: &str, token: &str) -> Result<(), &'static str> {
    if ack != ACKNOWLEDGEMENT {
        return Err("a disposable database must be explicitly acknowledged");
    }
    if token.trim().is_empty() {
        return Err("an authentication token is required");
    }
    if raw_url != raw_url.trim() || raw_url.chars().any(char::is_control) {
        return Err("invalid Turso endpoint");
    }
    let endpoint = url::Url::parse(raw_url).map_err(|_| "invalid Turso endpoint")?;
    // URL normalization erases explicit default ports (such as :443).
    // Check the original authority as well, not just Url::port().
    let authority = raw_url
        .split_once("://")
        .map(|(_, suffix)| suffix.split(['/', '?', '#']).next().unwrap_or_default())
        .unwrap_or_default();
    if !matches!(endpoint.scheme(), "https" | "libsql")
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.port().is_some()
        || authority.contains(':')
        || authority.contains('@')
        || !matches!(endpoint.path(), "" | "/")
        || !endpoint
            .host_str()
            .is_some_and(|host| host.len() > ".turso.io".len() && host.ends_with(".turso.io"))
    {
        return Err("certification requires an unambiguous Turso cloud HTTPS endpoint");
    }
    Ok(())
}

#[test]
fn test_certification_configuration_rejects_unsafe_inputs_before_network_io() {
    for (ack, url, token) in [
        (
            ACKNOWLEDGEMENT,
            "https://@sandbox.example.turso.io",
            "test-token",
        ),
        (
            ACKNOWLEDGEMENT,
            "libsql://@sandbox.example.turso.io",
            "test-token",
        ),
        ("", "libsql://sandbox.example.turso.io", "test-token"),
        ("yes", "libsql://sandbox.example.turso.io", "test-token"),
        (
            ACKNOWLEDGEMENT,
            "http://sandbox.example.turso.io",
            "test-token",
        ),
        (
            ACKNOWLEDGEMENT,
            "libsql://sandbox.example.turso.io.evil.invalid",
            "test-token",
        ),
        (
            ACKNOWLEDGEMENT,
            "libsql://alice:pass@sandbox.example.turso.io",
            "test-token",
        ),
        (
            ACKNOWLEDGEMENT,
            "libsql://sandbox.example.turso.io#secret",
            "test-token",
        ),
        (ACKNOWLEDGEMENT, "libsql://sandbox.example.turso.io", ""),
        (
            ACKNOWLEDGEMENT,
            "libsql://sandbox.example.turso.io?auth=secret",
            "test-token",
        ),
        (
            ACKNOWLEDGEMENT,
            "https://sandbox.example.turso.io:443",
            "test-token",
        ),
        (
            ACKNOWLEDGEMENT,
            "libsql://sandbox.example.turso.io/another",
            "test-token",
        ),
        (
            ACKNOWLEDGEMENT,
            "libsql://sandbox.example.turso.io ",
            "test-token",
        ),
    ] {
        assert!(
            validate_target(ack, url, token).is_err(),
            "unsafe configuration was accepted"
        );
    }
}

#[test]
fn test_certification_configuration_accepts_explicit_disposable_turso_target() {
    assert!(validate_target(
        ACKNOWLEDGEMENT,
        "libsql://sandbox.example.turso.io",
        "test-token"
    )
    .is_ok());
    assert!(validate_target(
        ACKNOWLEDGEMENT,
        "https://sandbox.example.turso.io",
        "test-token"
    )
    .is_ok());
}

/// Only the explicit cargo run --example invokes remote SQL; no test is ignored.
#[cfg(not(test))]
fn main() {
    let ack = std::env::var("NULANG_TURSO_TEST_IS_DISPOSABLE").unwrap_or_default();
    let endpoint = std::env::var("NULANG_TURSO_TEST_URL").unwrap_or_default();
    let token = std::env::var("NULANG_TURSO_TEST_TOKEN").unwrap_or_default();

    validate_target(&ack, &endpoint, &token)
        .expect("only an explicitly acknowledged disposable Turso DB is permitted");

    // new_remote creates Nulang's schema tables; never invoke on production.
    // Do not print the token or echo libsql's potentially sensitive errors.
    let store = LibsqlStore::new_remote(&endpoint, &token)
        .unwrap_or_else(|_| panic!("authenticated HTTPS/libSQL connection failed"));
    let rows = store
        .query("SELECT 1", &[])
        .unwrap_or_else(|_| panic!("authenticated remote SQL SELECT failed"));
    assert_eq!(rows, vec!["[1]"]);

    let rows = store
        .query(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'durable_outbox'",
            &[],
        )
        .unwrap_or_else(|_| panic!("schema verification query failed"));
    assert_eq!(rows, vec!["[1]"]);
    println!("Disposable Turso HTTPS/libSQL round-trip and schema certification passed");
}
