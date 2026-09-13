//! Reconciliation for ambiguous provider capacity acquisition.
//!
//! An indeterminate acquisition or invalid successful response must not fall
//! through to another provider: capacity may already exist remotely. This
//! module queries the original provider with the exact `LeaseRequest` identity
//! and resumes fallback only after that provider explicitly confirms absence.

use std::{future::Future, pin::Pin};

use serde::{Deserialize, Serialize};

use crate::broker::PlacementCandidate;
use crate::lease::{
    acquire_ranked_placement, validate_lease_response, CapacityLease, CapacityLeaser,
    LeaseRequest, LeaseResponseViolation, PlacementClaimStore, PlacementLeaseError,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseReconciliationStatus {
    /// The provider found capacity for the exact placement identity.
    Acquired(CapacityLease),
    /// The provider authoritatively confirms no capacity exists for this exact
    /// placement identity. Only this state permits fallback to another offer.
    ConfirmedAbsent,
    /// Provider state is still ambiguous. The durable placement claim must
    /// remain held and no fallback may occur.
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseReconciliationError {
    pub provider: String,
    pub offer_id: String,
    pub message: String,
    pub retryable: bool,
}

pub type ReconcileLeaseFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<LeaseReconciliationStatus, LeaseReconciliationError>>
            + Send
            + 'a,
    >,
>;

/// Provider-side lookup for one exact logical placement transaction.
///
/// Implementations must query by the strongest provider-native idempotency or
/// request identity available. `ConfirmedAbsent` is a safety-critical claim:
/// adapters must return it only when the provider can authoritatively establish
/// that the original acquisition did not allocate capacity.
pub trait CapacityLeaseReconciler: Send + Sync {
    fn provider_id(&self) -> &str;

    fn reconcile<'a>(&'a self, request: &'a LeaseRequest) -> ReconcileLeaseFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementReconciliationResult {
    /// Original provider confirms the original request acquired capacity.
    Recovered(CapacityLease),
    /// Original provider confirmed absence and the ranked fallback ladder
    /// subsequently acquired capacity elsewhere.
    Resumed(CapacityLease),
    /// Provider state remains ambiguous. Keep the durable claim and retry
    /// reconciliation later; never start another provider.
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementReconciliationError {
    ReconcilerUnavailable { provider: String },
    Provider(LeaseReconciliationError),
    InvalidRecoveredLease {
        request: LeaseRequest,
        lease: CapacityLease,
        violations: Vec<LeaseResponseViolation>,
    },
    OriginalCandidateNotFound { provider: String, offer_id: String },
    Resume(PlacementLeaseError),
}

/// Reconcile an ambiguous acquisition and, only after authoritative absence,
/// resume ranked placement after the original candidate.
///
/// The same job id and placement token from `request` are reused. The existing
/// durable claim is intentionally not released before fallback; the placement
/// core will re-enter the claim store with the same token, which a correct
/// store reports as `AlreadyHeldByCaller`.
pub async fn reconcile_and_resume_ranked_placement(
    claim_store: &dyn PlacementClaimStore,
    reconcilers: &[&dyn CapacityLeaseReconciler],
    leasers: &[&dyn CapacityLeaser],
    ranked_candidates: &[PlacementCandidate],
    request: &LeaseRequest,
    claim_expires_at_unix_ms: u64,
) -> Result<PlacementReconciliationResult, PlacementReconciliationError> {
    let reconciler = reconcilers
        .iter()
        .copied()
        .find(|reconciler| reconciler.provider_id() == request.provider)
        .ok_or_else(|| PlacementReconciliationError::ReconcilerUnavailable {
            provider: request.provider.clone(),
        })?;

    let status = reconciler
        .reconcile(request)
        .await
        .map_err(PlacementReconciliationError::Provider)?;

    match status {
        LeaseReconciliationStatus::Acquired(lease) => {
            validate_lease_response(request, &lease).map_err(|violations| {
                PlacementReconciliationError::InvalidRecoveredLease {
                    request: request.clone(),
                    lease: lease.clone(),
                    violations,
                }
            })?;
            Ok(PlacementReconciliationResult::Recovered(lease))
        }
        LeaseReconciliationStatus::Pending => Ok(PlacementReconciliationResult::Pending),
        LeaseReconciliationStatus::ConfirmedAbsent => {
            let original_index = ranked_candidates
                .iter()
                .position(|candidate| {
                    candidate.offer.provider == request.provider
                        && candidate.offer.offer_id == request.offer_id
                })
                .ok_or_else(|| PlacementReconciliationError::OriginalCandidateNotFound {
                    provider: request.provider.clone(),
                    offer_id: request.offer_id.clone(),
                })?;

            let remaining = &ranked_candidates[original_index + 1..];
            let lease = acquire_ranked_placement(
                claim_store,
                leasers,
                remaining,
                &request.job_id,
                &request.placement_token,
                claim_expires_at_unix_ms,
                request.requested_ttl_seconds,
            )
            .await
            .map_err(PlacementReconciliationError::Resume)?;
            Ok(PlacementReconciliationResult::Resumed(lease))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use futures::executor::block_on;

    use super::*;
    use crate::broker::PlacementCandidate;
    use crate::lease::{
        AcquireLeaseFuture, CapacityLeaser, ClaimFuture, ClaimResult, LeaseError, LeaseRequest,
        LeaseState, PlacementClaim, ReleaseClaimFuture, ReleaseLeaseFuture,
    };
    use crate::{Architecture, CapacityOffer, Lifecycle, TrustTier};

    struct MemoryClaimStore;

    impl PlacementClaimStore for MemoryClaimStore {
        fn try_claim<'a>(&'a self, _claim: &'a PlacementClaim) -> ClaimFuture<'a> {
            Box::pin(async { Ok(ClaimResult::AlreadyHeldByCaller) })
        }

        fn release_claim<'a>(&'a self, _claim: &'a PlacementClaim) -> ReleaseClaimFuture<'a> {
            Box::pin(async { Ok(()) })
        }
    }

    struct FakeReconciler {
        provider: String,
        status: Mutex<Option<Result<LeaseReconciliationStatus, LeaseReconciliationError>>>,
    }

    impl FakeReconciler {
        fn new(provider: &str, status: LeaseReconciliationStatus) -> Self {
            Self {
                provider: provider.into(),
                status: Mutex::new(Some(Ok(status))),
            }
        }
    }

    impl CapacityLeaseReconciler for FakeReconciler {
        fn provider_id(&self) -> &str {
            &self.provider
        }

        fn reconcile<'a>(&'a self, _request: &'a LeaseRequest) -> ReconcileLeaseFuture<'a> {
            Box::pin(async move {
                self.status
                    .lock()
                    .expect("reconciliation status lock")
                    .take()
                    .expect("configured reconciliation result")
            })
        }
    }

    struct FakeLeaser {
        provider: String,
        results: Mutex<VecDeque<Result<CapacityLease, LeaseError>>>,
        calls: AtomicUsize,
    }

    impl FakeLeaser {
        fn new(provider: &str, results: Vec<Result<CapacityLease, LeaseError>>) -> Self {
            Self {
                provider: provider.into(),
                results: Mutex::new(results.into()),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl CapacityLeaser for FakeLeaser {
        fn provider_id(&self) -> &str {
            &self.provider
        }

        fn acquire<'a>(&'a self, _request: &'a LeaseRequest) -> AcquireLeaseFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.results
                    .lock()
                    .expect("lease result lock")
                    .pop_front()
                    .expect("configured lease result")
            })
        }

        fn release<'a>(&'a self, _lease: &'a CapacityLease) -> ReleaseLeaseFuture<'a> {
            Box::pin(async { Ok(()) })
        }
    }

    fn request() -> LeaseRequest {
        LeaseRequest {
            provider: "aws".into(),
            offer_id: "aws-offer".into(),
            job_id: "job-1".into(),
            placement_token: "placement-1".into(),
            requested_ttl_seconds: Some(900),
        }
    }

    fn lease(provider: &str, offer_id: &str) -> CapacityLease {
        CapacityLease {
            lease_id: format!("lease-{provider}-{offer_id}"),
            provider: provider.into(),
            offer_id: offer_id.into(),
            job_id: "job-1".into(),
            placement_token: "placement-1".into(),
            acquired_at_unix_ms: 1,
            expires_at_unix_ms: None,
            state: LeaseState::Acquired,
        }
    }

    fn candidate(provider: &str, offer_id: &str, cost: f64) -> PlacementCandidate {
        PlacementCandidate {
            offer: CapacityOffer {
                provider: provider.into(),
                region: "us-east".into(),
                zone: None,
                offer_id: offer_id.into(),
                lifecycle: Lifecycle::OnDemand,
                architecture: Architecture::X86_64,
                vcpus: 4.0,
                memory_gib: 8.0,
                accelerator: None,
                hourly_usd: cost,
                storage_usd: 0.0,
                egress_usd_per_gib: 0.0,
                startup_p50_seconds: 1.0,
                startup_p95_seconds: 2.0,
                interruption_rate_per_hour: 0.0,
                interruption_notice_seconds: 0,
                capacity_confidence: 1.0,
                throughput_score: 1.0,
                trust_tier: TrustTier::CloudProvider,
            },
            effective_cost_usd: cost,
            expected_runtime_seconds: 60.0,
            expected_recovery_seconds: 0.0,
        }
    }

    #[test]
    fn recovered_lease_must_match_exact_request() {
        let recovered = lease("aws", "aws-offer");
        let reconciler = FakeReconciler::new(
            "aws",
            LeaseReconciliationStatus::Acquired(recovered.clone()),
        );
        let result = block_on(reconcile_and_resume_ranked_placement(
            &MemoryClaimStore,
            &[&reconciler],
            &[],
            &[candidate("aws", "aws-offer", 0.1)],
            &request(),
            10_000,
        ))
        .expect("recover exact lease");

        assert_eq!(result, PlacementReconciliationResult::Recovered(recovered));
    }

    #[test]
    fn pending_reconciliation_never_touches_fallback_provider() {
        let reconciler = FakeReconciler::new("aws", LeaseReconciliationStatus::Pending);
        let gcp = FakeLeaser::new("gcp", vec![Ok(lease("gcp", "gcp-offer"))]);
        let candidates = vec![
            candidate("aws", "aws-offer", 0.1),
            candidate("gcp", "gcp-offer", 0.2),
        ];

        let result = block_on(reconcile_and_resume_ranked_placement(
            &MemoryClaimStore,
            &[&reconciler],
            &[&gcp],
            &candidates,
            &request(),
            10_000,
        ))
        .expect("pending is not an error");

        assert_eq!(result, PlacementReconciliationResult::Pending);
        assert_eq!(gcp.calls(), 0);
    }

    #[test]
    fn confirmed_absence_resumes_only_after_original_candidate() {
        let reconciler = FakeReconciler::new("aws", LeaseReconciliationStatus::ConfirmedAbsent);
        let aws = FakeLeaser::new("aws", vec![Ok(lease("aws", "aws-offer"))]);
        let gcp_lease = lease("gcp", "gcp-offer");
        let gcp = FakeLeaser::new("gcp", vec![Ok(gcp_lease.clone())]);
        let candidates = vec![
            candidate("aws", "aws-offer", 0.1),
            candidate("gcp", "gcp-offer", 0.2),
        ];

        let result = block_on(reconcile_and_resume_ranked_placement(
            &MemoryClaimStore,
            &[&reconciler],
            &[&aws, &gcp],
            &candidates,
            &request(),
            10_000,
        ))
        .expect("resume fallback");

        assert_eq!(result, PlacementReconciliationResult::Resumed(gcp_lease));
        assert_eq!(aws.calls(), 0, "the reconciled offer must never be retried");
        assert_eq!(gcp.calls(), 1);
    }

    #[test]
    fn invalid_recovered_lease_remains_blocked() {
        let mut recovered = lease("aws", "aws-offer");
        recovered.job_id = "wrong-job".into();
        let reconciler = FakeReconciler::new(
            "aws",
            LeaseReconciliationStatus::Acquired(recovered),
        );

        let error = block_on(reconcile_and_resume_ranked_placement(
            &MemoryClaimStore,
            &[&reconciler],
            &[],
            &[candidate("aws", "aws-offer", 0.1)],
            &request(),
            10_000,
        ))
        .expect_err("invalid recovered identity must remain blocked");

        assert!(matches!(
            error,
            PlacementReconciliationError::InvalidRecoveredLease { .. }
        ));
    }
}
