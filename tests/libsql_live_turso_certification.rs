//! Manual, non-production Turso certification. Never runs in regular CI.
//! The LibsqlStore constructor creates tables: require explicit disposable-db
//! acknowledgement before making a network request.
#![cfg(feature = "sqlite")]

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
        || !matches!(endpoint.path(), "" | "/")
        || !endpoint
            .host_str()
            .is_some_and(|host| host.len() > ".turso.io".len() && host.ends_with(".turso.io"))
    {
        return Err("certification requires an unambiguous Turso cloud HTTPS endpoint");
    }
    Ok(())
}

// This table exists only on the explicitly acknowledged disposable target.
// Use a fixed, idempotent key so rerunning after a partial failure is safe.
const PROBE_CREATE: &str = "CREATE TABLE IF NOT EXISTS nulang_turso_tls_certification_probe (id INTEGER PRIMARY KEY, marker INTEGER NOT NULL)";
const PROBE_WRITE: &str =
    "INSERT OR REPLACE INTO nulang_turso_tls_certification_probe (id, marker) VALUES (1, 739101)";
const PROBE_READ: &str = "SELECT marker FROM nulang_turso_tls_certification_probe WHERE id = 1";
const PROBE_DROP: &str = "DROP TABLE IF EXISTS nulang_turso_tls_certification_probe";

fn query_without_leaking_credentials(store: &LibsqlStore, sql: &str, stage: &str) -> Vec<String> {
    store
        .query(sql, &[])
        .unwrap_or_else(|_| panic!("disposable Turso certification {stage} failed"))
}

fn write_probe(store: &LibsqlStore) {
    query_without_leaking_credentials(store, PROBE_CREATE, "table creation");
    query_without_leaking_credentials(store, PROBE_WRITE, "SQL write");
}

fn verify_and_remove_probe(store: &LibsqlStore) {
    assert_eq!(
        query_without_leaking_credentials(store, PROBE_READ, "SQL read"),
        vec!["[739101]"],
        "disposable SQL write was not visible to the verifying connection",
    );
    query_without_leaking_credentials(store, PROBE_DROP, "table cleanup");
}

/// Validate SQL DDL/DML and the LibsqlStore query interface without a remote
/// database, a credential, or changing the host machine's trust store.
#[test]
fn certification_probe_supports_real_sql_write_and_read_locally() {
    let store = LibsqlStore::in_memory().expect("isolated in-memory SQL store");
    write_probe(&store);
    verify_and_remove_probe(&store);
    assert_eq!(
        store.query("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'nulang_turso_tls_certification_probe'", &[])
            .expect("in-memory probe cleanup check"),
        vec!["[0]"],
    );
}

#[test]
fn certification_configuration_rejects_unsafe_inputs_before_network_io() {
    for (ack, url, token) in [
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
fn certification_configuration_accepts_explicit_disposable_turso_target() {
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

/// Runs only with operator-supplied credentials for an expendable remote DB.
/// Example:
/// NULANG_TURSO_TEST_IS_DISPOSABLE=I_CONFIRM_THIS_DB_MAY_BE_MODIFIED
/// NULANG_TURSO_TEST_URL=libsql://<disposable-instance>.turso.io
/// NULANG_TURSO_TEST_TOKEN=(secret, never log)
/// cargo test --locked --features sqlite --test libsql_live_turso_certification -- --ignored
#[test]
#[ignore = "requires explicitly confirmed disposable Turso DB and an auth token"]
fn authenticated_turso_sql_round_trip() {
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

    // A new remote connection must see an actual committed write. SELECT 1 and
    // schema creation alone are insufficient to prove remote durability.
    write_probe(&store);
    drop(store);
    let verifier = LibsqlStore::new_remote(&endpoint, &token)
        .unwrap_or_else(|_| panic!("authenticated verifying connection failed"));
    verify_and_remove_probe(&verifier);
}
