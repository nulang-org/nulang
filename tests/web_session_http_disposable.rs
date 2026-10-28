//! Disposable-only real TCP HTTP/1.1 witness. NO production HTTP route is exposed.
//! The socket bound to 127.0.0.1:0 is owned and dropped by each individual test.
//! Execute ONLY through tests/run_web_session_pg_disposable.sh.
#![cfg(feature = "postgres")]

use nulang::web::bindings::{RouteBindingContract, RouteBindingSource};
use nulang::web::csrf::SessionCsrfToken;
use nulang::web::origin_admission::validate_mutation_origin;
use nulang::web::session_pg_reference::{
    DisposableSessionLookup, DisposableTenantResourcePolicy, TenantDatabaseLogin,
};
use nulang::web::session_preflight::{
    authorized_session_preflight, AdmissionError, SESSION_COOKIE_NAME,
};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

const NOW: u64 = 1_800_000_000;
const HANDLE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HANDLE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TEST_PATH: &str = "/__nulang_internal_disposable_preflight";

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
    let csrf_a = SessionCsrfToken::generate().unwrap();
    let csrf_b = SessionCsrfToken::generate().unwrap();
    let mut admin = postgres::Config::new();
    admin
        .host("127.0.0.1")
        .port(port)
        .dbname("postgres")
        .user(&std::env::var("NULANG_PG_ADMIN_USER").unwrap())
        .password(&std::env::var("NULANG_PG_ADMIN_PASSWORD").unwrap());
    let mut client = admin.connect(postgres::NoTls).unwrap();
    client
        .execute("DELETE FROM nulang_private.sessions", &[])
        .unwrap();
    for (handle, principal, tenant, token) in [
        (HANDLE_A, "user-a", "tenant-a", csrf_a.encode()),
        (HANDLE_B, "user-b", "tenant-b", csrf_b.encode()),
    ] {
        let digest = blake3::hash(handle.as_bytes()).to_hex().to_string();
        client.execute(
            "INSERT INTO nulang_private.sessions (handle_digest, principal_id, tenant_id, expires_at, csrf_hex, revoked) VALUES ($1,$2,$3,$4,$5,false)",
            &[&digest, &principal, &tenant, &((NOW+90) as i64), &token],
        ).unwrap();
    }
    (store, policy, csrf_a, csrf_b)
}

fn binding() -> [RouteBindingContract; 1] {
    [RouteBindingContract {
        source: RouteBindingSource::Form,
        source_name: "quantity".into(),
        handler_param: "private_server_slot".into(),
        handler_index: 0,
        ty: Some("Int".into()),
    }]
}

/// One connection, never a keepalive server; 16 KiB max wire payload and
/// duplicate-preserving, bounded HTTP/1.1 parse. No VM or handler is invoked.
fn respond(
    mut socket: TcpStream,
    bound: SocketAddr,
    sessions: &DisposableSessionLookup,
    authority: &DisposableTenantResourcePolicy,
) {
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut raw = Vec::new();
    let read_result = Read::by_ref(&mut socket)
        .take(16 * 1024 + 1)
        .read_to_end(&mut raw);
    let result: (u16, String) = if read_result.is_err() || raw.len() > 16 * 1024 {
        (400, "{}".into())
    } else {
        parse_and_preflight(&raw, bound, sessions, authority)
    };
    let (status, body) = result;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).unwrap();
}

fn parse_and_preflight(
    raw: &[u8],
    bound: SocketAddr,
    sessions: &DisposableSessionLookup,
    authority: &DisposableTenantResourcePolicy,
) -> (u16, String) {
    let mut header_storage = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut header_storage);
    let head_end = match parsed.parse(raw) {
        Ok(httparse::Status::Complete(n)) if n <= 8192 => n,
        _ => return (400, "{}".into()),
    };
    let Some(method) = parsed.method else {
        return (400, "{}".into());
    };
    let Some(target) = parsed.path else {
        return (400, "{}".into());
    };
    let mut headers = Vec::new();
    let mut size = None;
    for h in parsed.headers.iter() {
        let Ok(v) = std::str::from_utf8(h.value) else {
            return (400, "{}".into());
        };
        if h.name.eq_ignore_ascii_case("Transfer-Encoding") {
            return (400, "{}".into());
        }
        if h.name.eq_ignore_ascii_case("Content-Length") {
            if size.is_some() {
                return (400, "{}".into());
            }
            let Ok(n) = v.parse::<usize>() else {
                return (400, "{}".into());
            };
            if n > 4096 {
                return (400, "{}".into());
            }
            size = Some(n);
        }
        headers.push((h.name.to_owned(), v.to_owned()));
    }
    if size != Some(raw.len() - head_end) {
        return (400, "{}".into());
    }
    // Provenance admission occurs before cookie lookup or any validation.
    if validate_mutation_origin(method, &headers, Some(bound), None).is_err() {
        return (403, "{}".into());
    }
    if target != TEST_PATH {
        return (404, "{}".into());
    }
    if method != "POST" {
        return (405, "{}".into());
    }
    let results = authorized_session_preflight(
        sessions,
        authority,
        &headers,
        &raw[head_end..],
        target,
        &HashMap::new(),
        NOW,
        "tenant-a",
        "order-4",
        &binding(),
        1,
    );
    match results {
        Ok(result) => (200, result.to_json().to_string()),
        Err(AdmissionError::Denied) => (403, "{}".into()),
        Err(AdmissionError::Unavailable) => (503, "{}".into()),
        Err(AdmissionError::InvalidPlan(_)) => (500, "{}".into()),
    }
}

fn exchange(
    sessions: &DisposableSessionLookup,
    authority: &DisposableTenantResourcePolicy,
    method: &str,
    target: &str,
    cookie: &str,
    csrf: &str,
    extra: &[(&str, &str)],
    omit_origin: bool,
) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let bound = listener.local_addr().unwrap();
    assert!(bound.ip().is_loopback());
    let host = bound.to_string();
    let origin = format!("http://{host}");
    std::thread::scope(|scope| {
        let server = scope.spawn(|| {
            let (socket, peer) = listener.accept().unwrap();
            assert!(peer.ip().is_loopback());
            respond(socket, bound, sessions, authority);
        });
        let mut client = TcpStream::connect(bound).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let body = b"quantity=wrong&tenant=tenant-attacker";
        let mut wire = format!(
            "{method} {target} HTTP/1.1\r\nHost: {host}\r\nCookie: {SESSION_COOKIE_NAME}={cookie}\r\nX-CSRF-Token: {csrf}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if !omit_origin {
            wire.push_str(&format!("Origin: {origin}\r\n"));
        }
        for (key, val) in extra {
            wire.push_str(&format!("{key}: {val}\r\n"));
        }
        wire.push_str("\r\n");
        client.write_all(wire.as_bytes()).unwrap();
        client.write_all(body).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        server.join().unwrap();
        response
    })
}

fn assert_status(response: &str, code: u16) {
    assert!(
        response.starts_with(&format!("HTTP/1.1 {code} ")),
        "{response}"
    );
    assert!(response.contains("Cache-Control: no-store"));
}

#[test]
fn loopback_real_http_denies_untrusted_and_only_returns_authorized_diagnostics() {
    let (store, policy, a, b) = fixture();
    let valid = exchange(
        &store,
        &policy,
        "POST",
        TEST_PATH,
        HANDLE_A,
        &a.encode(),
        &[
            ("Sec-Fetch-Site", "same-origin"),
            ("X-Tenant-Id", "tenant-b"),
        ],
        false,
    );
    assert_status(&valid, 200);
    assert!(valid.contains("\"source_name\":\"quantity\""));
    assert!(!valid.contains("private_server_slot"));
    assert!(!valid.contains("user-a"));
    for denied in [
        exchange(
            &store,
            &policy,
            "POST",
            TEST_PATH,
            HANDLE_A,
            &a.encode(),
            &[("Sec-Fetch-Site", "cross-site")],
            false,
        ),
        exchange(
            &store,
            &policy,
            "POST",
            TEST_PATH,
            HANDLE_A,
            &a.encode(),
            &[("Host", "evil.example")],
            false,
        ),
        exchange(
            &store,
            &policy,
            "POST",
            TEST_PATH,
            HANDLE_A,
            &a.encode(),
            &[],
            true,
        ),
        exchange(
            &store,
            &policy,
            "POST",
            TEST_PATH,
            HANDLE_A,
            &b.encode(),
            &[],
            false,
        ),
        exchange(
            &store,
            &policy,
            "POST",
            TEST_PATH,
            HANDLE_B,
            &b.encode(),
            &[],
            false,
        ),
        exchange(
            &store,
            &policy,
            "POST",
            TEST_PATH,
            HANDLE_A,
            &a.encode(),
            &[("X-CSRF-Token", "duplicated")],
            false,
        ),
    ] {
        assert_status(&denied, 403);
        assert!(!denied.contains("source_name"));
        assert!(!denied.contains("private_server_slot"));
    }
    assert_status(
        &exchange(
            &store,
            &policy,
            "POST",
            "/other",
            HANDLE_A,
            &a.encode(),
            &[],
            false,
        ),
        404,
    );
    assert_status(
        &exchange(
            &store,
            &policy,
            "GET",
            TEST_PATH,
            HANDLE_A,
            &a.encode(),
            &[],
            false,
        ),
        405,
    );
    // Reject ambiguous message framing before any provenance or session check.
    for unsafe_framing in [("Content-Length", "5"), ("Transfer-Encoding", "chunked")] {
        let malformed = exchange(
            &store,
            &policy,
            "POST",
            TEST_PATH,
            HANDLE_A,
            &a.encode(),
            &[unsafe_framing],
            false,
        );
        assert_status(&malformed, 400);
        assert!(!malformed.contains("source_name"));
    }
}
