//! Runtime bridge from the actor's legacy canonical-token set to typed authority.
//!
//! The actor runtime currently persists spawn authority as `BTreeSet<String>`.
//! That representation remains a compatibility boundary, but host operations
//! must not make authorization decisions by matching ad-hoc strings. These
//! helpers parse the complete set into [`AuthorityManifest`] first, so one
//! malformed persisted token invalidates the manifest and therefore fails
//! authorization closed.
//!
//! This module is a migration boundary, not proof that spawn authority is
//! end-to-end wired. Parser, MIR-codegen metadata emission, VM spawn callback
//! plumbing, and host-boundary enforcement must all preserve/use the manifest
//! before source-level grants are security-effective.

use crate::authority::{AuthorityGrant, AuthorityManifest, AuthorityParseError};
use crate::bytecode::CodeModule;
use crate::runtime::Actor;
use std::error::Error;
use std::fmt;

/// Failure while authorizing, decoding, or delegating external authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAuthorityError {
    /// The persisted/runtime token set is malformed. Treating malformed state
    /// as an empty or partially valid manifest would be ambiguous, so the
    /// entire manifest is rejected.
    InvalidManifest(AuthorityParseError),
    /// More than one grant record targets the same spawn instruction. The
    /// metadata is ambiguous and therefore cannot safely authorize anything.
    AmbiguousSpawnMetadata { pc: usize },
    /// The manifest is structurally valid but does not contain the exact grant
    /// required for the requested external action or delegation.
    Denied(AuthorityGrant),
}

impl fmt::Display for RuntimeAuthorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeAuthorityError::InvalidManifest(err) => {
                write!(f, "invalid actor authority manifest: {err}")
            }
            RuntimeAuthorityError::AmbiguousSpawnMetadata { pc } => {
                write!(f, "ambiguous spawn authority metadata at bytecode pc {pc}")
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
            RuntimeAuthorityError::AmbiguousSpawnMetadata { .. }
            | RuntimeAuthorityError::Denied(_) => None,
        }
    }
}

impl From<AuthorityParseError> for RuntimeAuthorityError {
    fn from(value: AuthorityParseError) -> Self {
        Self::InvalidManifest(value)
    }
}

/// Decode the authority attached to one bytecode `Spawn` instruction.
///
/// Missing metadata is an empty manifest (deny by default). Exactly one entry
/// is parsed through the typed authority boundary. Multiple entries for the
/// same PC are rejected instead of choosing one arbitrarily, because metadata
/// ambiguity at a security boundary must fail closed.
///
/// This is staged migration plumbing and becomes a normal runtime call once
/// exact spawn provenance is carried through `ActorVmCallbacks::spawn_actor`.
pub fn spawn_authority_manifest(
    module: &CodeModule,
    spawn_pc: usize,
) -> Result<AuthorityManifest, RuntimeAuthorityError> {
    let mut matches = module
        .spawn_capability_grants
        .iter()
        .filter(|(pc, _)| *pc == spawn_pc);

    let Some((_, tokens)) = matches.next() else {
        return Ok(AuthorityManifest::new());
    };
    if matches.next().is_some() {
        return Err(RuntimeAuthorityError::AmbiguousSpawnMetadata { pc: spawn_pc });
    }

    Ok(AuthorityManifest::from_tokens(
        tokens.iter().map(String::as_str),
    )?)
}

impl Actor {
    /// Parse this actor's compatibility token set into the typed authority
    /// representation. Invalid persisted/runtime authority fails closed.
    pub fn authority_manifest(&self) -> Result<AuthorityManifest, AuthorityParseError> {
        AuthorityManifest::from_token_set(&self.capabilities)
    }

    /// Replace this actor's compatibility token set from a validated typed
    /// manifest. Raw token insertion should stay confined to serialization and
    /// migration code; semantic runtime code should install manifests here.
    pub fn install_authority_manifest(&mut self, manifest: &AuthorityManifest) {
        self.capabilities = manifest.canonical_token_set();
    }

    /// Return whether this actor holds an exact typed grant.
    ///
    /// This returns an error, rather than `false`, for malformed manifests so
    /// callers cannot accidentally hide corrupted or attacker-controlled
    /// authority metadata behind an ordinary denial.
    pub fn allows_authority(&self, grant: &AuthorityGrant) -> Result<bool, AuthorityParseError> {
        Ok(self.authority_manifest()?.allows(grant))
    }

    /// Require an exact typed grant for a host-boundary action.
    ///
    /// Host integrations should prefer this method to direct access to
    /// `Actor::capabilities`: it parses the complete manifest first and only
    /// then performs the typed exact-match authorization decision.
    pub fn require_authority(&self, grant: &AuthorityGrant) -> Result<(), RuntimeAuthorityError> {
        let manifest = self.authority_manifest()?;
        if manifest.allows(grant) {
            Ok(())
        } else {
            Err(RuntimeAuthorityError::Denied(grant.clone()))
        }
    }

    /// Require permission to open one outbound TCP connection.
    pub fn require_tcp_out(&self, host: &str, port: u16) -> Result<(), RuntimeAuthorityError> {
        self.require_authority(&AuthorityGrant::NetTcpOut {
            host: host.to_string(),
            port,
        })
    }

    /// Validate a child/delegated manifest against this actor's authority.
    ///
    /// Delegation is monotonic: the child may receive equal or less exact
    /// authority, never a grant the parent does not already hold. Returning an
    /// error for the first missing grant makes accidental privilege escalation
    /// observable instead of silently intersecting the request.
    pub fn delegate_authority(
        &self,
        requested: &AuthorityManifest,
    ) -> Result<AuthorityManifest, RuntimeAuthorityError> {
        let parent = self.authority_manifest()?;
        if let Some(missing) = requested.iter().find(|grant| !parent.allows(grant)) {
            return Err(RuntimeAuthorityError::Denied(missing.clone()));
        }
        Ok(requested.clone())
    }

    /// Validate and install a requested manifest on a child actor.
    ///
    /// This is the runtime spawn primitive: the caller does not touch the
    /// child's raw token set, and installation only happens after monotonic
    /// delegation succeeds in full.
    pub fn delegate_authority_to(
        &self,
        child: &mut Actor,
        requested: &AuthorityManifest,
    ) -> Result<(), RuntimeAuthorityError> {
        let delegated = self.delegate_authority(requested)?;
        child.install_authority_manifest(&delegated);
        Ok(())
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
    fn missing_spawn_metadata_is_deny_by_default() {
        let module = CodeModule::new("authority-metadata");
        let manifest = spawn_authority_manifest(&module, 12).unwrap();
        assert!(manifest.is_empty());
    }

    #[test]
    fn spawn_metadata_decodes_to_typed_manifest() {
        let mut module = CodeModule::new("authority-metadata");
        module.spawn_capability_grants.push((
            12,
            vec![
                "Net::TcpOut(api.stripe.com:443)".to_string(),
                "Secret::Read(STRIPE_KEY)".to_string(),
            ],
        ));

        let manifest = spawn_authority_manifest(&module, 12).unwrap();
        assert!(manifest.allows_tcp_out("api.stripe.com", 443));
        assert!(manifest.allows(&AuthorityGrant::SecretRead {
            name: "STRIPE_KEY".into(),
        }));
    }

    #[test]
    fn malformed_spawn_metadata_fails_closed() {
        let mut module = CodeModule::new("authority-metadata");
        module
            .spawn_capability_grants
            .push((12, vec!["Net::TcpOut(malformed)".to_string()]));

        assert!(matches!(
            spawn_authority_manifest(&module, 12),
            Err(RuntimeAuthorityError::InvalidManifest(_))
        ));
    }

    #[test]
    fn duplicate_spawn_metadata_for_same_pc_is_rejected() {
        let mut module = CodeModule::new("authority-metadata");
        module
            .spawn_capability_grants
            .push((12, vec!["Secret::Read(FIRST)".to_string()]));
        module
            .spawn_capability_grants
            .push((12, vec!["Secret::Read(SECOND)".to_string()]));

        assert_eq!(
            spawn_authority_manifest(&module, 12),
            Err(RuntimeAuthorityError::AmbiguousSpawnMetadata { pc: 12 })
        );
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
        let actor = actor_with(&["Net::TcpOut(api.stripe.com:443)", "Net::TcpOut(malformed)"]);

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
        assert!(actor.allows_authority(&allowed).unwrap());
        assert!(!actor.allows_authority(&denied).unwrap());
    }

    #[test]
    fn delegation_cannot_manufacture_authority() {
        let parent = actor_with(&[
            "Net::TcpOut(api.stripe.com:443)",
            "Secret::Read(STRIPE_KEY)",
        ]);
        let allowed = AuthorityManifest::from_tokens(["Net::TcpOut(api.stripe.com:443)"]).unwrap();
        let escalation = AuthorityManifest::from_tokens([
            "Net::TcpOut(api.stripe.com:443)",
            "Secret::Read(OTHER_KEY)",
        ])
        .unwrap();

        assert_eq!(parent.delegate_authority(&allowed).unwrap(), allowed);
        assert_eq!(
            parent.delegate_authority(&escalation),
            Err(RuntimeAuthorityError::Denied(AuthorityGrant::SecretRead {
                name: "OTHER_KEY".into(),
            }))
        );
    }

    #[test]
    fn delegation_installs_only_after_full_validation() {
        let parent = actor_with(&[
            "Net::TcpOut(api.stripe.com:443)",
            "Secret::Read(STRIPE_KEY)",
        ]);
        let requested = AuthorityManifest::from_tokens(["Secret::Read(STRIPE_KEY)"]).unwrap();
        let escalation = AuthorityManifest::from_tokens(["Secret::Read(OTHER_KEY)"]).unwrap();
        let mut child = Actor::new(8, "child", 16);

        parent
            .delegate_authority_to(&mut child, &requested)
            .unwrap();
        assert_eq!(child.authority_manifest().unwrap(), requested);
        assert_eq!(
            child.capabilities,
            std::collections::BTreeSet::from(["Secret::Read(STRIPE_KEY)".to_string()])
        );

        let before = child.capabilities.clone();
        assert!(matches!(
            parent.delegate_authority_to(&mut child, &escalation),
            Err(RuntimeAuthorityError::Denied(_))
        ));
        assert_eq!(child.capabilities, before);
    }

    #[test]
    fn malformed_parent_cannot_delegate_even_an_exact_present_grant() {
        let parent = actor_with(&["Secret::Read(STRIPE_KEY)", "Net::TcpOut(malformed)"]);
        let requested = AuthorityManifest::from_tokens(["Secret::Read(STRIPE_KEY)"]).unwrap();
        let mut child = Actor::new(8, "child", 16);

        assert!(matches!(
            parent.delegate_authority_to(&mut child, &requested),
            Err(RuntimeAuthorityError::InvalidManifest(_))
        ));
        assert!(child.capabilities.is_empty());
    }
}
