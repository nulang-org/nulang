//! Typed authority requests for runtime host operations.
//!
//! This module defines the small, backend-neutral vocabulary that host
//! integrations should authorize before touching the filesystem, environment,
//! secret store, or network. It intentionally does not perform I/O itself.
//! The VM/runtime dispatch sites still need to call these helpers before the
//! corresponding external effect becomes security-enforced end to end.

use crate::authority::AuthorityGrant;
use crate::runtime::Actor;
use crate::RuntimeAuthorityError;

/// One concrete external host action that requires actor authority.
///
/// Requests are exact: paths, environment-variable names, secret names, hosts,
/// and ports are not widened, globbed, or prefix-matched here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAuthorityRequest {
    TcpOut { host: String, port: u16 },
    FsRead { path: String },
    FsWrite { path: String },
    EnvRead { name: String },
    SecretRead { name: String },
}

impl HostAuthorityRequest {
    pub fn tcp_out(host: impl Into<String>, port: u16) -> Self {
        Self::TcpOut {
            host: host.into(),
            port,
        }
    }

    pub fn fs_read(path: impl Into<String>) -> Self {
        Self::FsRead { path: path.into() }
    }

    pub fn fs_write(path: impl Into<String>) -> Self {
        Self::FsWrite { path: path.into() }
    }

    pub fn env_read(name: impl Into<String>) -> Self {
        Self::EnvRead { name: name.into() }
    }

    pub fn secret_read(name: impl Into<String>) -> Self {
        Self::SecretRead { name: name.into() }
    }

    /// Convert the host operation into the exact external authority grant it
    /// requires.
    ///
    /// HTTP integrations should extract their actual destination host/port and
    /// use `TcpOut`; source-level `Net::TcpOut` authority therefore governs the
    /// underlying outbound network connection rather than an unrelated `Http`
    /// string capability.
    pub fn required_grant(&self) -> AuthorityGrant {
        match self {
            Self::TcpOut { host, port } => AuthorityGrant::NetTcpOut {
                host: host.clone(),
                port: *port,
            },
            Self::FsRead { path } => AuthorityGrant::FsRead { path: path.clone() },
            Self::FsWrite { path } => AuthorityGrant::FsWrite { path: path.clone() },
            Self::EnvRead { name } => AuthorityGrant::EnvRead { name: name.clone() },
            Self::SecretRead { name } => AuthorityGrant::SecretRead { name: name.clone() },
        }
    }
}

impl Actor {
    /// Require the exact authority needed for one external host action.
    ///
    /// This delegates to `Actor::require_authority`, which first parses the
    /// complete compatibility token set into a typed manifest. A malformed
    /// sibling token therefore invalidates authorization instead of being
    /// ignored.
    pub fn require_host_authority(
        &self,
        request: &HostAuthorityRequest,
    ) -> Result<(), RuntimeAuthorityError> {
        self.require_authority(&request.required_grant())
    }

    pub fn require_fs_read(&self, path: &str) -> Result<(), RuntimeAuthorityError> {
        self.require_host_authority(&HostAuthorityRequest::fs_read(path))
    }

    pub fn require_fs_write(&self, path: &str) -> Result<(), RuntimeAuthorityError> {
        self.require_host_authority(&HostAuthorityRequest::fs_write(path))
    }

    pub fn require_env_read(&self, name: &str) -> Result<(), RuntimeAuthorityError> {
        self.require_host_authority(&HostAuthorityRequest::env_read(name))
    }

    pub fn require_secret_read(&self, name: &str) -> Result<(), RuntimeAuthorityError> {
        self.require_host_authority(&HostAuthorityRequest::secret_read(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::AuthorityManifest;

    fn actor_with(tokens: &[&str]) -> Actor {
        let mut actor = Actor::new(7, "host-authority-test", 16);
        let manifest = AuthorityManifest::from_tokens(tokens.iter().copied()).unwrap();
        actor.install_authority_manifest(&manifest);
        actor
    }

    #[test]
    fn host_request_maps_to_exact_grant() {
        assert_eq!(
            HostAuthorityRequest::fs_read("/srv/data/report.csv").required_grant(),
            AuthorityGrant::FsRead {
                path: "/srv/data/report.csv".into(),
            }
        );
        assert_eq!(
            HostAuthorityRequest::tcp_out("api.stripe.com", 443).required_grant(),
            AuthorityGrant::NetTcpOut {
                host: "api.stripe.com".into(),
                port: 443,
            }
        );
    }

    #[test]
    fn exact_fs_authority_is_required() {
        let actor = actor_with(&["Fs::Read(/srv/data/report.csv)"]);
        assert!(actor.require_fs_read("/srv/data/report.csv").is_ok());
        assert!(matches!(
            actor.require_fs_read("/srv/data/other.csv"),
            Err(RuntimeAuthorityError::Denied(_))
        ));
    }

    #[test]
    fn read_does_not_imply_write() {
        let actor = actor_with(&["Fs::Read(/srv/data/report.csv)"]);
        assert!(matches!(
            actor.require_fs_write("/srv/data/report.csv"),
            Err(RuntimeAuthorityError::Denied(_))
        ));
    }

    #[test]
    fn env_and_secret_authority_are_distinct() {
        let actor = actor_with(&["Env::Read(API_URL)", "Secret::Read(STRIPE_KEY)"]);
        assert!(actor.require_env_read("API_URL").is_ok());
        assert!(actor.require_secret_read("STRIPE_KEY").is_ok());
        assert!(matches!(
            actor.require_secret_read("API_URL"),
            Err(RuntimeAuthorityError::Denied(_))
        ));
    }

    #[test]
    fn host_authority_is_deny_by_default() {
        let actor = actor_with(&[]);
        assert!(matches!(
            actor.require_host_authority(&HostAuthorityRequest::tcp_out("example.com", 443)),
            Err(RuntimeAuthorityError::Denied(_))
        ));
    }

    #[test]
    fn malformed_runtime_manifest_still_fails_closed() {
        let mut actor = actor_with(&["Fs::Read(/srv/data/report.csv)"]);
        actor
            .capabilities
            .insert("Net::TcpOut(malformed)".to_string());

        assert!(matches!(
            actor.require_fs_read("/srv/data/report.csv"),
            Err(RuntimeAuthorityError::InvalidManifest(_))
        ));
    }
}
