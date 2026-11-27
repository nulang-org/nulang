//! Wire-level regressions for the bounded experimental SSE transport.

use nulang::runtime::{WebDevServer, WebServerOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

fn subscribe(server: &WebDevServer, room: &str) -> (String, BufReader<TcpStream>) {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        stream,
        "GET /__nulang/sse/{room} HTTP/1.1\r\nHost: localhost\r\n\r\n"
    )
    .unwrap();
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    loop {
        let mut header = String::new();
        assert_ne!(
            reader.read_line(&mut header).unwrap(),
            0,
            "incomplete SSE headers"
        );
        if header == "\r\n" {
            break;
        }
    }
    (status, reader)
}

#[test]
fn sse_room_capacity_is_enforced_without_blocking_other_rooms() {
    let server = WebDevServer::bind_runtime_with_options(
        0,
        None,
        None,
        Vec::new(),
        WebServerOptions::default(),
    )
    .unwrap();
    let mut active = Vec::new();
    for _ in 0..8 {
        let (status, reader) = subscribe(&server, "wire-capacity");
        assert_eq!(status, "HTTP/1.1 200 OK\r\n");
        active.push(reader);
    }
    let (other, _other_reader) = subscribe(&server, "wire-other-room");
    assert_eq!(other, "HTTP/1.1 200 OK\r\n");
    let (overflow, _overflow_reader) = subscribe(&server, "wire-capacity");
    assert!(overflow.starts_with("HTTP/1.1 503 "), "{overflow:?}");
    drop(server);
}

#[test]
fn invalid_room_names_are_rejected_before_streaming() {
    let server = WebDevServer::bind_runtime_with_options(
        0,
        None,
        None,
        Vec::new(),
        WebServerOptions::default(),
    )
    .unwrap();
    for room in ["", "../unsafe", "a//b", "a?b", &"x".repeat(129)] {
        let (status, _reader) = subscribe(&server, room);
        assert!(
            status.starts_with("HTTP/1.1 400 "),
            "room {room:?}: {status:?}"
        );
    }
    let (valid, _reader) = subscribe(&server, "wire:namespace/valid.room-1");
    assert_eq!(valid, "HTTP/1.1 200 OK\r\n");
}

#[test]
fn sse_rejects_cross_origin_browser_requests_before_subscribing() {
    let server = WebDevServer::bind_runtime_with_options(
        0,
        None,
        None,
        Vec::new(),
        WebServerOptions::default(),
    )
    .unwrap();
    let host = format!("localhost:{}", server.port);
    let origin = format!("http://{host}");
    let inputs = [
        "Origin: https://untrusted.example\r\n".to_string(),
        "Sec-Fetch-Site: cross-site\r\n".to_string(),
        "Sec-Fetch-Site: same-site\r\n".to_string(),
        format!("Origin: {origin}\r\nOrigin: {origin}\r\n"),
        "Sec-Fetch-Site: same-origin\r\nSec-Fetch-Site: same-origin\r\n".into(),
        "Origin: null\r\n".into(),
        "Referer: https://untrusted.example/\r\n".into(),
        format!("Origin: {origin}\r\nSec-Fetch-Site: cross-site\r\n"),
    ];
    for extra in inputs {
        let status = request_status(&server, "GET", &host, &extra);
        assert!(status.starts_with("HTTP/1.1 403 "), "{extra:?}: {status:?}");
    }
    // A normal browser and nonbrowser EventSource client remain supported.
    let valid = request_status(
        &server,
        "GET",
        &host,
        &format!("Origin: {origin}\r\nSec-Fetch-Site: same-origin\r\n"),
    );
    assert_eq!(valid, "HTTP/1.1 200 OK\r\n");
    let bare = request_status(&server, "GET", &host, "");
    assert_eq!(bare, "HTTP/1.1 200 OK\r\n");
}

#[test]
fn sse_is_get_only() {
    let server = WebDevServer::bind_runtime_with_options(
        0,
        None,
        None,
        Vec::new(),
        WebServerOptions::default(),
    )
    .unwrap();
    let host = format!("localhost:{}", server.port);
    for method in ["HEAD", "OPTIONS", "POST"] {
        let status = request_status(
            &server,
            method,
            &host,
            &format!("Origin: http://{host}\r\nSec-Fetch-Site: same-origin\r\n"),
        );
        assert!(status.starts_with("HTTP/1.1 405 "), "{method}: {status:?}");
    }
}

fn request_status(server: &WebDevServer, method: &str, host: &str, extra: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        stream,
        "{method} /__nulang/sse/admission-fixture HTTP/1.1\r\nHost: {host}\r\n{extra}Content-Length: 0\r\n\r\n"
    ).unwrap();
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    status
}
