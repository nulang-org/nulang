//! Tests must run ONLY under the disposable-cluster harness.
//! There are no public routes or production credentials here.
#![cfg(feature = "postgres")]

use nulang::web::bindings::{RouteBindingContract, RouteBindingSource};
use nulang::web::csrf::SessionCsrfToken;
use nulang::web::session_pg_reference::{
    DisposableSessionLookup, DisposableTenantResourcePolicy, TenantDatabaseLogin,
};
use nulang::web::session_preflight::{
    authorized_session_preflight, AdmissionError, ResourceAuthorization, SessionLookup,
    SESSION_COOKIE_NAME,
};
use std::collections::HashMap;

const NOW: u64 = 1_800_000_000;
const HANDLE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HANDLE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn fixture() -> (
    DisposableSessionLookup,
    DisposableTenantResourcePolicy,
    SessionCsrfToken,
    SessionCsrfToken,
) {
    assert_eq!(
        std::env::var("NULANG_TEST_POSTGRES_DISPOSABLE").as_deref(),
        Ok("1")
    );
    let port = std::env::var("PGPORT").unwrap().parse::<u16>().unwrap();
    let store = DisposableSessionLookup::new(
        "127.0.0.1",
        port,
        "nulang_session_reader",
        std::env::var("NULANG_PG_SESSION_PASSWORD").unwrap(),
    )
    .unwrap();
    let policy = DisposableTenantResourcePolicy::new(
        "127.0.0.1",
        port,
        HashMap::from([
            (
                "tenant-a".into(),
                TenantDatabaseLogin::new(
                    "nulang_tenant_a",
                    std::env::var("NULANG_PG_TENANT_PASSWORD_A").unwrap(),
                ),
            ),
            (
                "tenant-b".into(),
                TenantDatabaseLogin::new(
                    "nulang_tenant_b",
                    std::env::var("NULANG_PG_TENANT_PASSWORD_B").unwrap(),
                ),
            ),
        ]),
    )
    .unwrap();
    let a = SessionCsrfToken::generate().unwrap();
    let b = SessionCsrfToken::generate().unwrap();

    let mut admin = postgres::Config::new();
    admin
        .host("127.0.0.1")
        .port(port)
        .user(&std::env::var("NULANG_PG_ADMIN_USER").unwrap())
        .password(&std::env::var("NULANG_PG_ADMIN_PASSWORD").unwrap())
        .dbname("postgres");
    let mut client = admin.connect(postgres::NoTls).unwrap();
    client
        .execute("DELETE FROM nulang_private.sessions", &[])
        .unwrap();
    for (handle, principal, tenant, expires, token, revoked) in [
        (
            HANDLE_A,
            "user-a",
            "tenant-a",
            NOW as i64 + 120,
            a.encode(),
            false,
        ),
        (
            HANDLE_B,
            "user-b",
            "tenant-b",
            NOW as i64 + 120,
            b.encode(),
            false,
        ),
    ] {
        client.execute(
          "INSERT INTO nulang_private.sessions(handle_digest, principal_id, tenant_id, expires_at, csrf_hex, revoked) VALUES ($1,$2,$3,$4,$5,$6)",
          &[&blake3::hash(handle.as_bytes()).to_hex().to_string(), &principal, &tenant, &expires, &token, &revoked],
        ).unwrap();
    }
    (store, policy, a, b)
}

fn bindings() -> Vec<RouteBindingContract> {
    vec![RouteBindingContract {
        source: RouteBindingSource::Form,
        source_name: "quantity".into(),
        handler_param: "private_quantity".into(),
        handler_index: 0,
        ty: Some("Int".into()),
    }]
}
fn check(
    store: &DisposableSessionLookup,
    policy: &DisposableTenantResourcePolicy,
    handle: &str,
    csrf: &str,
    tenant: &str,
    resource: &str,
) -> Result<nulang::web::request_bindings::RequestPreflight, AdmissionError> {
    let headers = vec![
        ("Cookie".into(), format!("{SESSION_COOKIE_NAME}={handle}")),
        ("X-CSRF-Token".into(), csrf.into()),
        (
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        ),
        ("X-Tenant-Id".into(), "tenant-attacker".into()),
    ];
    authorized_session_preflight(
        store,
        policy,
        &headers,
        b"quantity=wrong&tenant=tenant-attacker",
        "/orders/order-4?tenant=tenant-attacker",
        &HashMap::new(),
        NOW,
        tenant,
        resource,
        &bindings(),
        1,
    )
}

#[test]
fn actual_postgres_session_role_and_rls_preserve_a_b_a_isolation() {
    let (store, policy, a, b) = fixture();
    let first = check(
        &store,
        &policy,
        HANDLE_A,
        &a.encode(),
        "tenant-a",
        "order-4",
    )
    .unwrap();
    assert_eq!(first.to_json()["errors"][0]["source_name"], "quantity");
    assert!(matches!(
        check(
            &store,
            &policy,
            HANDLE_A,
            &a.encode(),
            "tenant-b",
            "order-4"
        ),
        Err(AdmissionError::Denied)
    ));
    let second = check(
        &store,
        &policy,
        HANDLE_B,
        &b.encode(),
        "tenant-b",
        "order-4",
    )
    .unwrap();
    assert_eq!(second.to_json()["errors"][0]["source_name"], "quantity");
    assert!(check(
        &store,
        &policy,
        HANDLE_A,
        &a.encode(),
        "tenant-a",
        "order-4"
    )
    .is_ok());
    assert!(!policy
        .can_preflight("user-a", "tenant-b", "order-4")
        .unwrap());
    assert!(!policy
        .can_preflight("user-a", "tenant-a", "only-b")
        .unwrap());
}
#[test]
fn revoked_session_wrong_csrf_and_cross_tenant_access_fail_closed() {
    let (store, policy, a, b) = fixture();
    assert!(matches!(
        check(
            &store,
            &policy,
            HANDLE_A,
            &b.encode(),
            "tenant-a",
            "order-4"
        ),
        Err(AdmissionError::Denied)
    ));
    assert!(matches!(
        check(
            &store,
            &policy,
            HANDLE_B,
            &b.encode(),
            "tenant-a",
            "order-4"
        ),
        Err(AdmissionError::Denied)
    ));
    let port = std::env::var("PGPORT").unwrap().parse::<u16>().unwrap();
    let mut admin = postgres::Config::new();
    admin
        .host("127.0.0.1")
        .port(port)
        .dbname("postgres")
        .user(&std::env::var("NULANG_PG_ADMIN_USER").unwrap())
        .password(&std::env::var("NULANG_PG_ADMIN_PASSWORD").unwrap());
    let mut client = admin.connect(postgres::NoTls).unwrap();
    client
        .execute(
            "UPDATE nulang_private.sessions SET revoked=true WHERE principal_id='user-a'",
            &[],
        )
        .unwrap();
    assert!(matches!(
        check(
            &store,
            &policy,
            HANDLE_A,
            &a.encode(),
            "tenant-a",
            "order-4"
        ),
        Err(AdmissionError::Denied)
    ));
}
#[test]
fn readonly_roles_cannot_bypass_rls_or_mutate_sessions() {
    let (store, policy, _a, _b) = fixture();
    assert!(store.lookup(HANDLE_A).unwrap().is_some());
    assert!(policy
        .can_preflight("user-a", "tenant-a", "order-4")
        .unwrap());

    let port = std::env::var("PGPORT").unwrap().parse::<u16>().unwrap();
    let mut session_reader = postgres::Config::new();
    session_reader
        .host("127.0.0.1")
        .port(port)
        .dbname("postgres")
        .user("nulang_session_reader")
        .password(&std::env::var("NULANG_PG_SESSION_PASSWORD").unwrap());
    let mut client = session_reader.connect(postgres::NoTls).unwrap();
    assert!(client
        .execute("UPDATE nulang_private.sessions SET revoked=false", &[])
        .is_err());
    assert!(client
        .query("SELECT * FROM nulang_example.orders", &[])
        .is_err());

    let mut tenant_conn = postgres::Config::new();
    tenant_conn
        .host("127.0.0.1")
        .port(port)
        .dbname("postgres")
        .user("nulang_tenant_a")
        .password(&std::env::var("NULANG_PG_TENANT_PASSWORD_A").unwrap());
    let mut tenant = tenant_conn.connect(postgres::NoTls).unwrap();
    let visible: i64 = tenant
        .query_one("SELECT COUNT(*) FROM nulang_example.orders", &[])
        .unwrap()
        .get(0);
    assert_eq!(visible, 2);
    assert!(tenant
        .execute("UPDATE nulang_example.orders SET id='forged'", &[])
        .is_err());
    assert!(tenant
        .query("SELECT * FROM nulang_private.sessions", &[])
        .is_err());
}
#[test]
fn misconfigured_endpoints_and_mismatched_database_role_are_denied() {
    assert!(
        DisposableSessionLookup::new("db.example.com", 5432, "reader", "secret".into()).is_err()
    );
    assert!(DisposableSessionLookup::new("127.0.0.1", 5432, "reader", "secret".into()).is_err());
    let (_store, policy, _a, _b) = fixture();
    assert!(!policy
        .can_preflight("user-b", "tenant-a", "order-4")
        .unwrap());
}
