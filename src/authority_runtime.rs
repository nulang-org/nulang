//! Runtime bridge from the actor's legacy canonical-token set to typed authority.
//!
//! The actor runtime currently persists spawn authority as `BTreeSet<String>`.
//! That representation remains a compatibility boundary, but host operations
//! must not make authorization decisions by matching ad-hoc strings. These
//! helpers parse the complete set into [`AuthorityManifest`] first, so one
//! malformed persisted token invalidates the manifest and therefore fails
//! authorization closed.

use crate::authority::{AuthorityGrant, AuthorityManifest, AuthorityParseError};
use crate::runtime::Actor;
use std::error::Error;
use std::fmt;

/// Failure while authorizing an external action for an actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAuthorityError {
    /// The persisted/runtime token set is malformed. Treating malformed state
    /// as an empty or partially valid manifest would be ambiguous, so the
    /// entire manifest is rejected.
    InvalidManifest(AuthorityParseError),
    /// The manifest is structurally valid but does not contain the exact grant
    /// required for the requested external action.
    Denied(AuthorityGrant),
}

impl fmt::Display for RuntimeAuthorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeAuthorityError::InvalidManifest(err) => {
                write!(f, "invalid actor authority manifest: {err}")
            }
            RuntimeAuthorityError::Denied(grant) => {
                write!(f, "capability denied: {grant}")
            }
        }
    }
}

impl Error for RuntimeAuthorityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            RuntimeAuthorityError::InvalidManifest(err) => Some(err),
            RuntimeAuthorityError::Denied(_) => None,
        }
    }
}

impl From<AuthorityParseError> for RuntimeAuthorityError {
    fn from(value: AuthorityParseError) -> Self {
        Self::InvalidManifest(value)
    }
}

impl Actor {
    /// Parse this actor's compatibility token set into the typed authority
    /// representation. Invalid persisted/runtime authority fails closed.
    pub fn authority_manifest(&self) -> Result<AuthorityManifest, AuthorityParseError> {
        AuthorityManifest::from_token_set(&self.capabilities)
    }

    /// Return whether this actor holds an exact typed grant.
    ///
    /// This returns an error, rather than `false`, for malformed manifests so
    /// callers cannot accidentally hide corrupted or attacker-controlled
    /// authority metadata behind an ordinary denial.
    pub fn allows_authority(
        &self,
        grant: &AuthorityGrant,
    ) -> Result<bool, AuthorityParseError> {
        Ok(self.authority_manifest()?.allows(grant))
    }

    /// Require an exact typed grant for a host-boundary action.
    ///
    /// Host integrations should prefer this method to direct access to
    /// `Actor::capabilities`: it parses the complete manifest first and only
    /// then performs the typed exact-match authorization decision.
    pub fn require_authority(
        &self,
        grant: &AuthorityGrant,
    ) -> Result<(), RuntimeAuthorityError> {
        let manifest = self.authority_manifest()?;
        if manifest.allows(grant) {
            Ok(())
        } else {
            Err(RuntimeAuthorityError::Denied(grant.clone()))
        }
    }

    /// Require permission to open one outbound TCP connection.
    pub fn require_tcp_out(
        &self,
        host: &str,
        port: u16,
    ) -> Result<(), RuntimeAuthorityError> {
        self.require_authority(&AuthorityGrant::NetTcpOut {
            host: host.to_string(),
            port,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor_with(tokens: &[&str]) -> Actor {
        let mut actor = Actor::new(7, "authority-test", 16);
        actor.capabilities = tokens.iter().map(|token| (*token).to_string()).collect();
        actor
    }

    #[test]
    fn actor_authority_is_deny_by_default() {
        let actor = actor_with(&[]);
        let err = actor.require_tcp_out("api.stripe.com", 443).unwrap_err();
        assert_eq!(
            err,
            RuntimeAuthorityError::Denied(AuthorityGrant::NetTcpOut {
                host: "api.stripe.com".into(),
                port: 443,
            })
        );
        assert_eq!(
            err.to_string(),
            "capability denied: Net::TcpOut(api.stripe.com:443)"
        );
    }

    #[test]
    fn actor_allows_only_exact_tcp_destination() {
        let actor = actor_with(&["Net::TcpOut(api.stripe.com:443)"]);
        assert!(actor.require_tcp_out("api.stripe.com", 443).is_ok());
        assert!(matches!(
            actor.require_tcp_out("api.stripe.com", 80),
            Err(RuntimeAuthorityError::Denied(_))
        ));
        assert!(matches!(
            actor.require_tcp_out("example.com", 443),
            Err(RuntimeAuthorityError::Denied(_))
        ));
    }

    #[test]
    fn malformed_actor_manifest_fails_closed_before_authorization() {
        let actor = actor_with(&[
            "Net::TcpOut(api.stripe.com:443)",
            "Net::TcpOut(malformed)",
        ]);

        // Even though the exact requested grant is also present, the malformed
        // sibling token invalidates the manifest. Partial parsing must never
        // turn corrupt authority state into permission.
        assert!(matches!(
            actor.require_tcp_out("api.stripe.com", 443),
            Err(RuntimeAuthorityError::InvalidManifest(_))
        ));
    }

    #[test]
    fn generic_actor_authority_check_uses_typed_grants() {
        let actor = actor_with(&["Secret::Read(STRIPE_KEY)"]);
        let allowed = AuthorityGrant::SecretRead {
            name: "STRIPE_KEY".into(),
        };
        let denied = AuthorityGrant::SecretRead {
            name: "OTHER_KEY".into(),
        };
        assert_eq!(actor.allows_authority(&allowed).unwrap(), true);
        assert_eq!(actor.allows_authority(&denied).unwrap(), false);
    }
}
