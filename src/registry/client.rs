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

/// Construct a shared, explicitly verified HTTP transport for registry/deploy.
/// ureq 2 native TLS uses OS trust, not obsolete bundled root certificates.
#[cfg(feature = "ureq")]
pub(crate) fn verified_https_agent(target_url: &str) -> Result<ureq::Agent, String> {
    let (scheme, authority) = target_url
        .split_once("://")
        .ok_or_else(|| "registry/deploy URL must specify http:// or https://".to_string())?;
    // Keep the authority bounded to the host portion, not the URL path.
    let authority = authority.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return Err("registry/deploy URL must have a host without userinfo".to_string());
    }
    let https_only = if scheme.eq_ignore_ascii_case("https") {
        true
    } else if scheme.eq_ignore_ascii_case("http") {
        // Plaintext is permitted only for exact loopback development hosts.
        // Never let a registry/deploy bearer token reach an arbitrary origin.
        let loopback = if let Some(ipv6) = authority.strip_prefix('[') {
            let (literal, suffix) = ipv6
                .split_once(']')
                .ok_or_else(|| "malformed bracketed loopback host".to_string())?;
            (suffix.is_empty() || suffix.starts_with(':'))
                && literal
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        } else {
            let (host, _) = authority.split_once(':').unwrap_or((authority, ""));
            // A hostname is not proof of loopback: local DNS/NSS rules may
            // resolve "localhost" to an external address. Require an IP literal.
            host.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        };
        if !loopback {
            return Err(
                "plaintext registry/deploy URLs require an exact loopback host".to_string(),
            );
        }
        false
    } else {
        return Err("registry/deploy URL must specify http:// or https://".to_string());
    };

    let connector = native_tls::TlsConnector::new()
        .map_err(|e| format!("system TLS initialization failed: {}", e))?;
    Ok(ureq::AgentBuilder::new()
        .https_only(https_only)
        // Local HTTP must not redirect bearer-bearing requests off loopback.
        // HTTPS requests retain normal HTTPS-only redirect behavior.
        .redirects(if https_only { 5 } else { 0 })
        .tls_connector(std::sync::Arc::new(connector))
        .build())
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
            let agent = verified_https_agent(&url)?;
            let request = match &self.token {
                Some(token) => agent
                    .put(&url)
                    .set("Authorization", &format!("Bearer {}", token)),
                None => agent.put(&url),
            };
            match request.send_bytes(tarball) {
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
            match verified_https_agent(&url)?.get(&url).call() {
                Ok(response) => {
                    let mut bytes = Vec::new();
                    response
                        .into_reader()
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
            match verified_https_agent(&url)?.get(&url).call() {
                Ok(response) => {
                    let text = response.into_string().map_err(|e| e.to_string())?;
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
            ureq::Error::Status(code, response) => {
                format!("HTTP {} {}", code, response.status_text())
            }
            other => other.to_string(),
        }
    }
}

#[cfg(all(test, feature = "ureq"))]
mod tls_transport_tests {
    use super::{verified_https_agent, RegistryClient};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn https_configured_agent_rejects_all_plaintext_requests() {
        // For HTTPS registries/deploys, following a Location: http://...
        // must be forbidden by the same HTTPS-only transport policy.
        let agent = verified_https_agent("https://registry.example.test").unwrap();
        let failure = agent
            .get("http://127.0.0.1:9/not-a-registry")
            .call()
            .expect_err("HTTPS-configured agent must reject plaintext URLs");
        assert!(
            failure.to_string().to_ascii_lowercase().contains("https"),
            "expected explicit HTTPS-only rejection, got: {failure}"
        );
    }

    #[test]
    fn explicitly_configured_local_http_remains_supported() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut buf = [0u8; 1024];
            let count = stream.read(&mut buf).unwrap();
            assert!(count > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .unwrap();
        });
        let result = verified_https_agent(&url)
            .unwrap()
            .get(&url)
            .call()
            .expect("explicit local HTTP should work");
        assert_eq!(result.status(), 200);
        server.join().unwrap();
    }

    #[test]
    fn test_registry_and_deploy_reject_remote_plaintext_and_userinfo() {
        // A configured package/deploy endpoint can send bearer tokens.
        // Only exact loopback authorities may use plaintext for local dev.
        for url in [
            "http://registry.example.test/api/v1/packages",
            "http://203.0.113.2:8080/api/v1/deploy",
            "http://localhost.evil.test/api/v1/deploy",
            "http://localhost:8080/api/v1/packages",
            "http://user@localhost:8080/api/v1/deploy",
            "http://localhost@evil.test/api/v1/deploy",
        ] {
            assert!(
                verified_https_agent(url).is_err(),
                "must reject plaintext remote or userinfo destination: {url}"
            );
        }
        for url in [
            "http://127.0.0.1:8080/api/v1/packages",
            "http://[::1]:8080/api/v1/packages",
        ] {
            assert!(
                verified_https_agent(url).is_ok(),
                "explicit loopback dev transport must remain supported: {url}"
            );
        }
    }

    #[test]
    fn test_loopback_http_does_not_follow_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut buf = [0u8; 2048];
            assert!(stream.read(&mut buf).unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://example.test/not-loopback\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
        });
        let response = verified_https_agent(&url)
            .unwrap()
            .get(&url)
            .call()
            .unwrap();
        assert_eq!(
            response.status(),
            302,
            "local redirect must not be followed"
        );
        server.join().unwrap();
    }

    #[test]
    fn transport_rejects_unsupported_or_missing_url_schemes() {
        for target in ["registry.example.test", "ftp://registry.example.test"] {
            assert!(
                verified_https_agent(target).is_err(),
                "unsupported scheme should not select an insecure transport: {target}"
            );
        }
    }

    #[test]
    fn registry_https_never_downgrades_to_plain_http() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback TLS sentinel");
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept HTTPS connection");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut first = [0u8; 5];
            let count = stream.read(&mut first).unwrap_or(0);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            assert!(count > 0, "client did not initiate TLS handshake");
            first[0]
        });
        let client = RegistryClient::new(format!("https://127.0.0.1:{}", port), None);
        assert!(
            client.fetch("test", "1.0.0").is_err(),
            "plain server must not be trusted as HTTPS"
        );
        assert_eq!(
            server.join().unwrap(),
            0x16,
            "expected a TLS ClientHello record"
        );
    }
}
