//! Opt-in HTTPS/libsql smoke for an explicitly disposable Turso database.
//! The constructor creates/migrates tables, and this test writes a snapshot.
//! NEVER point this test at a production or shared database.
#![cfg(feature = "sqlite")]

use nulang::runtime::{ActorSnapshot, LibsqlStore, PersistedValue, PersistenceStore};
use std::time::{SystemTime, UNIX_EPOCH};

const CONFIRMATION: &str = "DISPOSABLE_DB_SCHEMA_CHANGES_OK";

fn approve_disposable_target(raw: &str, confirmation: &str) -> Result<(), &'static str> {
    if confirmation != CONFIRMATION {
        return Err("explicit disposable-database acknowledgement required");
    }
    let url = url::Url::parse(raw).map_err(|_| "invalid Turso smoke URL")?;
    if !matches!(url.scheme(), "https" | "libsql")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("smoke URL must use HTTPS or libsql with no embedded credentials");
    }
    let host = url.host_str().ok_or("smoke URL has no hostname")?;
    if !host.starts_with("nulang-smoke-") || !host.ends_with(".turso.io") {
        return Err("smoke target must be a dedicated nulang-smoke-* Turso database");
    }
    Ok(())
}

#[test]
fn remote_smoke_is_denied_without_an_explicit_disposable_target() {
    const TEST_URL: &str = "libsql://nulang-smoke-ci-example.turso.io";
    assert!(approve_disposable_target(TEST_URL, "").is_err());
    assert!(approve_disposable_target(TEST_URL, "yes").is_err());
    assert!(approve_disposable_target(TEST_URL, CONFIRMATION).is_ok());
    for uri in [
        "http://nulang-smoke-ci-example.turso.io",
        "https://production-example.turso.io",
        "https://nulang-smoke-ci-example.turso.io.evil.invalid",
        "https://user:password@nulang-smoke-ci-example.turso.io",
        "https://nulang-smoke-ci-example.turso.io#fragment",
        "https://nulang-smoke-ci-example.turso.io?token=secret",
        "libsql://localhost",
    ] {
        assert!(approve_disposable_target(uri, CONFIRMATION).is_err());
    }
}

#[test]
#[ignore = "requires an explicitly provisioned disposable Turso database and authorized credential"]
fn authenticated_disposable_turso_snapshot_survives_reconnect() {
    let endpoint = std::env::var("NULANG_TURSO_E2E_URL")
        .expect("NULANG_TURSO_E2E_URL must identify an authorized disposable database");
    let confirmation = std::env::var("NULANG_TURSO_E2E_CONFIRM").unwrap_or_default();
    approve_disposable_target(&endpoint, &confirmation)
        .expect("refusing schema or data mutations outside a confirmed disposable Turso database");

    let token = std::env::var("NULANG_TURSO_E2E_TOKEN")
        .expect("NULANG_TURSO_E2E_TOKEN must be supplied securely");
    assert!(!token.is_empty(), "Turso smoke token cannot be empty");

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after UNIX epoch")
        .as_nanos();
    let actor_id = (nanos % (i64::MAX as u128 - 1)) as u64 + 1;
    let marker = format!("nulang-smoke-{nanos}-{}", std::process::id());

    // new_remote itself initializes tables. This must be DISPOSABLE.
    let mut store = LibsqlStore::new_remote(&endpoint, &token)
        .unwrap_or_else(|_| panic!("authorized Turso connection/schema initialization failed"));
    let mut snapshot = ActorSnapshot {
        actor_id,
        sequence: 1,
        ..ActorSnapshot::default()
    };
    snapshot.state.insert(
        "__turso_acceptance_marker".to_owned(),
        PersistedValue::String(marker.clone()),
    );
    store
        .save_snapshot(snapshot)
        .unwrap_or_else(|_| panic!("authorized Turso snapshot write failed"));
    drop(store);

    let reopened = LibsqlStore::new_remote(&endpoint, &token)
        .unwrap_or_else(|_| panic!("authorized Turso reconnect failed"));
    let restored = reopened
        .load_snapshot(actor_id)
        .expect("snapshot not found after authenticated Turso reconnect");
    match restored.state.get("__turso_acceptance_marker") {
        Some(PersistedValue::String(value)) if value == &marker => {}
        _ => panic!("authenticated Turso snapshot readback did not match committed value"),
    }
    // Intentionally no cleanup SQL: the entire disposable database is retired separately.
    // Never DELETE, DROP, or mutate unrelated records.
}
