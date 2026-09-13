//! Race-safe provider lease acquisition for fully admitted execution targets.
//!
//! Preparation has no provider side effects: it filters runtime-compatible
//! targets, re-runs final artifact admission for each candidate, and constructs
//! immutable audit evidence. Acquisition then delegates to `nulang-capacity`'s
//! durable placement-claim and provider-idempotency machinery.

use std::collections::BTreeSet;
use std::fmt;

use nulang::admission_policy::{AdmissionDecision, AdmissionPolicy};
use nulang::deployment_audit::{DeploymentAdmissionRecord, DeploymentAuditError};
use nulang::deployment_bundle::DeploymentBundle;
use nulang_capacity::broker::PlacementCandidate;
use nulang_capacity::execution::{
    evaluate_execution_target, ExecutionRequirements, ExecutionTarget,
};
use nulang_capacity::lease::{
    acquire_ranked_placement, CapacityLease, CapacityLeaser, PlacementClaimStore,
    PlacementLeaseError,
};

use crate::{admission_environment, derive_execution_requirements};

/// One target that passed runtime eligibility and exact final artifact admission.
#[derive(Debug)]
pub struct PreparedExecutionCandidate<'a> {
    target: &'a ExecutionTarget,
    decision: AdmissionDecision,
    audit_record: DeploymentAdmissionRecord,
}

impl<'a> PreparedExecutionCandidate<'a> {
    pub fn target(&self) -> &'a ExecutionTarget {
        self.target
    }

    pub fn decision(&self) -> &AdmissionDecision {
        &self.decision
    }

    pub fn audit_record(&self) -> &DeploymentAdmissionRecord {
        &self.audit_record
    }
}

/// Ranked, fully admitted candidates prepared before any provider side effect.
#[derive(Debug)]
pub struct PreparedExecutionPlan<'a> {
    requirements: ExecutionRequirements,
    candidates: Vec<PreparedExecutionCandidate<'a>>,
}

impl<'a> PreparedExecutionPlan<'a> {
    pub fn requirements(&self) -> &ExecutionRequirements {
        &self.requirements
    }

    pub fn candidates(&self) -> &[PreparedExecutionCandidate<'a>] {
        &self.candidates
    }

    fn placement_candidates(&self) -> Vec<PlacementCandidate> {
        self.candidates
            .iter()
            .map(|candidate| candidate.target.placement.clone())
            .collect()
    }

    fn candidate_for_lease(&self, lease: &CapacityLease) -> Option<&PreparedExecutionCandidate<'a>> {
        self.candidates.iter().find(|candidate| {
            candidate.target.placement.offer.provider == lease.provider
                && candidate.target.placement.offer.offer_id == lease.offer_id
        })
    }
}

#[derive(Debug)]
pub enum PrepareExecutionError {
    PolicyDenied(AdmissionDecision),
    NoEligibleExecutionTarget,
    /// The provider lease API identifies capacity by provider+offer. Allowing
    /// two runtime targets to share that key would make the returned lease
    /// ambiguous and could attach the wrong audit/environment to execution.
    AmbiguousProviderOffer { provider: String, offer_id: String },
    FinalAdmissionDenied {
        target_id: String,
        decision: AdmissionDecision,
    },
    Audit {
        target_id: String,
        source: DeploymentAuditError,
    },
}

impl fmt::Display for PrepareExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PolicyDenied(decision) => write!(
                f,
                "deployment denied before capacity planning ({} reason(s))",
                decision.reasons.len()
            ),
            Self::NoEligibleExecutionTarget => write!(
                f,
                "no runtime/isolation-compatible execution target is available"
            ),
            Self::AmbiguousProviderOffer { provider, offer_id } => write!(
                f,
                "multiple execution targets map to provider/offer {provider}/{offer_id}"
            ),
            Self::FinalAdmissionDenied {
                target_id,
                decision,
            } => write!(
                f,
                "execution target {target_id} failed final admission ({} reason(s))",
                decision.reasons.len()
            ),
            Self::Audit { target_id, source } => {
                write!(f, "cannot build audit evidence for target {target_id}: {source}")
            }
        }
    }
}

impl std::error::Error for PrepareExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Audit { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Prepare a ranked execution plan without acquiring provider capacity.
///
/// All returned candidates have already passed exact artifact admission against
/// their concrete runtime envelope, and each candidate carries immutable audit
/// evidence. This keeps audit-construction failures ahead of provider side
/// effects.
pub fn prepare_execution_plan<'a>(
    deployment_id: &str,
    bundle: &DeploymentBundle,
    policy: &AdmissionPolicy,
    targets: &'a [ExecutionTarget],
) -> Result<PreparedExecutionPlan<'a>, PrepareExecutionError> {
    let (requirements, _preflight) = derive_execution_requirements(bundle, policy)
        .map_err(PrepareExecutionError::PolicyDenied)?;

    let mut provider_offer_keys = BTreeSet::new();
    let mut candidates = Vec::new();

    for target in targets {
        if !evaluate_execution_target(&requirements, target).eligible {
            continue;
        }

        let provider = target.placement.offer.provider.clone();
        let offer_id = target.placement.offer.offer_id.clone();
        if !provider_offer_keys.insert((provider.clone(), offer_id.clone())) {
            return Err(PrepareExecutionError::AmbiguousProviderOffer { provider, offer_id });
        }

        let environment = admission_environment(&target.runtime);
        let decision = bundle.evaluate_admission(policy, &environment);
        if !decision.admitted {
            return Err(PrepareExecutionError::FinalAdmissionDenied {
                target_id: target.target_id.clone(),
                decision,
            });
        }

        let audit_record = DeploymentAdmissionRecord::new(
            deployment_id,
            bundle,
            policy,
            &environment,
            &decision,
        )
        .map_err(|source| PrepareExecutionError::Audit {
            target_id: target.target_id.clone(),
            source,
        })?;

        candidates.push(PreparedExecutionCandidate {
            target,
            decision,
            audit_record,
        });
    }

    if candidates.is_empty() {
        return Err(PrepareExecutionError::NoEligibleExecutionTarget);
    }

    Ok(PreparedExecutionPlan {
        requirements,
        candidates,
    })
}

/// Provider capacity plus the exact runtime target and admission evidence that
/// authorized its eventual execution.
#[derive(Debug)]
pub struct AcquiredExecutionPlacement<'a> {
    target: &'a ExecutionTarget,
    lease: CapacityLease,
    requirements: ExecutionRequirements,
    decision: AdmissionDecision,
    audit_record: DeploymentAdmissionRecord,
}

impl<'a> AcquiredExecutionPlacement<'a> {
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
pub enum AcquireExecutionError {
    Lease(PlacementLeaseError),
    /// Defensive invariant: preparation enforces a unique provider/offer key,
    /// so a successful lease should always map back to exactly one candidate.
    LeaseTargetNotFound { provider: String, offer_id: String },
}

impl fmt::Display for AcquireExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lease(error) => write!(f, "capacity lease acquisition failed: {error:?}"),
            Self::LeaseTargetNotFound { provider, offer_id } => write!(
                f,
                "acquired lease {provider}/{offer_id} does not map to the prepared execution plan"
            ),
        }
    }
}

impl std::error::Error for AcquireExecutionError {}

/// Acquire provider capacity from a prepared ranked plan.
///
/// The underlying capacity primitive acquires a durable CAS placement claim
/// first, uses provider-scoped idempotency keys on retries, falls through only
/// on determinate failures, and keeps the claim on indeterminate acquisition so
/// reconciliation happens before any provider fallback.
pub async fn acquire_prepared_execution_placement<'a>(
    plan: &PreparedExecutionPlan<'a>,
    claim_store: &dyn PlacementClaimStore,
    leasers: &[&dyn CapacityLeaser],
    job_id: &str,
    placement_token: &str,
    claim_expires_at_unix_ms: u64,
    requested_ttl_seconds: Option<u64>,
) -> Result<AcquiredExecutionPlacement<'a>, AcquireExecutionError> {
    let placements = plan.placement_candidates();
    let lease = acquire_ranked_placement(
        claim_store,
        leasers,
        &placements,
        job_id,
        placement_token,
        claim_expires_at_unix_ms,
        requested_ttl_seconds,
    )
    .await
    .map_err(AcquireExecutionError::Lease)?;

    let candidate = plan.candidate_for_lease(&lease).ok_or_else(|| {
        AcquireExecutionError::LeaseTargetNotFound {
            provider: lease.provider.clone(),
            offer_id: lease.offer_id.clone(),
        }
    })?;

    Ok(AcquiredExecutionPlacement {
        target: candidate.target,
        lease,
        requirements: plan.requirements.clone(),
        decision: candidate.decision.clone(),
        audit_record: candidate.audit_record.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use futures::executor::block_on;
    use nulang::bytecode::{CodeModule, Constant, Instruction, OpCode};
    use nulang_capacity::broker::PlacementCandidate;
    use nulang_capacity::execution::RuntimeEnvelope;
    use nulang_capacity::lease::{
        AcquireLeaseFuture, CapacityLeaser, ClaimFuture, ClaimResult, LeaseError, LeaseErrorKind,
        LeaseRequest, LeaseState, PlacementClaim, PlacementClaimStore, PlacementLeaseError,
        ReleaseClaimFuture, ReleaseLeaseFuture,
    };
    use nulang_capacity::{Architecture, CapacityOffer, Lifecycle, TrustTier};

    use super::*;

    fn emit_effect(module: &mut CodeModule, effect: &str) {
        let idx = module.add_constant(Constant::String(effect.to_string()));
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((idx >> 8) & 0xff) as u8,
            (idx & 0xff) as u8,
            0,
        ));
    }

    fn deployment_bundle(module: CodeModule) -> DeploymentBundle {
        let nbc = module.to_nbc(None).expect("encode nbc");
        let manifest = b"[package]\nname = \"app\"\nversion = \"0.1.0\"\n";
        let mut bytes = Vec::new();
        {
            let gzip = GzEncoder::new(&mut bytes, Compression::default());
            let mut tar = tar::Builder::new(gzip);
            append(&mut tar, "Nulang.toml", manifest);
            append(&mut tar, ".nula/dist/app.nbc", &nbc);
            let gzip = tar.into_inner().expect("finish tar");
            gzip.finish().expect("finish gzip");
        }
        DeploymentBundle::parse(&bytes).expect("validated bundle")
    }

    fn append<W: std::io::Write>(tar: &mut tar::Builder<W>, path: &str, data: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, path, data)
            .expect("append bundle entry");
    }

    fn target(
        target_id: &str,
        provider: &str,
        offer_id: &str,
        cost: f64,
        isolation: u8,
    ) -> ExecutionTarget {
        ExecutionTarget {
            target_id: target_id.into(),
            placement: PlacementCandidate {
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
            },
            runtime: RuntimeEnvelope {
                runtime_features: ["effects".to_string()].into_iter().collect(),
                isolation_level: isolation,
            },
        }
    }

    fn allowed_net_policy() -> AdmissionPolicy {
        let mut policy = AdmissionPolicy::new("tenant/default", 1);
        policy.allowed_runtime_features.insert("effects".into());
        policy.allowed_external_effect_roots.insert("Net".into());
        policy.minimum_host_boundary_isolation_level = 2;
        policy
    }

    struct MemoryClaimStore;

    impl PlacementClaimStore for MemoryClaimStore {
        fn try_claim<'a>(&'a self, _claim: &'a PlacementClaim) -> ClaimFuture<'a> {
            Box::pin(async { Ok(ClaimResult::Acquired) })
        }

        fn release_claim<'a>(&'a self, _claim: &'a PlacementClaim) -> ReleaseClaimFuture<'a> {
            Box::pin(async { Ok(()) })
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
                    .expect("lease results lock")
                    .pop_front()
                    .expect("configured lease result")
            })
        }

        fn release<'a>(&'a self, _lease: &'a CapacityLease) -> ReleaseLeaseFuture<'a> {
            Box::pin(async { Ok(()) })
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

    fn lease_error(provider: &str, offer_id: &str, kind: LeaseErrorKind) -> LeaseError {
        LeaseError {
            provider: provider.into(),
            offer_id: offer_id.into(),
            kind,
            message: "test failure".into(),
            retryable: false,
        }
    }

    #[test]
    fn preparation_builds_audit_evidence_before_provider_side_effects() {
        let mut module = CodeModule::new("net");
        emit_effect(&mut module, "Net.fetch");
        let bundle = deployment_bundle(module);
        let policy = allowed_net_policy();
        let targets = vec![target("aws-pool", "aws", "offer-a", 0.10, 2)];

        let plan = prepare_execution_plan("dep-1", &bundle, &policy, &targets).unwrap();
        assert_eq!(plan.candidates().len(), 1);
        assert_eq!(plan.candidates()[0].audit_record().deployment_id(), "dep-1");
        assert!(plan.candidates()[0].audit_record().admitted());
    }

    #[test]
    fn duplicate_provider_offer_mapping_is_rejected_before_acquisition() {
        let mut module = CodeModule::new("net");
        emit_effect(&mut module, "Net.fetch");
        let bundle = deployment_bundle(module);
        let policy = allowed_net_policy();
        let targets = vec![
            target("pool-a", "aws", "same-offer", 0.10, 2),
            target("pool-b", "aws", "same-offer", 0.20, 3),
        ];

        assert!(matches!(
            prepare_execution_plan("dep-1", &bundle, &policy, &targets),
            Err(PrepareExecutionError::AmbiguousProviderOffer { .. })
        ));
    }

    #[test]
    fn determinate_capacity_failure_falls_through_to_next_admitted_target() {
        let mut module = CodeModule::new("net");
        emit_effect(&mut module, "Net.fetch");
        let bundle = deployment_bundle(module);
        let policy = allowed_net_policy();
        let targets = vec![
            target("aws-pool", "aws", "aws-offer", 0.10, 2),
            target("gcp-pool", "gcp", "gcp-offer", 0.20, 2),
        ];
        let plan = prepare_execution_plan("dep-1", &bundle, &policy, &targets).unwrap();
        let aws = FakeLeaser::new(
            "aws",
            vec![Err(lease_error("aws", "aws-offer", LeaseErrorKind::CapacityLost))],
        );
        let gcp = FakeLeaser::new("gcp", vec![Ok(lease("gcp", "gcp-offer"))]);
        let claim_store = MemoryClaimStore;

        let acquired = block_on(acquire_prepared_execution_placement(
            &plan,
            &claim_store,
            &[&aws, &gcp],
            "job-1",
            "placement-1",
            10_000,
            Some(900),
        ))
        .expect("fallback acquisition");

        assert_eq!(acquired.target().target_id, "gcp-pool");
        assert_eq!(acquired.lease().provider, "gcp");
        assert_eq!(aws.calls(), 1);
        assert_eq!(gcp.calls(), 1);
        assert!(acquired.audit_record().admitted());
    }

    #[test]
    fn indeterminate_acquisition_stops_before_fallback_provider() {
        let mut module = CodeModule::new("net");
        emit_effect(&mut module, "Net.fetch");
        let bundle = deployment_bundle(module);
        let policy = allowed_net_policy();
        let targets = vec![
            target("aws-pool", "aws", "aws-offer", 0.10, 2),
            target("gcp-pool", "gcp", "gcp-offer", 0.20, 2),
        ];
        let plan = prepare_execution_plan("dep-1", &bundle, &policy, &targets).unwrap();
        let aws = FakeLeaser::new(
            "aws",
            vec![Err(lease_error(
                "aws",
                "aws-offer",
                LeaseErrorKind::Indeterminate,
            ))],
        );
        let gcp = FakeLeaser::new("gcp", vec![Ok(lease("gcp", "gcp-offer"))]);
        let claim_store = MemoryClaimStore;

        let error = block_on(acquire_prepared_execution_placement(
            &plan,
            &claim_store,
            &[&aws, &gcp],
            "job-1",
            "placement-1",
            10_000,
            Some(900),
        ))
        .unwrap_err();

        assert!(matches!(
            error,
            AcquireExecutionError::Lease(PlacementLeaseError::Indeterminate(_))
        ));
        assert_eq!(aws.calls(), 1);
        assert_eq!(gcp.calls(), 0);
    }
}
