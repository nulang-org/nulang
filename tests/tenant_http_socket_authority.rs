//! Real TCP HTTP -> pinned Ed25519 -> PostgreSQL RLS proof.
//! Run exclusively under tests/run_tenant_pg_authority_ci.sh.
#![cfg(feature = "postgres")]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use nulang::runtime::WebDevServer;
use nulang::web::tenant_jwt_reference::{PinnedEd25519Verifier, RevocationPolicy};
use nulang::web::tenant_pg_reference::{TenantPostgresAuthority, TenantRoleBinding};
use serde_json::json;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct FixtureRevocation;
impl RevocationPolicy for FixtureRevocation {
    fn is_revoked(&self, _issuer: &str, _subject: &str, _jti: &str) -> Option<bool> {
        Some(false) // ONLY in the isolated integration test fixture
    }
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[41u8; 32]) // deterministic fixture, never production
}

fn signed_token(sub: &str, aud: &str) -> String {
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let h = json!({"alg":"EdDSA","kid":"fixture-key","typ":"at+jwt"});
    let c = json!({
        "iss":"issuer-a", "sub":sub, "aud":aud, "jti":format!("http-fixture-{sub}"),
        "iat":now-2, "nbf":now-2, "exp":now+120
    });
    let msg = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&h).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&c).unwrap())
    );
    let signature = signing_key().sign(msg.as_bytes());
    format!("{msg}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

fn server() -> WebDevServer {
    assert_eq!(
        std::env::var("NULANG_TEST_POSTGRES_DISPOSABLE").as_deref(),
        Ok("1")
    );
    let verifier = PinnedEd25519Verifier::new(
        "issuer-a",
        "nulang-tenant-api",
        "fixture-key",
        signing_key().verifying_key(),
        FixtureRevocation,
    )
    .unwrap();
    let credentials = HashMap::from([
        (
            "issuer-a|alice".into(),
            TenantRoleBinding::new(
                "tenant-a",
                "example_tenant_a",
                std::env::var("NULANG_TENANT_TEST_PASSWORD_A").unwrap(),
            ),
        ),
        (
            "issuer-a|bob".into(),
            TenantRoleBinding::new(
                "tenant-b",
                "example_tenant_b",
                std::env::var("NULANG_TENANT_TEST_PASSWORD_B").unwrap(),
            ),
        ),
    ]);
    let authority = TenantPostgresAuthority::new(
        "127.0.0.1",
        std::env::var("PGPORT").unwrap().parse().unwrap(),
        verifier,
        credentials,
    )
    .unwrap();
    WebDevServer::bind_disposable_tenant_authority(authority)
        .expect("loopback-only disposable WebDevServer binding")
}

fn request(port: u16, method: &str, path: &str, headers: &[(&str, &str)]) -> String {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut raw = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (name, value) in headers {
        raw.push_str(&format!("{name}: {value}\r\n"));
    }
    raw.push_str("Content-Length: 0\r\nConnection: close\r\n\r\n");
    socket.write_all(raw.as_bytes()).unwrap();
    socket.shutdown(Shutdown::Write).unwrap();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).unwrap();
    String::from_utf8(bytes).unwrap()
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").unwrap().1
}

fn status(response: &str) -> u16 {
    response.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn real_http_bearer_identity_restricts_rows_and_prevents_tenant_spoofing() {
    let srv = server();
    let alice = signed_token("alice", "nulang-tenant-api");
    let bob = signed_token("bob", "nulang-tenant-api");
    let a = request(
        srv.port,
        "GET",
        "/__nulang_test/tenant-faces?tenant_id=tenant-b",
        &[
            ("Authorization", &format!("Bearer {alice}")),
            ("X-Tenant-Id", "tenant-b"),
        ],
    );
    assert_eq!(status(&a), 200, "{a}");
    let faces_a: Vec<String> = serde_json::from_str(body(&a)).unwrap();
    assert_eq!(faces_a, vec!["Face A", "New face"]);

    let b = request(
        srv.port,
        "GET",
        "/__nulang_test/tenant-faces?tenant_id=tenant-a",
        &[
            ("Authorization", &format!("Bearer {bob}")),
            ("X-Tenant-Id", "tenant-a"),
        ],
    );
    assert_eq!(status(&b), 200, "{b}");
    let faces_b: Vec<String> = serde_json::from_str(body(&b)).unwrap();
    assert_eq!(faces_b, vec!["Face B", "Foreign Face"]);

    let a_again = request(
        srv.port,
        "GET",
        "/__nulang_test/tenant-faces",
        &[("Authorization", &format!("Bearer {alice}"))],
    );
    assert_eq!(status(&a_again), 200);
    assert_eq!(body(&a_again), body(&a), "A-B-A must preserve isolation");
    assert!(a.contains("Cache-Control: no-store") || a.contains("cache-control: no-store"));
}

#[test]
fn real_http_invalid_identity_or_host_never_reaches_sql() {
    let srv = server();
    let alice = signed_token("alice", "nulang-tenant-api");
    let wrong_audience = signed_token("alice", "untrusted-api");

    for hs in [
        vec![],
        vec![("X-Tenant-Id", "tenant-a")],
        vec![("Authorization", "Bearer fake.jwt.token")],
        vec![("Authorization", &format!("Bearer {wrong_audience}"))],
        vec![
            ("Authorization", &format!("Bearer {alice}")),
            ("authorization", "Bearer fake"),
        ],
    ] {
        let response = request(srv.port, "GET", "/__nulang_test/tenant-faces", &hs);
        assert_eq!(status(&response), 401, "{response}");
    }

    let host_bad = format!(
        "GET /__nulang_test/tenant-faces HTTP/1.1\r\nHost: attacker.example:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        srv.port, alice,
    );
    let mut socket = TcpStream::connect(("127.0.0.1", srv.port)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket.write_all(host_bad.as_bytes()).unwrap();
    socket.shutdown(Shutdown::Write).unwrap();
    let mut bytes = Vec::new();
    socket.read_to_end(&mut bytes).unwrap();
    assert_eq!(status(&String::from_utf8(bytes).unwrap()), 403);
}

#[test]
fn real_http_cross_origin_or_unprotected_mutation_never_executes_sql() {
    let srv = server();
    let alice = signed_token("alice", "nulang-tenant-api");
    let authorization = format!("Bearer {alice}");
    let url = "/__nulang_test/tenant-faces";
    let denied_origin = request(
        srv.port,
        "POST",
        url,
        &[
            ("Authorization", &authorization),
            ("Origin", "https://evil.example"),
        ],
    );
    assert_eq!(status(&denied_origin), 403, "{denied_origin}");

    let missing_origin = request(srv.port, "POST", url, &[("Authorization", &authorization)]);
    assert_eq!(status(&missing_origin), 403, "{missing_origin}");

    // Valid Origin is necessary but NOT a synchronizer CSRF token.
    // The reference remains GET-only, so all state-changing methods deny.
    let origin = format!("http://127.0.0.1:{}", srv.port);
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let denied = request(
            srv.port,
            method,
            url,
            &[
                ("Authorization", &authorization),
                ("Origin", &origin),
                ("Sec-Fetch-Site", "same-origin"),
            ],
        );
        assert_eq!(status(&denied), 405, "{method}: {denied}");
    }
}

#[test]
fn disposable_listener_denies_unauthenticated_requests() {
    let srv = server();
    let resp = request(srv.port, "GET", "/__nulang_test/tenant-faces", &[]);
    assert_eq!(status(&resp), 401);
}

#[test]
fn ordinary_webdevserver_never_installs_disposable_sql_route() {
    let srv = WebDevServer::bind_runtime(0, None, None, vec![])
        .expect("bind ordinary development server");
    let resp = request(srv.port, "GET", "/__nulang_test/tenant-faces", &[]);
    assert_eq!(status(&resp), 404, "{resp}");
}
