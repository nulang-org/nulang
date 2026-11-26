#![cfg(feature = "sqlite")]
//! End-to-end loopback TLS/HTTP/1.1 transport check for the audited libSQL overlay.
//! No untrusted certificates, HTTP plaintext, or real credentials enter production
//! code: the fixture certificate is explicitly trusted ONLY by this test client.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn remote_hrana_uses_http1_with_a_verified_test_certificate_and_bearer_token() {
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
        .expect("generate localhost test identity");
    let pem = certificate.cert.pem();
    let key_pem = certificate.signing_key.serialize_pem();

    let identity = native_tls::Identity::from_pkcs8(pem.as_bytes(), key_pem.as_bytes())
        .expect("valid local test identity");
    let acceptor = tokio_native_tls::TlsAcceptor::from(
        native_tls::TlsAcceptor::new(identity).expect("test TLS server"),
    );

    let socket = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback only");
    let address = socket.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(10), socket.accept())
            .await
            .expect("client connected")
            .expect("accepted connection");
        let mut stream = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream))
            .await
            .expect("TLS handshake finished")
            .expect("TLS test certificate accepted");

        let mut request = Vec::new();
        let mut buf = [0_u8; 2048];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf))
                .await
                .expect("HTTP request received")
                .expect("read request");
            assert!(n > 0, "connection closed before HTTP headers");
            request.extend_from_slice(&buf[..n]);
            assert!(request.len() < 16_384, "unexpected oversized headers");
            if request.windows(4).any(|v| v == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request);
        assert!(
            request.starts_with("POST /v3/pipeline HTTP/1.1\r\n"),
            "Hrana must use HTTP/1.1, not vulnerable h2: {request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("\r\nauthorization: bearer local-fixture-token\r\n"),
            "remote must send the expected Authorization bearer header"
        );
        let response =
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream
            .write_all(response)
            .await
            .expect("write HTTP fixture response");
        stream.flush().await.expect("flush test response");
    });

    let test_root =
        native_tls::Certificate::from_pem(pem.as_bytes()).expect("parse local trusted root");
    let client_tls = native_tls::TlsConnector::builder()
        .add_root_certificate(test_root)
        .build()
        .expect("test client with explicit local root");
    let mut http = hyper::client::HttpConnector::new();
    http.enforce_http(false);
    let mut connector =
        hyper_tls::HttpsConnector::from((http, tokio_native_tls::TlsConnector::from(client_tls)));
    connector.https_only(true);
    let db = libsql::Builder::new_remote(
        format!("https://127.0.0.1:{}", address.port()),
        "local-fixture-token".to_owned(),
    )
    .connector(connector)
    .build()
    .await
    .expect("build remote HTTP/1.1 fixture client");
    let connection = db.connect().expect("connect fixture");
    let result = tokio::time::timeout(Duration::from_secs(10), connection.execute("SELECT 1", ()))
        .await
        .expect("request must complete");
    assert!(result.is_err(), "fixture intentionally returns HTTP 401");
    server
        .await
        .expect("verify request protocol and authorization");
}
