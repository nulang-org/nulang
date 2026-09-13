use crate::broker::PlacementCandidate;
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Acquired,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityLease {
    pub lease_id: String,
    pub provider: String,
    pub offer_id: String,
    pub job_id: String,
    pub placement_token: String,
    pub acquired_at_unix_ms: u64,
    pub expires_at_unix_ms: Option<u64>,
    pub state: LeaseState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub provider: String,
    pub offer_id: String,
    pub job_id: String,
    /// Stable token for one logical placement transaction.
    pub placement_token: String,
    pub requested_ttl_seconds: Option<u64>,
}

impl LeaseRequest {
    /// Stable logical idempotency material. Provider adapters may hash this
    /// value when their native idempotency-token field has a shorter limit.
    pub fn idempotency_key(&self) -> String {
        format!(
            "j{}:{}|t{}:{}|p{}:{}|o{}:{}",
            self.job_id.len(),
            self.job_id,
            self.placement_token.len(),
            self.placement_token,
            self.provider.len(),
            self.provider,
            self.offer_id.len(),
            self.offer_id
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseErrorKind {
    CapacityLost,
    Conflict,
    RateLimited,
    Authentication,
    ProviderUnavailable,
    InvalidRequest,
    /// The provider may have allocated capacity but the response was lost.
    /// Falling through to another provider could double-place the job.
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseError {
    pub provider: String,
    pub offer_id: String,
    pub kind: LeaseErrorKind,
    pub message: String,
    pub retryable: bool,
}

pub type AcquireLeaseFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CapacityLease, LeaseError>> + Send + 'a>>;
pub type ReleaseLeaseFuture<'a> = Pin<Box<dyn Future<Output = Result<(), LeaseError>> + Send + 'a>>;

/// Provider-side capacity acquisition boundary.
///
/// Implementations must use `LeaseRequest::idempotency_key()` for retries so
/// a repeated request cannot allocate duplicate capacity at that provider.
pub trait CapacityLeaser: Send + Sync {
    fn provider_id(&self) -> &str;

    fn acquire<'a>(&'a self, request: &'a LeaseRequest) -> AcquireLeaseFuture<'a>;

    fn release<'a>(&'a self, lease: &'a CapacityLease) -> ReleaseLeaseFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementClaim {
    pub job_id: String,
    pub placement_token: String,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimResult {
    Acquired,
    /// An earlier retry already acquired the claim with this same token.
    AlreadyHeldByCaller,
    HeldByOther,
}

pub type ClaimFuture<'a> = Pin<Box<dyn Future<Output = Result<ClaimResult, String>> + Send + 'a>>;
pub type ReleaseClaimFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Durable compare-and-set boundary owned by the Cloud control plane.
///
/// Provider idempotency prevents duplicate allocation within one provider.
/// This claim prevents two scheduler replicas from concurrently placing the
/// same job on different providers. A repeated claim using the same
/// `placement_token` must return `AlreadyHeldByCaller`.
pub trait PlacementClaimStore: Send + Sync {
    fn try_claim<'a>(&'a self, claim: &'a PlacementClaim) -> ClaimFuture<'a>;

    fn release_claim<'a>(&'a self, claim: &'a PlacementClaim) -> ReleaseClaimFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseAttempt {
    pub provider: String,
    pub offer_id: String,
    /// `None` means no leaser was registered for this provider.
    pub error: Option<LeaseError>,
}

/// Structured mismatches between a provider lease response and the exact
/// request that produced it. A mismatched success response is unsafe to treat as
/// either success or a normal fallback failure because the provider may already
/// have allocated capacity under unexpected identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseResponseViolation {
    EmptyLeaseId,
    ProviderMismatch { expected: String, actual: String },
    OfferMismatch { expected: String, actual: String },
    JobMismatch { expected: String, actual: String },
    PlacementTokenMismatch { expected: String, actual: String },
    StateNotAcquired { actual: LeaseState },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementLeaseError {
    ClaimHeld,
    ClaimStore(String),
    /// Capacity acquisition may have succeeded remotely. Keep the durable
    /// claim and reconcile this provider before trying any other provider.
    Indeterminate(LeaseError),
    /// A provider returned `Ok` but the lease identity/state does not match the
    /// exact request. Keep the durable claim and reconcile before fallback;
    /// otherwise the job could be double-placed or bound to the wrong capacity.
    InvalidResponse {
        request: LeaseRequest,
        lease: CapacityLease,
        violations: Vec<LeaseResponseViolation>,
    },
    Exhausted(Vec<LeaseAttempt>),
}

/// Validate that a provider's successful lease response is bound to the exact
/// acquisition request and represents live acquired capacity.
pub fn validate_lease_response(
    request: &LeaseRequest,
    lease: &CapacityLease,
) -> Result<(), Vec<LeaseResponseViolation>> {
    let mut violations = Vec::new();

    if lease.lease_id.is_empty() {
        violations.push(LeaseResponseViolation::EmptyLeaseId);
    }
    if lease.provider != request.provider {
        violations.push(LeaseResponseViolation::ProviderMismatch {
            expected: request.provider.clone(),
            actual: lease.provider.clone(),
        });
    }
    if lease.offer_id != request.offer_id {
        violations.push(LeaseResponseViolation::OfferMismatch {
            expected: request.offer_id.clone(),
            actual: lease.offer_id.clone(),
        });
    }
    if lease.job_id != request.job_id {
        violations.push(LeaseResponseViolation::JobMismatch {
            expected: request.job_id.clone(),
            actual: lease.job_id.clone(),
        });
    }
    if lease.placement_token != request.placement_token {
        violations.push(LeaseResponseViolation::PlacementTokenMismatch {
            expected: request.placement_token.clone(),
            actual: lease.placement_token.clone(),
        });
    }
    if lease.state != LeaseState::Acquired {
        violations.push(LeaseResponseViolation::StateNotAcquired {
            actual: lease.state,
        });
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

/// Acquire one placement lease using ranked candidates as a sequential
/// fallback ladder. Only one scheduler replica may enter the ladder for a job
/// because the durable claim is acquired first.
pub async fn acquire_ranked_placement(
    claim_store: &dyn PlacementClaimStore,
    leasers: &[&dyn CapacityLeaser],
    candidates: &[PlacementCandidate],
    job_id: &str,
    placement_token: &str,
    claim_expires_at_unix_ms: u64,
    requested_ttl_seconds: Option<u64>,
) -> Result<CapacityLease, PlacementLeaseError> {
    let claim = PlacementClaim {
        job_id: job_id.to_owned(),
        placement_token: placement_token.to_owned(),
        expires_at_unix_ms: claim_expires_at_unix_ms,
    };

    match claim_store
        .try_claim(&claim)
        .await
        .map_err(PlacementLeaseError::ClaimStore)?
    {
        ClaimResult::Acquired | ClaimResult::AlreadyHeldByCaller => {}
        ClaimResult::HeldByOther => return Err(PlacementLeaseError::ClaimHeld),
    }

    let mut attempts = Vec::new();
    for candidate in candidates {
        let provider = &candidate.offer.provider;
        let Some(leaser) = leasers
            .iter()
            .copied()
            .find(|leaser| leaser.provider_id() == provider)
        else {
            attempts.push(LeaseAttempt {
                provider: provider.clone(),
                offer_id: candidate.offer.offer_id.clone(),
                error: None,
            });
            continue;
        };

        let request = LeaseRequest {
            provider: provider.clone(),
            offer_id: candidate.offer.offer_id.clone(),
            job_id: job_id.to_owned(),
            placement_token: placement_token.to_owned(),
            requested_ttl_seconds,
        };

        match leaser.acquire(&request).await {
            Ok(lease) => match validate_lease_response(&request, &lease) {
                Ok(()) => return Ok(lease),
                Err(violations) => {
                    return Err(PlacementLeaseError::InvalidResponse {
                        request,
                        lease,
                        violations,
                    })
                }
            },
            Err(error) if error.kind == LeaseErrorKind::Indeterminate => {
                return Err(PlacementLeaseError::Indeterminate(error));
            }
            Err(error) => attempts.push(LeaseAttempt {
                provider: provider.clone(),
                offer_id: candidate.offer.offer_id.clone(),
                error: Some(error),
            }),
        }
    }

    claim_store
        .release_claim(&claim)
        .await
        .map_err(PlacementLeaseError::ClaimStore)?;
    Err(PlacementLeaseError::Exhausted(attempts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LeaseRequest {
        LeaseRequest {
            provider: "aws".into(),
            offer_id: "c7g-spot".into(),
            job_id: "job-1".into(),
            placement_token: "placement-42".into(),
            requested_ttl_seconds: Some(900),
        }
    }

    fn lease() -> CapacityLease {
        CapacityLease {
            lease_id: "lease-1".into(),
            provider: "aws".into(),
            offer_id: "c7g-spot".into(),
            job_id: "job-1".into(),
            placement_token: "placement-42".into(),
            acquired_at_unix_ms: 100,
            expires_at_unix_ms: Some(1_000),
            state: LeaseState::Acquired,
        }
    }

    #[test]
    fn idempotency_key_is_unambiguous_and_offer_scoped() {
        assert_eq!(
            request().idempotency_key(),
            "j5:job-1|t12:placement-42|p3:aws|o8:c7g-spot"
        );
    }

    #[test]
    fn matching_lease_response_is_accepted() {
        assert_eq!(validate_lease_response(&request(), &lease()), Ok(()));
    }

    #[test]
    fn lease_response_validation_reports_all_identity_and_state_mismatches() {
        let mut actual = lease();
        actual.lease_id.clear();
        actual.provider = "gcp".into();
        actual.offer_id = "n2-spot".into();
        actual.job_id = "other-job".into();
        actual.placement_token = "other-placement".into();
        actual.state = LeaseState::Released;

        assert_eq!(
            validate_lease_response(&request(), &actual),
            Err(vec![
                LeaseResponseViolation::EmptyLeaseId,
                LeaseResponseViolation::ProviderMismatch {
                    expected: "aws".into(),
                    actual: "gcp".into(),
                },
                LeaseResponseViolation::OfferMismatch {
                    expected: "c7g-spot".into(),
                    actual: "n2-spot".into(),
                },
                LeaseResponseViolation::JobMismatch {
                    expected: "job-1".into(),
                    actual: "other-job".into(),
                },
                LeaseResponseViolation::PlacementTokenMismatch {
                    expected: "placement-42".into(),
                    actual: "other-placement".into(),
                },
                LeaseResponseViolation::StateNotAcquired {
                    actual: LeaseState::Released,
                },
            ])
        );
    }
}
