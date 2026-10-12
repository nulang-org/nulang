use crate::model::{DeploymentSpec, Evaluation, NodeDescriptor, PlacementPlan, PlanError};
use crate::scheduler::plan_evaluation;
use crate::store::{CommitOutcome, ControlStore, StoreError};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileResult {
    pub plan: PlacementPlan,
    pub commit: CommitOutcome,
}

#[derive(Debug)]
pub enum ReconcileError {
    Store(StoreError),
    Plan(PlanError),
}

impl fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::Plan(error) => write!(f, "placement planning failed: {error:?}"),
        }
    }
}

impl std::error::Error for ReconcileError {}

impl From<StoreError> for ReconcileError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<PlanError> for ReconcileError {
    fn from(value: PlanError) -> Self {
        Self::Plan(value)
    }
}

/// Execute one idempotent reconciliation turn.
///
/// The evaluation is persisted before planning. If the same evaluation already
/// committed, its durable plan is returned directly instead of being recomputed
/// against a newer cluster snapshot. Otherwise the current durable allocation
/// set is read, a pure plan is built, and the store atomically validates epochs,
/// installs allocation ownership, and writes start/stop outbox commands.
pub fn reconcile_once(
    store: &dyn ControlStore,
    evaluation: &Evaluation,
    deployment: &DeploymentSpec,
    nodes: &[NodeDescriptor],
) -> Result<ReconcileResult, ReconcileError> {
    store.record_evaluation(evaluation)?;

    if let Some(plan) = store.committed_plan(&evaluation.evaluation_id)? {
        return Ok(ReconcileResult {
            plan,
            commit: CommitOutcome::AlreadyCommitted,
        });
    }

    let allocations = store.allocations_for(&deployment.deployment_id)?;
    let plan = plan_evaluation(evaluation, deployment, nodes, &allocations)?;
    let commit = store.commit_plan(evaluation, &plan)?;

    Ok(ReconcileResult { plan, commit })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        EvaluationCause, NodeResources, NodeState, PlacementConstraints, PlacementPreferences,
        ResourceRequest,
    };
    use crate::store::{AllocationCommandKind, MemoryControlStore};
    use nulang_capacity::{Architecture, TrustTier};
    use std::collections::{BTreeMap, BTreeSet};

    fn deployment() -> DeploymentSpec {
        DeploymentSpec {
            deployment_id: "api".into(),
            revision: 1,
            replicas: 1,
            resources: ResourceRequest {
                cpu_millis: 250,
                memory_mib: 128,
                accelerator: None,
            },
            constraints: PlacementConstraints::default(),
            preferences: PlacementPreferences::default(),
        }
    }

    fn evaluation() -> Evaluation {
        Evaluation {
            evaluation_id: "eval-1".into(),
            deployment_id: "api".into(),
            revision: 1,
            cause: EvaluationCause::Manual,
        }
    }

    fn node(id: u64) -> NodeDescriptor {
        NodeDescriptor {
            node_id: id,
            state: NodeState::Ready,
            region: "us-east".into(),
            zone: Some("a".into()),
            architecture: Architecture::X86_64,
            trust_tier: TrustTier::CloudProvider,
            labels: BTreeMap::new(),
            capabilities: BTreeSet::new(),
            resources: NodeResources {
                cpu_millis_total: 2_000,
                cpu_millis_available: 2_000,
                memory_mib_total: 2_048,
                memory_mib_available: 2_048,
                accelerators: BTreeMap::new(),
            },
        }
    }

    #[test]
    fn test_reconcile_retry_returns_durable_plan_instead_of_replanning() {
        let store = MemoryControlStore::default();
        let eval = evaluation();
        let first = reconcile_once(&store, &eval, &deployment(), &[node(1)]).unwrap();
        assert_eq!(first.commit, CommitOutcome::Applied);
        assert_eq!(first.plan.placements[0].node_id, 1);

        let retry = reconcile_once(&store, &eval, &deployment(), &[node(2)]).unwrap();
        assert_eq!(retry.commit, CommitOutcome::AlreadyCommitted);
        assert_eq!(retry.plan.placements[0].node_id, 1);

        let commands = store.pending_commands().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].kind, AllocationCommandKind::Start);
        assert_eq!(commands[0].node_id, 1);
    }

    #[test]
    fn test_second_evaluation_observes_first_committed_epoch() {
        let store = MemoryControlStore::default();
        let spec = deployment();
        let first = evaluation();
        reconcile_once(&store, &first, &spec, &[node(1)]).unwrap();

        let second = Evaluation {
            evaluation_id: "eval-2".into(),
            ..first
        };
        let result = reconcile_once(&store, &second, &spec, &[node(1)]).unwrap();

        assert!(result.plan.placements.is_empty());
        assert_eq!(result.plan.retained.len(), 1);
        assert_eq!(result.plan.retained[0].epoch, 1);
    }
}
