//! Control-plane reconciliation for ambiguous capacity acquisition.
//!
//! This module keeps the provider-neutral reconciliation state machine bound to
//! the exact pre-admitted runtime target and immutable admission evidence. A
//! caller either receives an execution-ready placement with its audit record or
//! `Pending`; raw provider state is never promoted directly to execution.

use std::fmt;

use nulang::admission_policy::AdmissionDecision;
use nulang::deployment_audit::DeploymentAdmissionRecord;
use nulang_capacity::execution::{ExecutionRequirements, ExecutionTarget};
use nulang_capacity::lease::{
    CapacityLease, CapacityLeaser, LeaseRequest, PlacementClaimStore, PlacementLeaseError,
};
use nulang_capacity::reconcile::{
    reconcile_and_resume_ranked_placement, CapacityLeaseReconciler,
    PlacementReconciliationError, PlacementReconciliationResult,
};

use crate::acquisition::PreparedExecutionPlan;

/// Successful reconciliation mapped back to the exact runtime target and
/// evidence that authorized its execution.
#[derive(Debug)]
pub struct ReconciledExecutionPlacement<'a> {
    target: &'a ExecutionTarget,
    lease: CapacityLease,
    requirements: ExecutionRequirements,
    decision: AdmissionDecision,
    audit_record: DeploymentAdmissionRecord,
}

impl<'a> ReconciledExecutionPlacement<'a> {
    pub fn target(&self) -> &'a ExecutionTarget {
        self.target
    }

    pub fn lease(&self) -> &CapacityLease {
        &self.lease
    }

    pub fn requirements(&self) -> &ExecutionRequirements {
        &self.requirements
    }

    pub fn decision(&self) -> &AdmissionDecision {
        &self.decision
    }

    pub fn audit_record(&self) -> &DeploymentAdmissionRecord {
        &self.audit_record
    }
}

#[derive(Debug)]
pub enum ExecutionReconciliationResult<'a> {
    /// A recovered original lease or a post-absence fallback lease is safe to
    /// execute and has been rebound to its pre-admitted target/evidence.
    Placement(ReconciledExecutionPlacement<'a>),
    /// Provider state is still ambiguous. Keep the durable claim and do not
    /// execute or contact another provider.
    Pending,
}

#[derive(Debug)]
pub enum ExecutionReconciliationError {
    NotReconciliable,
    Capacity(PlacementReconciliationError),
    LeaseTargetNotFound { provider: String, offer_id: String },
}

impl fmt::Display for ExecutionReconciliationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReconciliable => write!(
                f,
                "placement error does not represent an ambiguous provider acquisition"
            ),
            Self::Capacity(error) => write!(f, "capacity reconciliation failed: {error:?}"),
            Self::LeaseTargetNotFound { provider, offer_id } => write!(
                f,
                "reconciled lease {provider}/{offer_id} does not map to the prepared execution plan"
            ),
        }
    }
}

impl std::error::Error for ExecutionReconciliationError {}

/// Return the exact acquisition request that must be reconciled for a blocked
/// placement error. Determinate failures and exhausted/claim errors return None.
pub fn reconciliation_request(error: &PlacementLeaseError) -> Option<&LeaseRequest> {
    match error {
        PlacementLeaseError::Indeterminate(indeterminate) => Some(&indeterminate.request),
        PlacementLeaseError::InvalidResponse { request, .. } => Some(request),
        _ => None,
    }
}

/// Reconcile a blocked acquisition and map any safe recovered/resumed lease
/// back to the exact runtime target, admission decision, and audit evidence.
///
/// `blocked_error` must be the `PlacementLeaseError` returned by the original
/// acquisition attempt. Pending reconciliation is a normal non-executable
/// result, not an error.
pub async fn reconcile_prepared_execution_placement<'a>(
    plan: &PreparedExecutionPlan<'a>,
    blocked_error: &PlacementLeaseError,
    claim_store: &dyn PlacementClaimStore,
    reconcilers: &[&dyn CapacityLeaseReconciler],
    leasers: &[&dyn CapacityLeaser],
    claim_expires_at_unix_ms: u64,
) -> Result<ExecutionReconciliationResult<'a>, ExecutionReconciliationError> {
    let request = reconciliation_request(blocked_error)
        .ok_or(ExecutionReconciliationError::NotReconciliable)?;
    let ranked_candidates = plan
        .candidates()
        .iter()
        .map(|candidate| candidate.target().placement.clone())
        .collect::<Vec<_>>();

    let result = reconcile_and_resume_ranked_placement(
        claim_store,
        reconcilers,
        leasers,
        &ranked_candidates,
        request,
        claim_expires_at_unix_ms,
    )
    .await
    .map_err(ExecutionReconciliationError::Capacity)?;

    match result {
        PlacementReconciliationResult::Pending => Ok(ExecutionReconciliationResult::Pending),
        PlacementReconciliationResult::Recovered(lease)
        | PlacementReconciliationResult::Resumed(lease) => {
            let candidate = plan
                .candidates()
                .iter()
                .find(|candidate| {
                    candidate.target().placement.offer.provider == lease.provider
                        && candidate.target().placement.offer.offer_id == lease.offer_id
                })
                .ok_or_else(|| ExecutionReconciliationError::LeaseTargetNotFound {
                    provider: lease.provider.clone(),
                    offer_id: lease.offer_id.clone(),
                })?;

            Ok(ExecutionReconciliationResult::Placement(
                ReconciledExecutionPlacement {
                    target: candidate.target(),
                    lease,
                    requirements: plan.requirements().clone(),
                    decision: candidate.decision().clone(),
                    audit_record: candidate.audit_record().clone(),
                },
            ))
        }
    }
}
