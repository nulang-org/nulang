//! Wire-level regressions for the bounded experimental SSE transport.

use nulang::runtime::{WebDevServer, WebServerOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

fn subscribe(server: &WebDevServer, room: &str) -> (String, BufReader<TcpStream>) {
    subscribe_path(server, &format!("/__nulang/sse/{room}"))
}

fn subscribe_path(server: &WebDevServer, path: &str) -> (String, BufReader<TcpStream>) {
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
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

fn drain_snapshot(reader: &mut BufReader<TcpStream>) {
    let mut frame = String::new();
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\n" {
            break;
        }
        frame.push_str(&line);
    }
    assert!(frame.contains("event: nulang-ui-snapshot"), "{frame}");
}

fn assert_idle_disconnect_recovers(path: &str, has_snapshot: bool) {
    // Saturate both the per-room bound and the host-wide connection bound.
    let server = WebDevServer::bind_runtime_with_options(
        0,
        None,
        None,
        vec![],
        WebServerOptions {
            max_concurrent_connections: 8,
            ..WebServerOptions::default()
        },
    )
    .unwrap();
    let mut readers = Vec::new();
    for _ in 0..8 {
        let (status, mut reader) = subscribe_path(&server, path);
        assert_eq!(status, "HTTP/1.1 200 OK\r\n");
        if has_snapshot {
            drain_snapshot(&mut reader);
        }
        readers.push(reader);
    }
    let (overflow, reader) = subscribe_path(&server, path);
    assert!(overflow.starts_with("HTTP/1.1 503 "), "{overflow}");
    drop(reader);

    // Read the complete initial frame before closing, so this also exercises
    // ordinary FIN rather than relying on unread data causing an abortive RST.
    let closing = readers.pop().unwrap();
    closing.get_ref().shutdown(Shutdown::Both).unwrap();
    drop(closing);
    let started = Instant::now();
    loop {
        let (status, mut replacement) = subscribe_path(&server, path);
        if status == "HTTP/1.1 200 OK\r\n" {
            if has_snapshot {
                drain_snapshot(&mut replacement);
            }
            assert!(started.elapsed() < Duration::from_secs(6));
            readers.push(replacement);
            break;
        }
        assert!(status.starts_with("HTTP/1.1 503 "), "{status}");
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "idle disconnection did not release room and host capacity within six seconds"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (overflow, _reader) = subscribe_path(&server, path);
    assert!(overflow.starts_with("HTTP/1.1 503 "), "{overflow}");
}

#[test]
fn sse_idle_disconnect_releases_room_and_host_capacity() {
    assert_idle_disconnect_recovers("/__nulang/sse/wire-idle-release", false);
}

#[test]
fn ui_sse_idle_disconnect_releases_room_and_host_capacity() {
    assert_idle_disconnect_recovers("/__nulang/ui-sse/wire-ui-idle-release", true);
}

#[test]
fn sse_request_write_half_close_preserves_response_stream() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![]).unwrap();
    let (status, mut reader) = subscribe(&server, "wire-half-close");
    assert_eq!(status, "HTTP/1.1 200 OK\r\n");
    reader.get_ref().shutdown(Shutdown::Write).unwrap();
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(6)))
        .unwrap();
    // Receiving repeated comment frames proves that request-side EOF does not
    // cancel a client that is still receiving the response.
    for _ in 0..2 {
        let mut comment = String::new();
        assert_ne!(reader.read_line(&mut comment).unwrap(), 0);
        assert_eq!(comment, ": keep-alive\n");
        let mut blank = String::new();
        assert_ne!(reader.read_line(&mut blank).unwrap(), 0);
        assert_eq!(blank, "\n");
    }
}

#[test]
fn overloaded_listener_returns_complete_503_to_fragmented_request() {
    use std::io::Read;
    let server = WebDevServer::bind_runtime_with_options(
        0,
        None,
        None,
        vec![],
        WebServerOptions {
            max_concurrent_connections: 1,
            ..WebServerOptions::default()
        },
    )
    .unwrap();
    let (status, _occupied) = subscribe(&server, "wire-fragment-overload");
    assert_eq!(status, "HTTP/1.1 200 OK\r\n");
    for _ in 0..4 {
        let mut peer = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        peer.write_all(b"GET / HTTP/1.1\r\n").unwrap();
        std::thread::sleep(Duration::from_millis(80));
        peer.write_all(b"Host: localhost\r\n\r\n")
            .expect("overload response must allow request completion");
        peer.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        peer.read_to_string(&mut response)
            .expect("complete overload response without reset");
        assert!(response.starts_with("HTTP/1.1 503 "), "{response:?}");
        assert!(response.contains("Retry-After: 1\r\n"), "{response:?}");
        assert!(
            response.ends_with("Server at connection capacity"),
            "{response:?}"
        );
    }
}

#[test]
fn idle_overload_peers_do_not_serialize_rejection_responses() {
    use std::io::Read;
    let server = WebDevServer::bind_runtime_with_options(
        0,
        None,
        None,
        vec![],
        WebServerOptions {
            max_concurrent_connections: 1,
            ..WebServerOptions::default()
        },
    )
    .unwrap();
    let (status, occupied) = subscribe(&server, "wire-concurrent-overload");
    assert_eq!(status, "HTTP/1.1 200 OK\r\n");
    // Keep the rejected write sides open until all responses arrive. The
    // listener must not spend a full drain timeout on each connection.
    let started = Instant::now();
    let mut peers = Vec::new();
    for _ in 0..8 {
        let peer = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peers.push(peer);
    }
    for peer in &mut peers {
        let mut response = String::new();
        peer.read_to_string(&mut response)
            .expect("rejection should finish its write side");
        assert!(response.starts_with("HTTP/1.1 503 "), "{response:?}");
        assert!(
            response.ends_with("Server at connection capacity"),
            "{response:?}"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "rejections were serialized"
    );
    drop(peers);
    drop(occupied);
}
