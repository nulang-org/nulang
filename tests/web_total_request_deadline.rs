//! Real TCP regression: per-read socket timeouts must not permit indefinite
//! trickle-fed request headers or bodies. The deadline spans the full request.
use nulang::runtime::{WebDevServer, WebServerOptions};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

fn trickle_request(prefix: &[u8]) -> String {
    let options = WebServerOptions {
        read_timeout: Duration::from_millis(200),
        write_timeout: Duration::from_secs(2),
        ..WebServerOptions::default()
    };
    let server = WebDevServer::bind_runtime_with_options(0, None, None, vec![], options)
        .expect("bind development HTTP listener");
    let mut socket = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket.write_all(prefix).unwrap();

    // A legacy per-read timeout resets each time a byte arrives.
    // Every interval is comfortably under 200ms, but total exceeds it.
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(20));
        if socket.write_all(b"x").is_err() {
            break;
        }
    }
    let _ = socket.shutdown(Shutdown::Write);
    let mut response = Vec::new();
    socket
        .read_to_end(&mut response)
        .expect("read bounded server response");
    String::from_utf8(response).unwrap()
}

#[test]
fn incomplete_header_trickle_is_rejected_with_total_request_deadline() {
    let response = trickle_request(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Slow: ");
    assert!(
        response.starts_with("HTTP/1.1 408 Request Timeout"),
        "{response}"
    );
}

#[test]
fn incomplete_body_trickle_is_rejected_with_total_request_deadline() {
    let response =
        trickle_request(b"POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: 256\r\n\r\n");
    assert!(
        response.starts_with("HTTP/1.1 408 Request Timeout"),
        "{response}"
    );
}
