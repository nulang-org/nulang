#![cfg(feature = "sqlite")]

use nulang::runtime::LibsqlStore;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
fn sqlite_feature_excludes_legacy_rustls_transport_from_lockfile() {
    let lock: toml::Value = toml::from_str(include_str!("../Cargo.lock")).unwrap();
    let packages = lock["package"].as_array().unwrap();

    for package in packages {
        let name = package["name"].as_str().unwrap();
        let version = package["version"].as_str().unwrap();
        assert!(
            !(name == "rustls" && version.starts_with("0.22.")),
            "legacy rustls {version} must not remain in the resolved graph"
        );
        assert!(
            !(name == "hyper-rustls" && version.starts_with("0.25.")),
            "legacy hyper-rustls {version} must not remain in the resolved graph"
        );
        assert!(
            !(name == "rustls-webpki" && version.starts_with("0.102.")),
            "unsupported rustls-webpki {version} must not return"
        );
        assert!(
            !(name == "webpki-roots" && version.starts_with("0.26.")),
            "deprecated bundled webpki roots {version} must not return"
        );
    }
}

#[test]
fn remote_connector_preserves_bearer_authentication() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client must connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        while !request.windows(4).any(|part| part == b"\r\n\r\n") && request.len() < 32_768 {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }

        sender
            .send(String::from_utf8_lossy(&request).into_owned())
            .unwrap();
        stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
    });

    let secret = "nulang-remote-auth-secret-do-not-log";
    let result = LibsqlStore::new_remote(&format!("http://127.0.0.1:{port}"), secret);
    let error = match result {
        Ok(_) => panic!("a 401 remote response must not initialize a persistence store"),
        Err(error) => error,
    };

    let request = receiver.recv_timeout(Duration::from_secs(8)).unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {secret}")),
        "remote connector must retain the authorization header"
    );
    assert!(
        !error.to_string().contains(secret),
        "remote connection errors must not expose authentication tokens"
    );
    server.join().unwrap();
}

#[test]
fn remote_https_rejects_untrusted_certificates() {
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::sync::Arc;

    let identity = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let key: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into();
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert.der().clone()], key)
        .unwrap();

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, receiver) = mpsc::channel();

    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut tls = rustls::ServerConnection::new(Arc::new(server_config)).unwrap();
                    let rejected = tls.complete_io(&mut stream).is_err();
                    sender.send(rejected).unwrap();
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "HTTPS client did not connect");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("TLS accept error: {error}"),
            }
        }
    });

    let secret = "nulang-tls-auth-secret-do-not-log";
    let result = LibsqlStore::new_remote(&format!("https://127.0.0.1:{port}"), secret);
    let error = match result {
        Ok(_) => panic!("an untrusted certificate must never initialize a remote database"),
        Err(error) => error,
    };

    assert!(
        receiver.recv_timeout(Duration::from_secs(12)).unwrap(),
        "the client must reject the untrusted TLS server"
    );
    assert!(
        !error.to_string().contains(secret),
        "TLS errors must not expose remote authentication tokens"
    );
    server.join().unwrap();
}

#[test]
fn remote_url_rejects_non_loopback_plaintext_and_credential_authorities() {
    // Invalid-input classification proves rejection happened before a DNS lookup
    // or attempted TCP connection; a network failure is not sufficient.
    let cases = [
        "http://0.0.0.0:0",
        "http://192.0.2.10:0",
        "http://localhost:0",
        "http://localhost.evil.invalid:0",
        "http://127.0.0.1.evil.invalid:0",
        "http://127.0.0.1@192.0.2.10:0",
        "https://alice:password@remote.example",
        "ftp://remote.example",
        "https://",
        "javascript:alert(1)",
    ];

    let secret = "secret-that-must-not-leak-to-errors";
    for url in cases {
        let error = match LibsqlStore::new_remote(url, secret) {
            Ok(_) => panic!("unsafe remote URL {url:?} unexpectedly accepted"),
            Err(error) => error,
        };
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "{url:?} must be rejected before any network I/O: {error}"
        );
        assert!(
            !error.to_string().contains(secret),
            "{url:?} exposed the bearer token in a validation error"
        );
        assert!(
            !error.to_string().contains("password"),
            "{url:?} exposed embedded URL credentials in a validation error"
        );
    }
}
