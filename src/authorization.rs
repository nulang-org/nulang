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
    DelegationId, DelegationProvenance, Principal, PrincipalKind, RevocationDomainId,
    RevocationEpoch, RevocationVersion, UnixSeconds,
};
use std::cmp::Ordering;
use std::error::Error;
use std::fmt;

/// Context supplied by the embedding policy boundary for one authorization
/// evaluation.
///
/// Revocation is explicit rather than ambient: callers must present the current
/// version for the same revocation domain that issued the delegation. This
/// prevents authorization from silently succeeding with stale hosted-policy
/// state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationContext {
    now: UnixSeconds,
    revocation: RevocationVersion,
}

impl AuthorizationContext {
    pub fn new(now: UnixSeconds, revocation: RevocationVersion) -> Self {
        Self { now, revocation }
    }

    pub const fn now(&self) -> UnixSeconds {
        self.now
    }

    pub fn revocation(&self) -> &RevocationVersion {
        &self.revocation
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
/// Trust in a root issuer is established by the embedding identity system.
/// Child delegations are derived only through [`AuthorityDelegation::delegate`]
/// so authority, time constraints, provenance, and revocation state all
/// attenuate monotonically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityDelegation {
    provenance: DelegationProvenance,
    issuer: Principal,
    subject: Principal,
    authority: AuthorityManifest,
    constraints: DelegationConstraints,
}

impl AuthorityDelegation {
    /// Establish a root delegation at an authenticated trust boundary.
    ///
    /// Root issuance requires provenance with no parent. Delegation IDs and
    /// revocation versions are supplied by the embedding trust domain rather
    /// than generated inside this pure authorization kernel.
    pub fn issue(
        provenance: DelegationProvenance,
        issuer: Principal,
        subject: Principal,
        authority: AuthorityManifest,
        constraints: DelegationConstraints,
    ) -> Result<Self, DelegationError> {
        if let Some(parent_id) = provenance.parent_id() {
            return Err(DelegationError::RootHasParent(parent_id.clone()));
        }

        Ok(Self {
            provenance,
            issuer,
            subject,
            authority,
            constraints,
        })
    }

    pub fn provenance(&self) -> &DelegationProvenance {
        &self.provenance
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

    fn revocation_reason(&self, current: &RevocationVersion) -> Option<DecisionReason> {
        match self.provenance.revocation().compare_epoch(current) {
            None => Some(DecisionReason::RevocationDomainMismatch {
                delegation_domain: self.provenance.revocation().domain().clone(),
                current_domain: current.domain().clone(),
            }),
            Some(Ordering::Less) => Some(DecisionReason::Revoked {
                delegation_epoch: self.provenance.revocation().epoch(),
                current_epoch: current.epoch(),
            }),
            Some(Ordering::Equal) => None,
            Some(Ordering::Greater) => Some(DecisionReason::PolicyVersionBehind {
                delegation_epoch: self.provenance.revocation().epoch(),
                current_epoch: current.epoch(),
            }),
        }
    }

    /// Evaluate one exact authority request for one principal and policy view.
    ///
    /// Decision ordering is deterministic: principal identity, revocation
    /// freshness, provenance issue time, explicit validity constraints, then
    /// exact authority. Audit/explain surfaces therefore receive one stable
    /// primary reason rather than policy-order-dependent output.
    pub fn authorize(
        &self,
        principal: &Principal,
        grant: &AuthorityGrant,
        context: &AuthorizationContext,
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

        if let Some(reason) = self.revocation_reason(context.revocation()) {
            return AuthorizationDecision::deny(principal.clone(), grant.clone(), reason);
        }

        if context.now() < self.provenance.issued_at() {
            return AuthorizationDecision::deny(
                principal.clone(),
                grant.clone(),
                DecisionReason::NotYetIssued {
                    issued_at: self.provenance.issued_at(),
                    now: context.now(),
                },
            );
        }

        if let Some(reason) = self.constraints.inactive_reason(context.now()) {
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
    /// This cannot manufacture authority, widen the parent's validity interval,
    /// delegate from a revoked/stale policy view, or reuse the direct parent's
    /// delegation identity. The child provenance points to the parent and is
    /// stamped with the current policy version and issue time.
    pub fn delegate(
        &self,
        context: &AuthorizationContext,
        id: DelegationId,
        subject: Principal,
        requested: AuthorityManifest,
        constraints: DelegationConstraints,
    ) -> Result<Self, DelegationError> {
        if let Some(reason) = self.revocation_reason(context.revocation()) {
            return Err(DelegationError::InactiveParent(reason));
        }
        if context.now() < self.provenance.issued_at() {
            return Err(DelegationError::InactiveParent(
                DecisionReason::NotYetIssued {
                    issued_at: self.provenance.issued_at(),
                    now: context.now(),
                },
            ));
        }
        if let Some(reason) = self.constraints.inactive_reason(context.now()) {
            return Err(DelegationError::InactiveParent(reason));
        }
        if !self.constraints.can_redelegate {
            return Err(DelegationError::RedelegationForbidden);
        }
        if &id == self.provenance.id() {
            return Err(DelegationError::DelegationIdReuse(id));
        }

        if let Some(missing) = requested.iter().find(|grant| !self.authority.allows(grant)) {
            return Err(DelegationError::AuthorityEscalation(missing.clone()));
        }

        constraints.ensure_attenuates(&self.constraints)?;

        Ok(Self {
            provenance: DelegationProvenance::new(
                id,
                Some(self.provenance.id().clone()),
                context.now(),
                context.revocation().clone(),
            ),
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
    RevocationDomainMismatch {
        delegation_domain: RevocationDomainId,
        current_domain: RevocationDomainId,
    },
    Revoked {
        delegation_epoch: RevocationEpoch,
        current_epoch: RevocationEpoch,
    },
    PolicyVersionBehind {
        delegation_epoch: RevocationEpoch,
        current_epoch: RevocationEpoch,
    },
    NotYetIssued {
        issued_at: UnixSeconds,
        now: UnixSeconds,
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

/// Failure to derive or establish a delegation safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationError {
    InvalidTimeWindow {
        not_before: UnixSeconds,
        expires_at: UnixSeconds,
    },
    RootHasParent(DelegationId),
    DelegationIdReuse(DelegationId),
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
            Self::RootHasParent(parent_id) => {
                write!(f, "root delegation must not have parent {parent_id}")
            }
            Self::DelegationIdReuse(id) => {
                write!(f, "child delegation must not reuse parent id {id}")
            }
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

    fn revocation(epoch: u64) -> RevocationVersion {
        RevocationVersion::new(
            RevocationDomainId::new("test:authorization").unwrap(),
            RevocationEpoch::new(epoch),
        )
    }

    fn provenance(id: &str, issued_at: u64) -> DelegationProvenance {
        DelegationProvenance::new(
            DelegationId::new(id).unwrap(),
            None,
            t(issued_at),
            revocation(1),
        )
    }

    fn context(now: u64) -> AuthorizationContext {
        AuthorizationContext::new(t(now), revocation(1))
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
            provenance("delegation:root", 50),
            human,
            agent.clone(),
            AuthorityManifest::from_tokens(["Env::Read(API_URL)"]).unwrap(),
            DelegationConstraints::new(Some(t(100)), Some(t(200)), false).unwrap(),
        )
        .unwrap();
        let grant: AuthorityGrant = "Env::Read(API_URL)".parse().unwrap();

        assert_eq!(
            delegation
                .authorize(&agent, &grant, &context(99))
                .reason(),
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
            provenance("delegation:root", 50),
            human,
            agent,
            authority.clone(),
            DelegationConstraints::new(None, Some(t(200)), true).unwrap(),
        )
        .unwrap();

        assert_eq!(
            parent.delegate(
                &context(100),
                DelegationId::new("delegation:child").unwrap(),
                child,
                authority,
                DelegationConstraints::default(),
            ),
            Err(DelegationError::ConstraintExpansion(
                ConstraintField::ExpiresAt
            ))
        );
    }
}
