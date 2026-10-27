//! An opt-in, read-only preflight for the modern Turso SQL-over-HTTPS client.
//!
//! This module is deliberately NOT a PersistenceStore, not a remote durable
//! commit implementation, and cannot change the default libSQL backend.
//! It verifies a caller-supplied URL and credential *before* network access.
use std::{fmt, io};
use url::Url;

/// An explicitly configured, read-only remote SQL transport probe.
///
/// Only HTTPS is permitted. Standard libsql/turso URL aliases are normalized
/// to HTTPS before constructing the serverless driver. Authentication is
/// supplied by the caller, never read from ambient environment variables.
pub struct TursoRemotePreflight {
    endpoint: String,
    auth_token: String,
}

impl fmt::Debug for TursoRemotePreflight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TursoRemotePreflight")
            .field("endpoint", &self.endpoint)
            .field("auth_token", &"[REDACTED]")
            .finish()
    }
}

impl TursoRemotePreflight {
    /// Validate a remote URL without making a network request.
    ///
    /// Reject URL-embedded credentials, non-HTTPS schemes, query strings,
    /// fragments, and database paths to avoid accidental credential leaks.
    /// The accepted `libsql://` and `turso://` schemes mean HTTPS, NOT an
    /// embedded SQLite replica or a persistent transaction session.
    pub fn new(endpoint: &str, auth_token: &str) -> io::Result<Self> {
        let url = Url::parse(endpoint).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid remote database URL")
        })?;
        if !matches!(url.scheme(), "https" | "libsql" | "turso")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote database URL must use HTTPS without credentials, query, fragment or path",
            ));
        }
        if auth_token.is_empty()
            || auth_token.trim() != auth_token
            || auth_token.bytes().any(|b| b.is_ascii_control())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "remote database authorization token is required and must be a valid header value",
            ));
        }

        // Url::set_scheme cannot switch an unknown (non-special) scheme to
        // HTTPS, so create a *new* HTTPS URL from the already-validated host
        // representation. The original query/userinfo/path were rejected.
        let secure = if url.scheme() == "https" {
            url
        } else {
            let source = url.as_str();
            let (_, rest) = source.split_once("://").ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid remote URL scheme")
            })?;
            Url::parse(&format!("https://{rest}")).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "remote HTTPS URL normalization failed",
                )
            })?
        };
        Ok(Self {
            endpoint: secure.as_str().trim_end_matches('/').to_owned(),
            auth_token: auth_token.to_owned(),
        })
    }

    /// Perform a single read-only network preflight over TLS using the modern
    /// serverless Turso driver. No schema writes, transactions, or durable
    /// actor state are performed.
    ///
    /// The upstream driver's error messages might include HTTP details.
    /// Deliberately return a generic error rather than leak any credential.
    pub async fn verify_tls_auth(&self) -> io::Result<()> {
        let db = turso_serverless::Builder::new_remote(self.endpoint.clone())
            .with_auth_token(self.auth_token.clone())
            .build()
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "remote transport preflight failed",
                )
            })?;
        let conn = db.connect().map_err(|_| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "remote transport preflight failed",
            )
        })?;
        let mut rows = conn.query("SELECT 1", ()).await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "remote transport preflight failed",
            )
        })?;
        let row = rows
            .next()
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected remote preflight response",
                )
            })?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "empty remote preflight response",
                )
            })?;
        if row.get::<i64>(0).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected remote preflight response",
            )
        })? != 1
            || rows
                .next()
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unexpected remote preflight response",
                    )
                })?
                .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected remote preflight response",
            ));
        }
        Ok(())
    }
}
