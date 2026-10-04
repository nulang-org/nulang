//! Principal-aware authorization and constrained authority delegation.
//!
//! This module builds on [`crate::authority::AuthorityManifest`] rather than
//! introducing a second permission model. `AuthorityManifest` remains Nulang's
//! exact, deny-by-default external-authority vocabulary; this layer adds the
//! delegation semantics needed for shared security principals.
//!
//! Identity, explicit Unix-second time, and delegation provenance primitives
//! live in the dependency-light `nulang-security` crate so agent tooling can
//! share them without depending on the compiler or VM.

use crate::authority::{AuthorityGrant, AuthorityManifest};
pub use nulang_security::{
    DelegationId, DelegationProvenance, Principal, PrincipalKind, RevocationEpoch, UnixSeconds,
};
use std::error::Error;
use std::fmt;

/// Constraints that may narrow a delegation.
///
/// The validity interval is half-open: `not_before <= now < expires_at`.
/// `can_redelegate` controls whether the subject may derive a child
/// delegation. Child delegations may only narrow these constraints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DelegationConstraints {
    not_before: Option<UnixSeconds>,
    expires_at: Option<UnixSeconds>,
    can_redelegate: bool,
}

impl DelegationConstraints {
    pub fn new(
        not_before: Option<UnixSeconds>,
        expires_at: Option<UnixSeconds>,
        can_redelegate: bool,
    ) -> Result<Self, DelegationError> {
        if let (Some(not_before), Some(expires_at)) = (not_before, expires_at) {
            if not_before >= expires_at {
                return Err(DelegationError::InvalidTimeWindow {
                    not_before,
                    expires_at,
                });
            }
        }
        Ok(Self {
            not_before,
            expires_at,
            can_redelegate,
        })
    }

    pub fn not_before(&self) -> Option<UnixSeconds> {
        self.not_before
    }

    pub fn expires_at(&self) -> Option<UnixSeconds> {
        self.expires_at
    }

    pub fn can_redelegate(&self) -> bool {
        self.can_redelegate
    }

    fn inactive_reason(&self, now: UnixSeconds) -> Option<DecisionReason> {
        if let Some(not_before) = self.not_before {
            if now < not_before {
                return Some(DecisionReason::NotYetValid { not_before, now });
            }
        }
        if let Some(expires_at) = self.expires_at {
            if now >= expires_at {
                return Some(DecisionReason::Expired { expires_at, now });
            }
        }
        None
    }

    fn ensure_attenuates(&self, parent: &Self) -> Result<(), DelegationError> {
        if let Some(parent_not_before) = parent.not_before {
            match self.not_before {
                Some(child_not_before) if child_not_before >= parent_not_before => {}
                _ => {
                    return Err(DelegationError::ConstraintExpansion(
                        ConstraintField::NotBefore,
                    ))
                }
            }
        }

        if let Some(parent_expires_at) = parent.expires_at {
            match self.expires_at {
                Some(child_expires_at) if child_expires_at <= parent_expires_at => {}
                _ => {
                    return Err(DelegationError::ConstraintExpansion(
                        ConstraintField::ExpiresAt,
                    ))
                }
            }
        }

        Ok(())
    }
}

/// Stable field identifiers for explaining why delegation attenuation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintField {
    NotBefore,
    ExpiresAt,
}

/// One explicit authority delegation from an issuer to a subject.
///
/// The issuer is provenance identity. Trust in an initial issuer is established
/// by the embedding identity system. Chained delegations are safe by
/// construction because [`AuthorityDelegation::delegate`] makes the current
/// subject the child issuer and enforces monotonic attenuation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityDelegation {
    issuer: Principal,
    subject: Principal,
    authority: AuthorityManifest,
    constraints: DelegationConstraints,
}

impl AuthorityDelegation {
    /// Establish an authority delegation at an authenticated trust boundary.
    ///
    /// This constructor deliberately does not claim to authenticate `issuer`;
    /// the embedding system must establish that trust before calling it.
    pub fn issue(
        issuer: Principal,
        subject: Principal,
        authority: AuthorityManifest,
        constraints: DelegationConstraints,
    ) -> Self {
        Self {
            issuer,
            subject,
            authority,
            constraints,
        }
    }

    pub fn issuer(&self) -> &Principal {
        &self.issuer
    }

    pub fn subject(&self) -> &Principal {
        &self.subject
    }

    pub fn authority(&self) -> &AuthorityManifest {
        &self.authority
    }

    pub fn constraints(&self) -> DelegationConstraints {
        self.constraints
    }

    /// Evaluate one exact authority request for one principal at one instant.
    ///
    /// Decision ordering is deterministic: principal identity, temporal
    /// validity, then exact authority. Audit/explain surfaces therefore receive
    /// one stable primary reason rather than policy-order-dependent output.
    pub fn authorize(
        &self,
        principal: &Principal,
        grant: &AuthorityGrant,
        now: UnixSeconds,
    ) -> AuthorizationDecision {
        if principal != &self.subject {
            return AuthorizationDecision::deny(
                principal.clone(),
                grant.clone(),
                DecisionReason::PrincipalMismatch {
                    expected: self.subject.clone(),
                    actual: principal.clone(),
                },
            );
        }

        if let Some(reason) = self.constraints.inactive_reason(now) {
            return AuthorizationDecision::deny(principal.clone(), grant.clone(), reason);
        }

        if !self.authority.allows(grant) {
            return AuthorizationDecision::deny(
                principal.clone(),
                grant.clone(),
                DecisionReason::MissingGrant(grant.clone()),
            );
        }

        AuthorizationDecision::allow(principal.clone(), grant.clone())
    }

    /// Create an attenuated child delegation from the current subject.
    ///
    /// This cannot manufacture authority or widen the parent's validity
    /// interval. It also refuses to delegate from inactive or explicitly
    /// non-redelegable authority.
    pub fn delegate(
        &self,
        now: UnixSeconds,
        subject: Principal,
        requested: AuthorityManifest,
        constraints: DelegationConstraints,
    ) -> Result<Self, DelegationError> {
        if let Some(reason) = self.constraints.inactive_reason(now) {
            return Err(DelegationError::InactiveParent(reason));
        }
        if !self.constraints.can_redelegate {
            return Err(DelegationError::RedelegationForbidden);
        }

        if let Some(missing) = requested.iter().find(|grant| !self.authority.allows(grant)) {
            return Err(DelegationError::AuthorityEscalation(missing.clone()));
        }

        constraints.ensure_attenuates(&self.constraints)?;

        Ok(Self {
            issuer: self.subject.clone(),
            subject,
            authority: requested,
            constraints,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionEffect {
    Allow,
    Deny,
}

/// Deterministic, explainable result of one authorization evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationDecision {
    effect: DecisionEffect,
    principal: Principal,
    grant: AuthorityGrant,
    reason: Option<DecisionReason>,
}

impl AuthorizationDecision {
    fn allow(principal: Principal, grant: AuthorityGrant) -> Self {
        Self {
            effect: DecisionEffect::Allow,
            principal,
            grant,
            reason: None,
        }
    }

    fn deny(principal: Principal, grant: AuthorityGrant, reason: DecisionReason) -> Self {
        Self {
            effect: DecisionEffect::Deny,
            principal,
            grant,
            reason: Some(reason),
        }
    }

    pub fn effect(&self) -> DecisionEffect {
        self.effect
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self.effect, DecisionEffect::Allow)
    }

    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    pub fn grant(&self) -> &AuthorityGrant {
        &self.grant
    }

    pub fn reason(&self) -> Option<&DecisionReason> {
        self.reason.as_ref()
    }
}

/// Primary reason for an authorization denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionReason {
    PrincipalMismatch {
        expected: Principal,
        actual: Principal,
    },
    NotYetValid {
        not_before: UnixSeconds,
        now: UnixSeconds,
    },
    Expired {
        expires_at: UnixSeconds,
        now: UnixSeconds,
    },
    MissingGrant(AuthorityGrant),
}

/// Failure to derive a child delegation safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationError {
    InvalidTimeWindow {
        not_before: UnixSeconds,
        expires_at: UnixSeconds,
    },
    InactiveParent(DecisionReason),
    RedelegationForbidden,
    AuthorityEscalation(AuthorityGrant),
    ConstraintExpansion(ConstraintField),
}

impl fmt::Display for DelegationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimeWindow {
                not_before,
                expires_at,
            } => write!(
                f,
                "invalid delegation window: not_before ({not_before}) must be before expires_at ({expires_at})"
            ),
            Self::InactiveParent(reason) => {
                write!(f, "cannot delegate inactive authority: {reason:?}")
            }
            Self::RedelegationForbidden => write!(f, "authority does not permit redelegation"),
            Self::AuthorityEscalation(grant) => {
                write!(f, "delegation would manufacture authority: {grant}")
            }
            Self::ConstraintExpansion(ConstraintField::NotBefore) => {
                write!(f, "child delegation widens the parent's not-before bound")
            }
            Self::ConstraintExpansion(ConstraintField::ExpiresAt) => {
                write!(f, "child delegation widens the parent's expiry bound")
            }
        }
    }
}

impl Error for DelegationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(seconds: u64) -> UnixSeconds {
        UnixSeconds::from_secs(seconds)
    }

    #[test]
    fn whitespace_only_principal_is_rejected_by_shared_identity_layer() {
        assert!(Principal::new(PrincipalKind::Agent, "   ").is_err());
    }

    #[test]
    fn future_delegation_is_not_yet_valid() {
        let human = Principal::new(PrincipalKind::Human, "human:root").unwrap();
        let agent = Principal::new(PrincipalKind::Agent, "agent:worker").unwrap();
        let delegation = AuthorityDelegation::issue(
            human,
            agent.clone(),
            AuthorityManifest::from_tokens(["Env::Read(API_URL)"]).unwrap(),
            DelegationConstraints::new(Some(t(100)), Some(t(200)), false).unwrap(),
        );
        let grant: AuthorityGrant = "Env::Read(API_URL)".parse().unwrap();

        assert_eq!(
            delegation.authorize(&agent, &grant, t(99)).reason(),
            Some(&DecisionReason::NotYetValid {
                not_before: t(100),
                now: t(99),
            })
        );
    }

    #[test]
    fn child_cannot_remove_parent_expiry_bound() {
        let human = Principal::new(PrincipalKind::Human, "human:root").unwrap();
        let agent = Principal::new(PrincipalKind::Agent, "agent:parent").unwrap();
        let child = Principal::new(PrincipalKind::Agent, "agent:child").unwrap();
        let authority = AuthorityManifest::from_tokens(["Env::Read(API_URL)"]).unwrap();
        let parent = AuthorityDelegation::issue(
            human,
            agent,
            authority.clone(),
            DelegationConstraints::new(None, Some(t(200)), true).unwrap(),
        );

        assert_eq!(
            parent.delegate(t(100), child, authority, DelegationConstraints::default()),
            Err(DelegationError::ConstraintExpansion(
                ConstraintField::ExpiresAt
            ))
        );
    }
}
