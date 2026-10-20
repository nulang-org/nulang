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
pub(crate) fn verified_https_agent() -> Result<ureq::Agent, String> {
    let connector = native_tls::TlsConnector::new()
        .map_err(|e| format!("system TLS initialization failed: {}", e))?;
    Ok(ureq::AgentBuilder::new()
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
            let agent = verified_https_agent()?;
            let request = match &self.token {
                Some(token) => agent.put(&url).set("Authorization", &format!("Bearer {}", token)),
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
            match verified_https_agent()?.get(&url).call() {
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
            match verified_https_agent()?.get(&url).call() {
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
    use super::RegistryClient;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn registry_https_never_downgrades_to_plain_http() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback TLS sentinel");
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept HTTPS connection");
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut first = [0u8; 5];
            let count = stream.read(&mut first).unwrap_or(0);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            assert!(count > 0, "client did not initiate TLS handshake");
            first[0]
        });
        let client = RegistryClient::new(format!("https://127.0.0.1:{}", port), None);
        assert!(client.fetch("test", "1.0.0").is_err(), "plain server must not be trusted as HTTPS");
        assert_eq!(server.join().unwrap(), 0x16, "expected a TLS ClientHello record");
    }
}
