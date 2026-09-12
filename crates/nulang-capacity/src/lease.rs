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
    pub fn idempotency_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.job_id, self.placement_token, self.provider, self.offer_id
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
pub type ReleaseLeaseFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), LeaseError>> + Send + 'a>>;

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
    HeldByOther,
}

pub type ClaimFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ClaimResult, String>> + Send + 'a>>;
pub type ReleaseClaimFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// Durable compare-and-set boundary owned by the Cloud control plane.
///
/// Provider idempotency prevents duplicate allocation within one provider.
/// This claim prevents two scheduler replicas from concurrently placing the
/// same job on different providers.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementLeaseError {
    ClaimHeld,
    ClaimStore(String),
    /// Capacity acquisition may have succeeded remotely. Keep the durable
    /// claim and reconcile this provider before trying any other provider.
    Indeterminate(LeaseError),
    Exhausted(Vec<LeaseAttempt>),
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
        ClaimResult::Acquired => {}
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
            Ok(lease) => return Ok(lease),
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

    #[test]
    fn idempotency_key_is_offer_scoped() {
        let request = LeaseRequest {
            provider: "aws".into(),
            offer_id: "c7g-spot".into(),
            job_id: "job-1".into(),
            placement_token: "placement-42".into(),
            requested_ttl_seconds: Some(900),
        };

        assert_eq!(
            request.idempotency_key(),
            "job-1:placement-42:aws:c7g-spot"
        );
    }
}
