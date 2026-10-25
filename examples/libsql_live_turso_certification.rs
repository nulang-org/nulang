//! Guarded disposable Turso certification. The regular test suite only checks
//! configuration safety, without making remote requests. To run actual SQL:
//! NULANG_TURSO_TEST_IS_DISPOSABLE=I_CONFIRM_THIS_DB_MAY_BE_MODIFIED \
//! NULANG_TURSO_TEST_URL=libsql://<disposable-db>.turso.io \
//! NULANG_TURSO_TEST_TOKEN=<short-lived-scoped-token> \
//! cargo run --locked --features sqlite --example libsql_live_turso_certification
//! The constructor creates schema tables and the probe persists one uniquely
//! keyed snapshot. Only use an isolated disposable database, never production.
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

// This probe exercises the public persistence API, not a raw SQL shortcut.
// The caller must reconnect before verifying to prove committed remote state.
// It leaves one uniquely keyed snapshot on the explicitly disposable database.
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

#[cfg(test)]
#[test]
fn local_certification_probe_survives_reopening_the_database() {
    let path = std::env::temp_dir().join(format!(
        "nulang-cert-probe-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let db_path = path.to_str().expect("UTF-8 test path");
    let mut writer = LibsqlStore::new(db_path).expect("open local libsql");
    let actor_id = 672_304;
    write_certification_probe(&mut writer, actor_id).expect("write isolated probe");
    drop(writer);
    let reader = LibsqlStore::new(db_path).expect("reopen local libsql");
    verify_certification_probe(&reader, actor_id).expect("read persisted probe");
    drop(reader);
    std::fs::remove_file(path).expect("remove disposable local database");
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

    // Force a fresh authenticated connection: an in-process read is not proof
    // the write was durably committed to the remote database.
    let reader = LibsqlStore::new_remote(&endpoint, &token)
        .unwrap_or_else(|_| panic!("remote SQL reconnection failed"));
    verify_certification_probe(&reader, actor_id)
        .expect("authenticated remote SQL snapshot recovery failed");
    println!("Disposable Turso HTTPS/libSQL authenticated write, reconnect, and read passed");
}
