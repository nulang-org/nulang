// Process-scoped SSL_CERT_FILE is honored by OpenSSL-backed native-tls on
// Linux. Other platforms have different trust stores and need their own smoke.
#![cfg(all(feature = "sqlite", target_os = "linux"))]

use nulang::runtime::LibsqlStore;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SECRET: &str = "nulang-local-https-test-bearer-never-log";

/// Exercise LibsqlStore's real native-TLS connector, not a test-only connector.
/// A child process isolates OpenSSL trust environment from other Rust tests.
#[test]
fn remote_https_with_trusted_ca_sends_bearer_authentication() {
    if std::env::var_os("NULANG_TLS_TRUST_CHILD").is_some() {
        let url = std::env::var("NULANG_TLS_TEST_URL").expect("child URL");
        let error = match LibsqlStore::new_remote(&url, SECRET) {
            Ok(_) => panic!("HTTP 401 response must not initialize a database"),
            Err(error) => error,
        };
        assert!(
            !error.to_string().contains(SECRET),
            "TLS response leaked bearer"
        );
        return;
    }
    probe_remote_native_tls("127.0.0.1", true);
}

/// The same certificate is trusted by the child process, but its DNS SAN
/// does not match the loopback IP URL. Authentication must not be transmitted.
#[test]
fn remote_https_rejects_trusted_cert_with_wrong_hostname() {
    probe_remote_native_tls("localhost", false);
}

fn probe_remote_native_tls(certificate_san: &str, expect_bearer: bool) {
    let identity = rcgen::generate_simple_self_signed(vec![certificate_san.to_owned()])
        .expect("create ephemeral loopback certificate");
    let key: PrivateKeyDer<'static> =
        PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der()).into();
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert.der().clone()], key)
        .expect("server TLS config");
    let trust_path = std::env::temp_dir().join(format!(
        "nulang-local-ca-{}-{}.pem",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&trust_path, identity.cert.pem()).expect("write ephemeral CA");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let url = format!("https://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (socket, _) = listener.accept().expect("client must connect");
        socket
            .set_read_timeout(Some(Duration::from_secs(7)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(7)))
            .unwrap();
        let tls = rustls::ServerConnection::new(Arc::new(server_config)).unwrap();
        let mut conn = rustls::StreamOwned::new(tls, socket);
        let mut request = Vec::new();
        let mut buf = [0_u8; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") && request.len() < 32768 {
            match conn.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
        }
        if request.windows(4).any(|w| w == b"\r\n\r\n") {
            conn.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
            let _ = conn.flush();
        }
        String::from_utf8_lossy(&request).into_owned()
    });
    let mut process = Command::new(std::env::current_exe().unwrap());
    process
        .arg("--exact")
        .arg("remote_https_with_trusted_ca_sends_bearer_authentication")
        .arg("--nocapture")
        .env("NULANG_TLS_TRUST_CHILD", "1")
        .env("NULANG_TLS_TEST_URL", url)
        // Process-local root trust: never modify the host certificate store.
        .env("SSL_CERT_FILE", &trust_path);
    let child = process.output().expect("run isolated child test");
    let request = server.join().expect("join TLS server");
    std::fs::remove_file(trust_path).expect("clean ephemeral certificate");
    assert!(child.status.success(), "child TLS test process failed");
    let bearer_arrived = request
        .to_ascii_lowercase()
        .contains(&format!("authorization: bearer {SECRET}"));
    assert_eq!(
        bearer_arrived, expect_bearer,
        "bearer delivery did not match the certificate validation expectation"
    );
}
