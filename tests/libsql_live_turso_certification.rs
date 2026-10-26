//! Manual, non-production Turso certification. Never runs in regular CI.
//! The LibsqlStore constructor creates tables: require explicit disposable-db
//! acknowledgement before making a network request.
#![cfg(feature = "sqlite")]

use nulang::runtime::{ActorSnapshot, LibsqlStore, PersistedValue, PersistenceStore};

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

// Verify actual committed Nulang persistence, not only SELECT 1 or schema
// creation. Keep the probe in the explicitly disposable database.
fn write_certification_probe(store: &mut LibsqlStore, actor_id: u64) -> Result<(), &'static str> {
    if actor_id > i64::MAX as u64 || store.load_snapshot(actor_id).is_some() {
        return Err("certification probe identifier is unsafe or already exists");
    }
    let mut snapshot = ActorSnapshot {
        actor_id,
        sequence: 1,
        ..ActorSnapshot::default()
    };
    snapshot.state.insert(
        "nulang_certification".to_string(),
        PersistedValue::String("verified".to_string()),
    );
    store
        .save_snapshot(snapshot)
        .map_err(|_| "authenticated SQL write failed")
}

fn verify_certification_probe(store: &LibsqlStore, actor_id: u64) -> Result<(), &'static str> {
    let snapshot = store
        .load_snapshot(actor_id)
        .ok_or("authenticated SQL recovery read failed")?;
    if snapshot.actor_id != actor_id
        || snapshot.sequence != 1
        || snapshot.state.get("nulang_certification")
            != Some(&PersistedValue::String("verified".to_string()))
    {
        return Err("persisted certification probe does not match the written data");
    }
    Ok(())
}

#[test]
fn certification_configuration_rejects_unsafe_inputs_before_network_io() {
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

#[test]
fn local_certification_probe_survives_reopening_database() {
    let path = std::env::temp_dir().join(format!(
        "nulang-cert-probe-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path_str = path.to_str().expect("UTF-8 local test path");
    let mut writer = LibsqlStore::new(path_str).expect("open local libsql");
    let actor_id = 672_304;
    write_certification_probe(&mut writer, actor_id).expect("write isolated snapshot");
    drop(writer);
    let reader = LibsqlStore::new(path_str).expect("reopen local libsql");
    verify_certification_probe(&reader, actor_id).expect("verify persisted snapshot");
    drop(reader);
    std::fs::remove_file(path).expect("remove local test DB");
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
    let mut store = LibsqlStore::new_remote(&endpoint, &token)
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

    let actor_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    let actor_id = u64::try_from(actor_id).expect("certification ID overflow");
    write_certification_probe(&mut store, actor_id)
        .expect("authenticated remote SQL snapshot write failed");
    drop(store);

    // New authenticated connection, not a cached result from the writer.
    let reader = LibsqlStore::new_remote(&endpoint, &token)
        .unwrap_or_else(|_| panic!("remote SQL reconnection failed"));
    verify_certification_probe(&reader, actor_id)
        .expect("authenticated remote SQL snapshot recovery failed");
}
