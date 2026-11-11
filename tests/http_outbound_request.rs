//! Loopback-only behavioral tests for the experimental Http.request transport.
#![cfg(feature = "ureq")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

fn serve_once(
    status: &str,
    body: &str,
    extra_headers: &str,
) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let url = format!("http://{}/v1/test", listener.local_addr().unwrap());
    let status = status.to_owned();
    let body = body.to_owned();
    let extra_headers = extra_headers.to_owned();
    let task = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut received = Vec::new();
        let header_end = loop {
            let mut buffer = [0u8; 4096];
            let n = stream.read(&mut buffer).expect("read HTTP request");
            assert!(n > 0, "connection ended before HTTP headers");
            received.extend_from_slice(&buffer[..n]);
            if let Some(start) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                break start + 4;
            }
            assert!(received.len() < 16384, "request headers too long");
        };
        let header_text = String::from_utf8_lossy(&received[..header_end]).to_ascii_lowercase();
        let length: usize = header_text
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .unwrap_or("0")
            .trim()
            .parse()
            .expect("parse content length");
        while received.len() - header_end < length {
            let mut buffer = [0u8; 4096];
            let n = stream.read(&mut buffer).expect("read request body");
            assert!(n > 0);
            received.extend_from_slice(&buffer[..n]);
        }
        write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}", body.len()).expect("send reply");
        String::from_utf8(received).expect("request is UTF-8")
    });
    (url, task)
}

#[test]
fn transmits_headers_and_methods_and_preserves_response_status() {
    let (url, listener) = serve_once("201 Created", "created", "X-Request-Id: req-123\r\n");
    let result = nulang::http_outbound::request_text(
        "PATCH",
        &url,
        r#"[["Authorization","Bearer sample"],["Content-Type","application/json"],["X-Tenant","tenant-1"],["traceparent","00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"]]"#,
        r#"{"name":"nulang"}"#,
    ).expect("HTTP reply");
    let seen = listener.join().expect("server");
    assert!(seen.starts_with("PATCH /v1/test HTTP/1.1\r\n"));
    assert!(
        seen.contains("Authorization: Bearer sample\r\n")
            || seen
                .to_lowercase()
                .contains("authorization: bearer sample\r\n")
    );
    assert!(seen.to_ascii_lowercase().contains("x-tenant: tenant-1\r\n"));
    assert!(seen
        .to_ascii_lowercase()
        .contains("traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01\r\n"));
    assert!(seen.contains(r#"{"name":"nulang"}"#));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["status"], 201);
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["body"], "created");
    assert_eq!(parsed["headers"]["x-request-id"], "req-123");
}

#[test]
fn reports_non_2xx_http_responses_instead_of_discarding_status() {
    let (url, server) = serve_once("401 Unauthorized", "denied", "");
    let result = nulang::http_outbound::request_text("GET", &url, "[]", "").unwrap();
    server.join().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["status"], 401);
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["body"], "denied");
}

#[test]
fn does_not_follow_http_redirects_that_could_forward_credentials() {
    let (url, server) = serve_once("302 Found", "", "Location: http://127.0.0.1:1/steal\r\n");
    let result = nulang::http_outbound::request_text(
        "GET",
        &url,
        r#"[["Authorization","Bearer secret"]]"#,
        "",
    )
    .unwrap();
    server.join().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["status"], 302);
    assert_eq!(parsed["ok"], false);
}

#[test]
fn rejects_invalid_headers_methods_and_urls_before_opening_sockets() {
    let cases = [
        (
            "GET",
            "http://127.0.0.1:1/",
            r#"[["X-Test","a\r\nInjected: 1"]]"#,
        ),
        (
            "GET",
            "http://127.0.0.1:1/",
            r#"[["Host","malicious.test"]]"#,
        ),
        (
            "GET",
            "http://127.0.0.1:1/",
            r#"[["x-token","a"],["X-Token","b"]]"#,
        ),
        ("GET", "http://127.0.0.1:1/", r#"[["Bad Name","x"]]"#),
        ("TRACE", "http://127.0.0.1:1/", "[]"),
        ("GET", "file:///etc/passwd", "[]"),
        ("GET", "http://user:password@127.0.0.1:1/", "[]"),
    ];
    for (method, url, headers) in cases {
        assert!(
            nulang::http_outbound::request_text(method, url, headers, "").is_err(),
            "must reject {method} {url} {headers}"
        );
    }
}
