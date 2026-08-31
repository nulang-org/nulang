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
            let request = match &self.token {
                Some(token) => ureq::put(&url).set("Authorization", &format!("Bearer {}", token)),
                None => ureq::put(&url),
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
            match ureq::get(&url).call() {
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
            match ureq::get(&url).call() {
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
