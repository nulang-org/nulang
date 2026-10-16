//! An explicitly mounted, bearer-gated native Web route receives only a
//! tenant-scoped SQLite capability. Ordinary Web routes are unchanged.

#![cfg(feature = "sqlite")]

use nulang::runtime::{
    AuthorizedWebSqlite, HttpMethod, HttpRequest, HttpResponse, WebDevServer,
    WebSqliteHttpMount, WebSqliteNativeHandler, WebSqliteStore, WebTenantTokenVerifier,
};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct Verifier;

impl WebTenantTokenVerifier for Verifier {
    fn verified_tenant(&self, token: &str) -> io::Result<Option<String>> {
        match token {
            "a" => Ok(Some("tenant-a".into())),
            "b" => Ok(Some("tenant-b".into())),
            "down" => Err(io::Error::other("secret upstream identity failure")),
            _ => Ok(None),
        }
    }
}

struct Handler {
    calls: Arc<AtomicUsize>,
}

impl WebSqliteNativeHandler for Handler {
    fn handle(
        &self,
        request: &HttpRequest,
        scoped: &AuthorizedWebSqlite<'_>,
    ) -> io::Result<HttpResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body = match (request.method, request.path.as_str()) {
            (HttpMethod::Post, "/__nulang/sqlite/item") => {
                let value = String::from_utf8(request.body.clone())
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid utf8"))?;
                scoped.put("item", &value)?;
                "saved".into()
            }
            (HttpMethod::Get, "/__nulang/sqlite/item") => {
                scoped.get("item")?.unwrap_or_else(|| "missing".into())
            }
            (HttpMethod::Delete, "/__nulang/sqlite/item") => {
                scoped.delete("item")?.to_string()
            }
            _ => return Ok(HttpResponse {
                status: 404,
                headers: vec![("Content-Type".into(), "text/plain".into())],
                body: b"missing".to_vec(),
            }),
        };
        Ok(HttpResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: body.into_bytes(),
        })
    }
}

fn db_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_web_http_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn cleanup(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}

fn mount(path: &std::path::Path, calls: &Arc<AtomicUsize>) -> Arc<WebSqliteHttpMount> {
    Arc::new(WebSqliteHttpMount::new(
        Arc::new(WebSqliteStore::open(path).unwrap()),
        Arc::new(Verifier),
        Arc::new(Handler { calls: calls.clone() }),
    ))
}

fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut bytes = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        bytes.push_str(&format!("{name}: {value}\r\n"));
    }
    bytes.push_str("\r\n");
    bytes.push_str(body);
    stream.write_all(bytes.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn auth(token: &str) -> Vec<(&'static str, &str)> {
    vec![("Authorization", token), ("X-Tenant-ID", "forged")]
}

#[test]
fn mounted_http_crud_is_tenant_scoped_and_survives_server_restart() {
    let path = db_path();
    let calls = Arc::new(AtomicUsize::new(0));
    {
        let server = WebDevServer::bind_runtime_with_sqlite(
            0,
            None,
            None,
            vec![],
            mount(&path, &calls),
        )
        .unwrap();
        let posted = send(
            server.port, "POST", "/__nulang/sqlite/item", &auth("Bearer a"), "hello",
        );
        assert!(posted.starts_with("HTTP/1.1 200"), "{posted}");
        let a = send(server.port, "GET", "/__nulang/sqlite/item", &auth("Bearer a"), "");
        assert!(a.ends_with("hello"), "{a}");
        let b = send(server.port, "GET", "/__nulang/sqlite/item", &auth("Bearer b"), "");
        assert!(b.ends_with("missing"), "{b}");
    }
    {
        let server = WebDevServer::bind_runtime_with_sqlite(
            0,
            None,
            None,
            vec![],
            mount(&path, &calls),
        )
        .unwrap();
        let a = send(server.port, "GET", "/__nulang/sqlite/item", &auth("Bearer a"), "");
        assert!(a.ends_with("hello"), "SQLite must survive a server restart: {a}");
        let b = send(server.port, "GET", "/__nulang/sqlite/item", &auth("Bearer b"), "");
        assert!(b.ends_with("missing"), "{b}");
        let delete = send(server.port, "DELETE", "/__nulang/sqlite/item", &auth("Bearer a"), "");
        assert!(delete.ends_with("true"), "{delete}");
    }
    cleanup(&path);
}

#[test]
fn mounted_http_rejects_credentials_before_invoking_native_handler() {
    let path = db_path();
    let calls = Arc::new(AtomicUsize::new(0));
    let server = WebDevServer::bind_runtime_with_sqlite(
        0, None, None, vec![], mount(&path, &calls),
    )
    .unwrap();

    for headers in [
        vec![("X-Tenant-ID", "tenant-a")],
        auth("Bearer invalid"),
        vec![("Authorization", "Bearer a"), ("authorization", "Bearer b")],
    ] {
        let response = send(server.port, "GET", "/__nulang/sqlite/item", &headers, "");
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    }
    let down = send(server.port, "GET", "/__nulang/sqlite/item", &auth("Bearer down"), "");
    assert!(down.starts_with("HTTP/1.1 503"), "{down}");
    assert!(!down.contains("secret upstream"), "provider error must be sanitized");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "must reject before dispatch");

    let prefix_collision = send(
        server.port,
        "GET",
        "/__nulang/sqliteevil/item",
        &auth("Bearer a"),
        "",
    );
    assert!(prefix_collision.starts_with("HTTP/1.1 404"), "{prefix_collision}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop(server);
    cleanup(&path);
}


#[test]
fn standard_listener_does_not_enable_sqlite_mount_implicitly() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![]).unwrap();
    let response = send(
        server.port,
        "GET",
        "/__nulang/sqlite/item",
        &auth("Bearer a"),
        "",
    );
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
}

#[test]
fn mounted_http_marks_successful_tenant_data_uncacheable() {
    let path = db_path();
    let calls = Arc::new(AtomicUsize::new(0));
    let server = WebDevServer::bind_runtime_with_sqlite(
        0, None, None, vec![], mount(&path, &calls),
    )
    .unwrap();
    let response = send(server.port, "GET", "/__nulang/sqlite/item", &auth("Bearer a"), "");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("Cache-Control: no-store\r\n"), "{response}");
    drop(server);
    cleanup(&path);
}
