use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use flate2::write::GzEncoder;
use flate2::Compression;
use futures::executor::block_on;
use nulang::admission_policy::AdmissionPolicy;
use nulang::bytecode::{CodeModule, Constant, Instruction, OpCode};
use nulang::deployment_bundle::DeploymentBundle;
use nulang_capacity::broker::PlacementCandidate;
use nulang_capacity::execution::{ExecutionTarget, RuntimeEnvelope};
use nulang_capacity::lease::{
    AcquireLeaseFuture, CapacityLease, CapacityLeaser, ClaimFuture, ClaimResult, LeaseError,
    LeaseErrorKind, LeaseRequest, LeaseState, PlacementClaim, PlacementClaimStore,
    PlacementLeaseError, ReleaseClaimFuture, ReleaseLeaseFuture,
};
use nulang_capacity::reconcile::{
    CapacityLeaseReconciler, LeaseReconciliationError, LeaseReconciliationStatus,
    ReconcileLeaseFuture,
};
use nulang_capacity::{Architecture, CapacityOffer, Lifecycle, TrustTier};
use nulang_cloud_control::acquisition::{
    acquire_prepared_execution_placement, prepare_execution_plan, AcquireExecutionError,
};
use nulang_cloud_control::reconciliation::{
    reconcile_prepared_execution_placement, ExecutionReconciliationResult,
};

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

fn target(target_id: &str, provider: &str, offer_id: &str, cost: f64) -> ExecutionTarget {
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
            isolation_level: 2,
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

struct StickyClaimStore {
    calls: AtomicUsize,
}

impl StickyClaimStore {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

impl PlacementClaimStore for StickyClaimStore {
    fn try_claim<'a>(&'a self, _claim: &'a PlacementClaim) -> ClaimFuture<'a> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Ok(ClaimResult::Acquired)
            } else {
                Ok(ClaimResult::AlreadyHeldByCaller)
            }
        })
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
                .expect("lease result lock")
                .pop_front()
                .expect("configured lease result")
        })
    }

    fn release<'a>(&'a self, _lease: &'a CapacityLease) -> ReleaseLeaseFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

struct SequencedReconciler {
    provider: String,
    statuses: Mutex<VecDeque<LeaseReconciliationStatus>>,
}

impl SequencedReconciler {
    fn new(provider: &str, statuses: Vec<LeaseReconciliationStatus>) -> Self {
        Self {
            provider: provider.into(),
            statuses: Mutex::new(statuses.into()),
        }
    }
}

impl CapacityLeaseReconciler for SequencedReconciler {
    fn provider_id(&self) -> &str {
        &self.provider
    }

    fn reconcile<'a>(&'a self, _request: &'a LeaseRequest) -> ReconcileLeaseFuture<'a> {
        Box::pin(async move {
            self.statuses
                .lock()
                .expect("reconciliation status lock")
                .pop_front()
                .ok_or_else(|| LeaseReconciliationError {
                    provider: self.provider.clone(),
                    offer_id: "aws-offer".into(),
                    message: "no configured reconciliation status".into(),
                    retryable: false,
                })
        })
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

fn indeterminate(provider: &str, offer_id: &str) -> LeaseError {
    LeaseError {
        provider: provider.into(),
        offer_id: offer_id.into(),
        kind: LeaseErrorKind::Indeterminate,
        message: "provider outcome unknown".into(),
        retryable: true,
    }
}

#[test]
fn pending_then_confirmed_absent_resumes_with_correct_audit_evidence() {
    let mut module = CodeModule::new("net");
    emit_effect(&mut module, "Net.fetch");
    let bundle = deployment_bundle(module);
    let policy = allowed_net_policy();
    let targets = vec![
        target("aws-pool", "aws", "aws-offer", 0.10),
        target("gcp-pool", "gcp", "gcp-offer", 0.20),
    ];
    let plan = prepare_execution_plan("dep-reconcile", &bundle, &policy, &targets)
        .expect("prepare execution plan");

    let aws = FakeLeaser::new("aws", vec![Err(indeterminate("aws", "aws-offer"))]);
    let gcp_lease = lease("gcp", "gcp-offer");
    let gcp = FakeLeaser::new("gcp", vec![Ok(gcp_lease.clone())]);
    let claim_store = StickyClaimStore::new();

    let blocked = block_on(acquire_prepared_execution_placement(
        &plan,
        &claim_store,
        &[&aws, &gcp],
        "job-1",
        "placement-1",
        10_000,
        Some(900),
    ))
    .expect_err("AWS result should require reconciliation");

    let blocked_lease_error = match blocked {
        AcquireExecutionError::Lease(error @ PlacementLeaseError::Indeterminate(_)) => error,
        other => panic!("expected indeterminate lease error, got {other:?}"),
    };
    assert_eq!(gcp.calls(), 0, "initial fallback must be blocked");

    let reconciler = SequencedReconciler::new(
        "aws",
        vec![
            LeaseReconciliationStatus::Pending,
            LeaseReconciliationStatus::ConfirmedAbsent,
        ],
    );

    let pending = block_on(reconcile_prepared_execution_placement(
        &plan,
        &blocked_lease_error,
        &claim_store,
        &[&reconciler],
        &[&aws, &gcp],
        10_000,
    ))
    .expect("pending reconciliation");
    assert!(matches!(pending, ExecutionReconciliationResult::Pending));
    assert_eq!(gcp.calls(), 0, "pending must not contact fallback provider");

    let resumed = block_on(reconcile_prepared_execution_placement(
        &plan,
        &blocked_lease_error,
        &claim_store,
        &[&reconciler],
        &[&aws, &gcp],
        10_000,
    ))
    .expect("confirmed absence should resume fallback");

    let placement = match resumed {
        ExecutionReconciliationResult::Placement(placement) => placement,
        ExecutionReconciliationResult::Pending => panic!("expected resumed placement"),
    };
    assert_eq!(placement.target().target_id, "gcp-pool");
    assert_eq!(placement.lease(), &gcp_lease);
    assert_eq!(placement.audit_record().deployment_id(), "dep-reconcile");
    assert!(placement.audit_record().admitted());
    assert_eq!(gcp.calls(), 1);
}
