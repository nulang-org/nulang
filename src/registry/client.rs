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

/// An archive already authenticated against a project-pinned publisher key.
/// Retain the detached signature for subsequent offline provenance checks.
pub struct VerifiedRegistryArchive {
    pub bytes: Vec<u8>,
    pub signature: String,
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

    /// Publish bytes with the project publisher's detached signature.
    /// The registry transports the signature, but never chooses trust roots.
    pub fn publish_signed(
        &self,
        name: &str,
        version: &str,
        tarball: &[u8],
        signature: &str,
    ) -> Result<(), String> {
        #[cfg(feature = "ureq")]
        {
            if signature.len() != 128 || !signature.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("publisher signature must be 128 hexadecimal characters".to_string());
            }
            let url = format!("{}/api/v1/packages/{}/{}", self.base_url(), name, version);
            let request = ureq::put(&url)
                .set(crate::registry::trust::SIGNATURE_HEADER, signature);
            let request = match &self.token {
                Some(token) => request.set("Authorization", &format!("Bearer {}", token)),
                None => request,
            };
            match request.send_bytes(tarball) {
                Ok(_) => Ok(()),
                Err(err) => Err(Self::describe_error(err)),
            }
        }
        #[cfg(not(feature = "ureq"))]
        {
            let _ = (&self.registry_url, &self.token, name, version, tarball, signature);
            Err("registry client disabled (feature 'ureq' not enabled)".to_string())
        }
    }

    /// Fetch only when the compressed archive has a valid publisher signature
    /// under a public key pinned in <project>/Nulang.trust.toml. No TOFU.
    pub fn fetch_verified(
        &self,
        name: &str,
        version: &str,
        project_root: &std::path::Path,
    ) -> Result<VerifiedRegistryArchive, String> {
        #[cfg(feature = "ureq")]
        {
            let keys = crate::registry::trust::trusted_publisher_keys(
                project_root,
                &self.registry_url,
            )?;
            let url = format!("{}/api/v1/packages/{}/{}", self.base_url(), name, version);
            let response = ureq::get(&url).call().map_err(Self::describe_error)?;
            let signature = response
                .header(crate::registry::trust::SIGNATURE_HEADER)
                .ok_or_else(|| format!(
                    "registry archive '{}@{}' has no publisher signature",
                    name, version
                ))?
                .to_string();

            let limit = crate::package::registry_archive::MAX_REGISTRY_ARCHIVE_BYTES;
            let mut reader = response.into_reader().take(limit as u64 + 1);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            if bytes.len() > limit {
                return Err(format!("registry archive exceeds {} byte limit", limit));
            }
            crate::registry::trust::verify_publisher_signature(
                &self.registry_url,
                name,
                version,
                &bytes,
                &signature,
                &keys,
            )?;
            Ok(VerifiedRegistryArchive { bytes, signature })
        }
        #[cfg(not(feature = "ureq"))]
        {
            let _ = (&self.registry_url, &self.token, name, version, project_root);
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
                    // The registry response is untrusted. Bound the compressed
                    // payload before attempting to decompress or extract it.
                    let limit = crate::package::registry_archive::MAX_REGISTRY_ARCHIVE_BYTES;
                    let mut reader = response.into_reader().take(limit as u64 + 1);
                    let mut bytes = Vec::new();
                    reader.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                    if bytes.len() > limit {
                        return Err(format!(
                            "registry archive exceeds maximum download size of {} bytes",
                            limit
                        ));
                    }
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
