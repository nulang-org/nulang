//! Principal-aware authorization and constrained authority delegation.
//!
//! This module builds on [`crate::authority::AuthorityManifest`] rather than
//! introducing a second permission model. `AuthorityManifest` remains Nulang's
//! exact, deny-by-default external-authority vocabulary; this layer adds the
//! identity and delegation semantics needed for humans, agents, services,
//! workloads, and devices.
//!
//! The kernel is intentionally pure and deterministic. It does not mint
//! tokens, perform cryptography, consult storage, or make network calls. Those
//! protocol and persistence concerns can wrap this layer later (for example an
//! AuthZEN PDP or a signed delegation envelope).

use crate::authority::{AuthorityGrant, AuthorityManifest};
use std::error::Error;
use std::fmt;

/// Unix time in whole seconds.
///
/// A security boundary should never accept an unqualified integer timestamp:
/// seconds-vs-milliseconds mistakes can accidentally extend or prematurely
/// expire delegated authority. This newtype makes the unit explicit while
/// keeping authorization evaluation allocation-free and deterministic.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixSeconds(u64);

impl UnixSeconds {
    pub const fn from_secs(seconds: u64) -> Self {
        Self(seconds)
    }

    pub const fn as_secs(self) -> u64 {
        self.0
    }
}

impl fmt::Display for UnixSeconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Security-principal classes understood by the authorization kernel.
///
/// Authorization semantics are uniform across kinds: a human, agent, service,
/// workload, or device receives only authority delegated to that exact
/// principal. `Workload` is explicit rather than being folded into `Service`
/// so hosted runtimes can distinguish deployable execution identities from
/// long-lived service/application identities without string conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PrincipalKind {
    Human,
    Agent,
    Service,
    Workload,
    Device,
}

/// Stable principal identity within the caller's trust domain.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Principal {
    kind: PrincipalKind,
    id: String,
}

impl Principal {
    pub fn new(kind: PrincipalKind, id: impl Into<String>) -> Result<Self, DelegationError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(DelegationError::InvalidPrincipalId(id));
        }
        Ok(Self { kind, id })
    }

    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id)
    }
}

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

        if let Some(missing) = requested
            .iter()
            .find(|grant| !self.authority.allows(grant))
        {
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

/// Failure to construct an identity or derive a child delegation safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationError {
    InvalidPrincipalId(String),
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
            Self::InvalidPrincipalId(_) => write!(f, "principal id must not be empty"),
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
    fn whitespace_only_principal_is_rejected() {
        assert!(matches!(
            Principal::new(PrincipalKind::Agent, "   "),
            Err(DelegationError::InvalidPrincipalId(_))
        ));
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
