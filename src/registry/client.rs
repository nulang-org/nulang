//! Synchronous HTTP client for the package registry, built on `ureq`.
//!
//! Talks to `crate::registry::server::RegistryServer` over the registry API:
//! `PUT /api/v1/packages/<name>/<version>` to publish, `GET ...` to fetch or
//! list versions. Non-2xx responses surface as `Err(String)`.
//!
//! When the `ureq` feature is disabled, the client methods return
//! `Err("registry client disabled ...")`; `new` still constructs so callers
//! (the resolver, `nula publish`) compile unchanged.

#[cfg(feature = "ureq")]
use std::io::Read;

/// Never transmit a bearer token to a plaintext or malformed endpoint.
/// Deliberately fail closed instead of relying on an HTTP client's defaults.
#[cfg(feature = "ureq")]
pub(crate) fn require_https_for_bearer(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("HTTPS is required for bearer-token requests".to_string());
    }
    let authority = url["https://".len()..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    if authority.is_empty()
        || authority.starts_with(':')
        || authority.contains('@')
        || authority.chars().any(char::is_whitespace)
    {
        return Err("HTTPS bearer-token endpoint requires a valid host without userinfo".into());
    }
    Ok(())
}

/// Client for a package registry server.
pub struct RegistryClient {
    registry_url: String,
    token: Option<String>,
}

impl RegistryClient {
    pub fn new(registry_url: String, token: Option<String>) -> Self {
        RegistryClient {
            registry_url,
            token,
        }
    }

    #[cfg(feature = "ureq")]
    fn base_url(&self) -> String {
        self.registry_url.trim_end_matches('/').to_string()
    }

    /// Publish a tarball for `name@version`. Returns `Ok(())` on 201 Created;
    /// a 409 Conflict (version already exists) or other failure is `Err`.
    pub fn publish(&self, name: &str, version: &str, tarball: &[u8]) -> Result<(), String> {
        #[cfg(feature = "ureq")]
        {
            let url = format!("{}/api/v1/packages/{}/{}", self.base_url(), name, version);
            if self.token.is_some() {
                require_https_for_bearer(&url)?;
            }
            let request = match &self.token {
                Some(token) => ureq::put(&url).header("Authorization", format!("Bearer {}", token)),
                None => ureq::put(&url),
            };
            match request.send(tarball) {
                Ok(_) => Ok(()),
                Err(err) => Err(Self::describe_error(err)),
            }
        }
        #[cfg(not(feature = "ureq"))]
        {
            let _ = (&self.registry_url, &self.token, name, version, tarball);
            Err("registry client disabled (feature 'ureq' not enabled)".to_string())
        }
    }

    /// Fetch the stored tarball for `name@version`.
    pub fn fetch(&self, name: &str, version: &str) -> Result<Vec<u8>, String> {
        #[cfg(feature = "ureq")]
        {
            let url = format!("{}/api/v1/packages/{}/{}", self.base_url(), name, version);
            match ureq::get(&url).call() {
                Ok(mut response) => {
                    // Preserve ureq 2's streaming tarball behavior: ureq 3's
                    // read_to_vec() imposes a new default 10 MiB size limit.
                    let mut bytes = Vec::new();
                    response
                        .body_mut()
                        .as_reader()
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    Ok(bytes)
                }
                Err(err) => Err(Self::describe_error(err)),
            }
        }
        #[cfg(not(feature = "ureq"))]
        {
            let _ = (&self.registry_url, &self.token, name, version);
            Err("registry client disabled (feature 'ureq' not enabled)".to_string())
        }
    }

    /// List published versions of `name`.
    pub fn list_versions(&self, name: &str) -> Result<Vec<String>, String> {
        #[cfg(feature = "ureq")]
        {
            let url = format!("{}/api/v1/packages/{}", self.base_url(), name);
            match ureq::get(&url).call() {
                Ok(mut response) => {
                    let text = response
                        .body_mut()
                        .read_to_string()
                        .map_err(|e| e.to_string())?;
                    Self::parse_versions(&text)
                }
                Err(err) => Err(Self::describe_error(err)),
            }
        }
        #[cfg(not(feature = "ureq"))]
        {
            let _ = (&self.registry_url, &self.token, name);
            Err("registry client disabled (feature 'ureq' not enabled)".to_string())
        }
    }

    /// Parse the version list JSON: either the server's
    /// `{"name": ..., "versions": [...]}` object or a bare JSON array.
    #[cfg(feature = "ureq")]
    fn parse_versions(text: &str) -> Result<Vec<String>, String> {
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| format!("invalid JSON response from registry: {}", e))?;
        let versions: Vec<String> = match &value {
            serde_json::Value::Array(items) => items
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            serde_json::Value::Object(map) => map
                .get("versions")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            _ => return Err("unexpected JSON response from registry".to_string()),
        };
        Ok(versions)
    }

    #[cfg(feature = "ureq")]
    fn describe_error(err: ureq::Error) -> String {
        match err {
            ureq::Error::StatusCode(code) => format!("HTTP {}", code),
            other => other.to_string(),
        }
    }
}

#[cfg(all(test, feature = "ureq"))]
mod ureq3_regressions {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn authenticated_publish_rejects_plaintext_before_connect() {
        let client = RegistryClient::new(
            "http://127.0.0.1:1".into(),
            Some("fake-test-credential".into()),
        );
        let error = client
            .publish("demo", "1.0", b"fixture")
            .expect_err("authenticated publishing must reject HTTP");
        assert!(
            error.contains("HTTPS"),
            "credential guard should fail before network I/O: {error}"
        );
    }

    #[test]
    fn authenticated_endpoint_requires_https_and_host() {
        for rejected in [
            "http://registry.example",
            "http://127.0.0.1:8080",
            "ftp://registry.example",
            "https://",
            "https:///missing-host",
            "https://user:pass@registry.example",
        ] {
            assert!(require_https_for_bearer(rejected).is_err(), "{rejected}");
        }
        assert!(require_https_for_bearer("https://registry.example/api/v1/packages").is_ok());
    }

    #[test]
    fn registry_publish_fetch_versions_and_status_are_preserved() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for i in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut len = 0;
                let mut auth = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length: ") {
                        len = v.trim().parse().unwrap();
                    }
                    if line.to_ascii_lowercase().starts_with("authorization: ") {
                        auth = line.trim().to_string();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let (expected, status, answer) = match i {
                    0 => (
                        "PUT /api/v1/packages/demo/1.0",
                        "201 Created",
                        b"".as_slice(),
                    ),
                    1 => (
                        "GET /api/v1/packages/demo/1.0",
                        "200 OK",
                        b"binary tarball".as_slice(),
                    ),
                    2 => (
                        "GET /api/v1/packages/demo ",
                        "200 OK",
                        br#"{"versions":["1.0"]}"#.as_slice(),
                    ),
                    _ => (
                        "PUT /api/v1/packages/demo/1.0",
                        "409 Conflict",
                        b"duplicate".as_slice(),
                    ),
                };
                if i == 2 {
                    assert!(
                        request.starts_with("GET /api/v1/packages/demo HTTP/"),
                        "{request}"
                    );
                } else {
                    assert!(request.starts_with(expected), "{request}");
                }
                if i == 0 || i == 3 {
                    assert!(
                        auth.is_empty(),
                        "unauthenticated HTTP must not send a bearer token"
                    );
                    assert_eq!(body, b"test-tarball");
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    answer.len()
                );
                socket.write_all(response.as_bytes()).unwrap();
                socket.write_all(answer).unwrap();
            }
        });
        let client = RegistryClient::new(format!("http://{address}"), None);
        client.publish("demo", "1.0", b"test-tarball").unwrap();
        assert_eq!(client.fetch("demo", "1.0").unwrap(), b"binary tarball");
        assert_eq!(client.list_versions("demo").unwrap(), vec!["1.0"]);
        assert!(client
            .publish("demo", "1.0", b"test-tarball")
            .unwrap_err()
            .contains("409"));
        server.join().unwrap();
    }

    #[test]
    fn ureq_rustls_rejects_untrusted_tls_server() {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let fixture = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
            .expect("generate test-only untrusted certificate");
        let cert = CertificateDer::from(fixture.cert.der().as_ref().to_vec());
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            fixture.signing_key.serialize_der(),
        ));
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let conn = rustls::ServerConnection::new(std::sync::Arc::new(config)).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, socket);
            let mut request = [0; 4096];
            // If client incorrectly trusts the self-signed cert, answer 200
            // so the test fails rather than passing on a missing HTTP reply.
            if tls.read(&mut request).is_ok() {
                let _ = tls.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                );
                let _ = tls.flush();
            }
        });
        assert!(
            ureq::get(format!("https://127.0.0.1:{}/", addr.port()))
                .call()
                .is_err(),
            "self-signed certificate must not be accepted by the ureq 3 TLS client"
        );
        server.join().unwrap();
    }
}
