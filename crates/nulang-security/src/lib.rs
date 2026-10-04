//! Shared security primitives for Nulang runtime and agent components.
//!
//! The crate intentionally contains only dependency-light identity, time, and
//! delegation metadata. Runtime capability semantics remain in the runtime
//! crate, while application policy engines can depend on these primitives
//! without pulling in the compiler or VM.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::error::Error;
use std::fmt;

/// Security-principal classes shared by runtime and agent components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    Human,
    Agent,
    Service,
    Workload,
    Device,
}

/// Stable principal identity within an embedding trust domain.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Principal {
    kind: PrincipalKind,
    id: String,
}

impl Principal {
    pub fn new(kind: PrincipalKind, id: impl Into<String>) -> Result<Self, SecurityPrimitiveError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(SecurityPrimitiveError::EmptyPrincipalId);
        }
        Ok(Self { kind, id })
    }

    pub const fn kind(&self) -> PrincipalKind {
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

/// Unix time in whole seconds.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
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

/// Stable delegation identity supplied by the issuing trust boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DelegationId(String);

impl DelegationId {
    pub fn new(id: impl Into<String>) -> Result<Self, SecurityPrimitiveError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(SecurityPrimitiveError::EmptyDelegationId);
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DelegationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Names the authority that owns a revocation generation sequence.
///
/// Epochs are comparable only inside one domain. A tenant policy authority,
/// workload identity issuer, or another embedding trust boundary should use a
/// stable domain ID and never compare its epochs with another domain's epochs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevocationDomainId(String);

impl RevocationDomainId {
    pub fn new(id: impl Into<String>) -> Result<Self, SecurityPrimitiveError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(SecurityPrimitiveError::EmptyRevocationDomainId);
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RevocationDomainId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Monotonic hosted-policy revocation generation within one domain.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct RevocationEpoch(u64);

impl RevocationEpoch {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl fmt::Display for RevocationEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A revocation generation together with the domain that owns it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RevocationVersion {
    domain: RevocationDomainId,
    epoch: RevocationEpoch,
}

impl RevocationVersion {
    pub fn new(domain: RevocationDomainId, epoch: RevocationEpoch) -> Self {
        Self { domain, epoch }
    }

    pub fn domain(&self) -> &RevocationDomainId {
        &self.domain
    }

    pub const fn epoch(&self) -> RevocationEpoch {
        self.epoch
    }

    /// Compare generations only when both versions belong to the same domain.
    /// A domain mismatch is intentionally not ordered and must fail closed at
    /// the policy/authorization layer rather than accidentally comparing the
    /// raw epoch integers.
    pub fn compare_epoch(&self, other: &Self) -> Option<Ordering> {
        if self.domain != other.domain {
            return None;
        }
        Some(self.epoch.cmp(&other.epoch))
    }
}

/// Immutable provenance attached to one issued delegation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationProvenance {
    id: DelegationId,
    parent_id: Option<DelegationId>,
    issued_at: UnixSeconds,
    revocation: RevocationVersion,
}

impl DelegationProvenance {
    pub fn new(
        id: DelegationId,
        parent_id: Option<DelegationId>,
        issued_at: UnixSeconds,
        revocation: RevocationVersion,
    ) -> Self {
        Self {
            id,
            parent_id,
            issued_at,
            revocation,
        }
    }

    pub fn id(&self) -> &DelegationId {
        &self.id
    }

    pub fn parent_id(&self) -> Option<&DelegationId> {
        self.parent_id.as_ref()
    }

    pub const fn issued_at(&self) -> UnixSeconds {
        self.issued_at
    }

    pub fn revocation(&self) -> &RevocationVersion {
        &self.revocation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityPrimitiveError {
    EmptyPrincipalId,
    EmptyDelegationId,
    EmptyRevocationDomainId,
}

impl fmt::Display for SecurityPrimitiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPrincipalId => f.write_str("principal id must not be empty"),
            Self::EmptyDelegationId => f.write_str("delegation id must not be empty"),
            Self::EmptyRevocationDomainId => f.write_str("revocation domain id must not be empty"),
        }
    }
}

impl Error for SecurityPrimitiveError {}
