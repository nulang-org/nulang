//! Native-host authorization boundary for durable web SQLite access.
//! The gate never trusts a client-selected tenant ID; only a verifier's
//! successful bearer-token resolution may create a tenant-scoped handle.

#![cfg(feature = "sqlite")]

use nulang::runtime::{
    AuthorizedWebSqlite, WebSqliteAuthGate, WebSqliteStore, WebTenantTokenVerifier,
};
use std::io;

struct TestVerifier;

impl WebTenantTokenVerifier for TestVerifier {
    fn verified_tenant(&self, bearer_token: &str) -> io::Result<Option<String>> {
        match bearer_token {
            "allow-a" => Ok(Some("tenant-a".into())),
            "allow-b" => Ok(Some("tenant-b".into())),
            "broken-tenant" => Ok(Some("  ".into())),
            "verifier-error" => Err(io::Error::other("identity provider offline")),
            _ => Ok(None),
        }
    }
}

fn headers(auth: &str, untrusted_tenant: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".into(), auth.into()),
        ("X-Tenant-Id".into(), untrusted_tenant.into()),
    ]
}

fn database() -> WebSqliteStore {
    WebSqliteStore::open(format!(
        "{}/nulang_auth_gate_{}_{}.db",
        std::env::temp_dir().display(),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
    .unwrap()
}

#[test]
fn gate_scopes_all_crud_operations_to_verified_tenant_not_spoofed_headers() {
    let store = database();
    let verifier = TestVerifier;
    let gate = WebSqliteAuthGate::new(&store, &verifier);
    let a: AuthorizedWebSqlite<'_> = gate
        .authorize_headers(&headers("Bearer allow-a", "tenant-b"))
        .expect("valid tenant A credentials");
    let b = gate
        .authorize_headers(&headers("Bearer allow-b", "tenant-a"))
        .expect("valid tenant B credentials");

    a.put("shared", "alpha").unwrap();
    b.put("shared", "bravo").unwrap();
    a.put("a-only", "private").unwrap();

    assert_eq!(a.get("shared").unwrap(), Some("alpha".into()));
    assert_eq!(b.get("shared").unwrap(), Some("bravo".into()));
    assert_eq!(b.get("a-only").unwrap(), None);
    assert_eq!(b.list(10).unwrap(), vec![("shared".into(), "bravo".into())]);
    assert!(b.delete("shared").unwrap());
    assert_eq!(a.get("shared").unwrap(), Some("alpha".into()));
}

#[test]
fn gate_denies_missing_invalid_and_ambiguous_authorization() {
    let store = database();
    let verifier = TestVerifier;
    let gate = WebSqliteAuthGate::new(&store, &verifier);

    let invalid = vec![
        vec![],
        vec![("X-Tenant-Id".into(), "tenant-a".into())],
        headers("Bearer unknown", "tenant-a"),
        headers("Basic allow-a", "tenant-a"),
        headers("Bearer ", "tenant-a"),
        headers("Bearer allow-a extra", "tenant-a"),
        headers("Bearer allow-a,allow-b", "tenant-a"),
        headers("Bearer broken-tenant", "tenant-a"),
        vec![
            ("Authorization".into(), "Bearer allow-a".into()),
            ("authorization".into(), "Bearer allow-b".into()),
        ],
    ];
    for input in invalid {
        let error = match gate.authorize_headers(&input) {
            Ok(_) => panic!("incorrectly authorized {input:?}"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{input:?}");
    }

    assert!(gate
        .authorize_headers(&headers("bearer allow-a", "tenant-b"))
        .is_ok(), "HTTP auth scheme is case insensitive");
}

#[test]
fn verifier_errors_fail_closed_without_mutating_storage() {
    let store = database();
    let verifier = TestVerifier;
    let gate = WebSqliteAuthGate::new(&store, &verifier);
    assert!(gate.authorize_headers(&headers("Bearer verifier-error", "tenant-a")).is_err());
    assert_eq!(store.list("tenant-a", 10).unwrap(), vec![]);
}

#[test]
fn tenant_handle_is_request_scoped_and_has_no_tenant_selector() {
    let store = database();
    let verifier = TestVerifier;
    let gate = WebSqliteAuthGate::new(&store, &verifier);
    let authorized = gate.authorize_headers(&headers("Bearer allow-a", "tenant-b")).unwrap();
    authorized.put("item", "value").unwrap();

    // The handle contains no caller-settable tenant ID. Each operation uses
    // the immutable tenant ID produced by verified_tenant().
    assert_eq!(authorized.get("item").unwrap(), Some("value".into()));
    assert_eq!(store.get("tenant-b", "item").unwrap(), None);
}
