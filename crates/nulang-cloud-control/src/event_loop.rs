use crate::model::{DeploymentSpec, Evaluation, EvaluationCause, NodeDescriptor};
use crate::reconciler::{reconcile_once, ReconcileError, ReconcileResult};
use crate::store::ControlStore;
use std::collections::BTreeMap;
use std::fmt;

/// Immutable reconciliation request produced by an external deployment,
/// membership, allocation, capacity, or operator event.
///
/// The caller supplies the evaluation id. The control-plane loop deliberately
/// does not use wall-clock time, randomness, or process-local counters to invent
/// durable identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileRequest {
    pub evaluation: Evaluation,
    pub deployment: DeploymentSpec,
}

impl ReconcileRequest {
    pub fn new(
        evaluation_id: impl Into<String>,
        cause: EvaluationCause,
        deployment: DeploymentSpec,
    ) -> Self {
        let evaluation = Evaluation {
            evaluation_id: evaluation_id.into(),
            deployment_id: deployment.deployment_id.clone(),
            revision: deployment.revision,
            cause,
        };
        Self {
            evaluation,
            deployment,
        }
    }

    fn validate(&self) -> Result<(), ReconcileLoopError> {
        if self.evaluation.evaluation_id.trim().is_empty() {
            return Err(ReconcileLoopError::InvalidRequest(
                "evaluation id must not be empty".into(),
            ));
        }
        if self.evaluation.deployment_id != self.deployment.deployment_id {
            return Err(ReconcileLoopError::InvalidRequest(format!(
                "evaluation deployment {} does not match desired deployment {}",
                self.evaluation.deployment_id, self.deployment.deployment_id
            )));
        }
        if self.evaluation.revision != self.deployment.revision {
            return Err(ReconcileLoopError::InvalidRequest(format!(
                "evaluation revision {} does not match desired revision {}",
                self.evaluation.revision, self.deployment.revision
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// No request for this deployment was pending.
    Enqueued,
    /// An exact retry was already pending; the queue remains unchanged.
    AlreadyQueued,
    /// A newer deployment revision replaced an older pending request.
    SupersededOlderRevision,
    /// The incoming request was for an older revision than the queued request.
    IgnoredStaleRevision,
    /// Same desired revision, but a newer trigger replaced the pending trigger.
    ///
    /// Reconciliation is state-based: one turn against the current durable
    /// allocations and the supplied node snapshot subsumes older triggers for
    /// the same desired revision.
    CoalescedSameRevision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileTurn {
    pub evaluation_id: String,
    pub deployment_id: String,
    pub revision: u64,
    pub result: ReconcileResult,
}

#[derive(Debug)]
pub enum ReconcileLoopError {
    InvalidRequest(String),
    Reconcile(ReconcileError),
}

impl fmt::Display for ReconcileLoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "invalid reconcile request: {message}"),
            Self::Reconcile(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ReconcileLoopError {}

impl From<ReconcileError> for ReconcileLoopError {
    fn from(error: ReconcileError) -> Self {
        Self::Reconcile(error)
    }
}

/// Deterministic in-process reconciliation queue.
///
/// This is intentionally not a second durable database. Durable evaluation,
/// ownership, fencing, plan, and outbox state remain in `ControlStore`.
/// External event delivery is expected to be replayable/redeliverable. The
/// queue only coalesces transient wakeups while a controller process is alive.
///
/// Ordering is stable by deployment id. A failed turn remains at the front of
/// the queue so retry uses the same evaluation id and therefore the same
/// idempotent `ControlStore` identity.
#[derive(Debug, Default)]
pub struct ReconcileLoop {
    pending: BTreeMap<String, ReconcileRequest>,
}

impl ReconcileLoop {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn pending(&self, deployment_id: &str) -> Option<&ReconcileRequest> {
        self.pending.get(deployment_id)
    }

    /// Add or coalesce one state-based reconcile request.
    pub fn enqueue(
        &mut self,
        request: ReconcileRequest,
    ) -> Result<EnqueueOutcome, ReconcileLoopError> {
        request.validate()?;
        let deployment_id = request.deployment.deployment_id.clone();

        let Some(current) = self.pending.get(&deployment_id) else {
            self.pending.insert(deployment_id, request);
            return Ok(EnqueueOutcome::Enqueued);
        };

        if request == *current {
            return Ok(EnqueueOutcome::AlreadyQueued);
        }

        if request.deployment.revision < current.deployment.revision {
            return Ok(EnqueueOutcome::IgnoredStaleRevision);
        }

        let outcome = if request.deployment.revision > current.deployment.revision {
            EnqueueOutcome::SupersededOlderRevision
        } else {
            EnqueueOutcome::CoalescedSameRevision
        };
        self.pending.insert(deployment_id, request);
        Ok(outcome)
    }

    /// Enqueue one trigger for each desired deployment.
    ///
    /// `event_id` must be a durable/event-source identity. The per-deployment
    /// evaluation id is derived deterministically as
    /// `{event_id}:{deployment_id}:{revision}`.
    pub fn enqueue_all(
        &mut self,
        event_id: &str,
        cause: EvaluationCause,
        deployments: impl IntoIterator<Item = DeploymentSpec>,
    ) -> Result<Vec<(String, EnqueueOutcome)>, ReconcileLoopError> {
        if event_id.trim().is_empty() {
            return Err(ReconcileLoopError::InvalidRequest(
                "event id must not be empty".into(),
            ));
        }

        let mut outcomes = Vec::new();
        for deployment in deployments {
            let evaluation_id = format!(
                "{}:{}:{}",
                event_id, deployment.deployment_id, deployment.revision
            );
            let deployment_id = deployment.deployment_id.clone();
            let outcome = self.enqueue(ReconcileRequest::new(
                evaluation_id,
                cause,
                deployment,
            ))?;
            outcomes.push((deployment_id, outcome));
        }
        Ok(outcomes)
    }

    /// Reconcile the lexicographically first pending deployment.
    ///
    /// Success removes the request. Failure keeps the exact request queued so
    /// the caller can retry without manufacturing a new evaluation identity.
    pub fn reconcile_next(
        &mut self,
        store: &dyn ControlStore,
        nodes: &[NodeDescriptor],
    ) -> Result<Option<ReconcileTurn>, ReconcileLoopError> {
        let Some((deployment_id, request)) = self
            .pending
            .first_key_value()
            .map(|(id, request)| (id.clone(), request.clone()))
        else {
            return Ok(None);
        };

        let result = reconcile_once(
            store,
            &request.evaluation,
            &request.deployment,
            nodes,
        )?;

        self.pending.remove(&deployment_id);
        Ok(Some(ReconcileTurn {
            evaluation_id: request.evaluation.evaluation_id,
            deployment_id,
            revision: request.deployment.revision,
            result,
        }))
    }

    /// Drain at most `max_turns` successful reconciliation turns.
    ///
    /// Stops on the first error and preserves the failed request in the queue.
    /// A bound is mandatory so an embedding controller retains explicit
    /// scheduling/backpressure control.
    pub fn reconcile_ready(
        &mut self,
        store: &dyn ControlStore,
        nodes: &[NodeDescriptor],
        max_turns: usize,
    ) -> Result<Vec<ReconcileTurn>, ReconcileLoopError> {
        let mut turns = Vec::new();
        while turns.len() < max_turns {
            match self.reconcile_next(store, nodes)? {
                Some(turn) => turns.push(turn),
                None => break,
            }
        }
        Ok(turns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        EvaluationCause, NodeResources, NodeState, PlacementConstraints, PlacementPreferences,
        ResourceRequest,
    };
    use crate::store::{
        CommitOutcome, ControlStore, EvaluationRecord, MemoryControlStore, StoreError,
    };
    use nulang_capacity::{Architecture, TrustTier};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn deployment(id: &str, revision: u64) -> DeploymentSpec {
        DeploymentSpec {
            deployment_id: id.into(),
            revision,
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
    fn newer_revision_supersedes_pending_older_revision() {
        let mut loop_ = ReconcileLoop::new();
        assert_eq!(
            loop_
                .enqueue(ReconcileRequest::new(
                    "old",
                    EvaluationCause::DeploymentChanged,
                    deployment("api", 1),
                ))
                .unwrap(),
            EnqueueOutcome::Enqueued
        );
        assert_eq!(
            loop_
                .enqueue(ReconcileRequest::new(
                    "new",
                    EvaluationCause::DeploymentChanged,
                    deployment("api", 2),
                ))
                .unwrap(),
            EnqueueOutcome::SupersededOlderRevision
        );
        assert_eq!(loop_.len(), 1);
        assert_eq!(loop_.pending("api").unwrap().evaluation.evaluation_id, "new");
        assert_eq!(loop_.pending("api").unwrap().deployment.revision, 2);
    }

    #[test]
    fn stale_revision_cannot_replace_newer_pending_request() {
        let mut loop_ = ReconcileLoop::new();
        loop_
            .enqueue(ReconcileRequest::new(
                "new",
                EvaluationCause::DeploymentChanged,
                deployment("api", 3),
            ))
            .unwrap();

        assert_eq!(
            loop_
                .enqueue(ReconcileRequest::new(
                    "old",
                    EvaluationCause::NodeChanged,
                    deployment("api", 2),
                ))
                .unwrap(),
            EnqueueOutcome::IgnoredStaleRevision
        );
        assert_eq!(loop_.pending("api").unwrap().evaluation.evaluation_id, "new");
    }

    #[test]
    fn same_revision_trigger_coalesces_to_latest_event_identity() {
        let mut loop_ = ReconcileLoop::new();
        loop_
            .enqueue(ReconcileRequest::new(
                "node-event-1",
                EvaluationCause::NodeChanged,
                deployment("api", 4),
            ))
            .unwrap();

        assert_eq!(
            loop_
                .enqueue(ReconcileRequest::new(
                    "capacity-event-2",
                    EvaluationCause::CapacityChanged,
                    deployment("api", 4),
                ))
                .unwrap(),
            EnqueueOutcome::CoalescedSameRevision
        );
        let request = loop_.pending("api").unwrap();
        assert_eq!(request.evaluation.evaluation_id, "capacity-event-2");
        assert_eq!(request.evaluation.cause, EvaluationCause::CapacityChanged);
    }

    #[test]
    fn enqueue_all_derives_stable_per_deployment_ids() {
        let mut loop_ = ReconcileLoop::new();
        let outcomes = loop_
            .enqueue_all(
                "membership-42",
                EvaluationCause::NodeChanged,
                [deployment("api", 2), deployment("worker", 7)],
            )
            .unwrap();

        assert_eq!(outcomes.len(), 2);
        assert_eq!(
            loop_.pending("api").unwrap().evaluation.evaluation_id,
            "membership-42:api:2"
        );
        assert_eq!(
            loop_.pending("worker").unwrap().evaluation.evaluation_id,
            "membership-42:worker:7"
        );
    }

    #[test]
    fn reconcile_next_is_stably_ordered_and_commits_outbox() {
        let store = MemoryControlStore::default();
        let mut loop_ = ReconcileLoop::new();
        loop_
            .enqueue(ReconcileRequest::new(
                "z-eval",
                EvaluationCause::Manual,
                deployment("z-worker", 1),
            ))
            .unwrap();
        loop_
            .enqueue(ReconcileRequest::new(
                "a-eval",
                EvaluationCause::Manual,
                deployment("a-api", 1),
            ))
            .unwrap();

        let first = loop_.reconcile_next(&store, &[node(1)]).unwrap().unwrap();
        assert_eq!(first.deployment_id, "a-api");
        assert_eq!(first.result.commit, CommitOutcome::Applied);
        assert_eq!(loop_.len(), 1);

        let commands = store.pending_commands().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].deployment_id, "a-api");
    }

    struct FailOnceStore {
        inner: MemoryControlStore,
        fail_record_once: AtomicBool,
    }

    impl FailOnceStore {
        fn new() -> Self {
            Self {
                inner: MemoryControlStore::default(),
                fail_record_once: AtomicBool::new(true),
            }
        }
    }

    impl ControlStore for FailOnceStore {
        fn record_evaluation(&self, evaluation: &Evaluation) -> Result<(), StoreError> {
            if self.fail_record_once.swap(false, Ordering::SeqCst) {
                return Err(StoreError::Backend("injected record failure".into()));
            }
            self.inner.record_evaluation(evaluation)
        }

        fn evaluation(&self, evaluation_id: &str) -> Result<Option<EvaluationRecord>, StoreError> {
            self.inner.evaluation(evaluation_id)
        }

        fn committed_plan(
            &self,
            evaluation_id: &str,
        ) -> Result<Option<crate::model::PlacementPlan>, StoreError> {
            self.inner.committed_plan(evaluation_id)
        }

        fn allocations_for(
            &self,
            deployment_id: &str,
        ) -> Result<Vec<crate::model::ObservedAllocation>, StoreError> {
            self.inner.allocations_for(deployment_id)
        }

        fn commit_plan(
            &self,
            evaluation: &Evaluation,
            plan: &crate::model::PlacementPlan,
        ) -> Result<CommitOutcome, StoreError> {
            self.inner.commit_plan(evaluation, plan)
        }

        fn pending_commands(
            &self,
        ) -> Result<Vec<crate::store::AllocationCommand>, StoreError> {
            self.inner.pending_commands()
        }

        fn acknowledge_command(&self, command_id: &str) -> Result<(), StoreError> {
            self.inner.acknowledge_command(command_id)
        }
    }

    #[test]
    fn failed_turn_remains_queued_and_retries_same_evaluation() {
        let store = FailOnceStore::new();
        let mut loop_ = ReconcileLoop::new();
        loop_
            .enqueue(ReconcileRequest::new(
                "eval-stable",
                EvaluationCause::Manual,
                deployment("api", 1),
            ))
            .unwrap();

        assert!(matches!(
            loop_.reconcile_next(&store, &[node(1)]),
            Err(ReconcileLoopError::Reconcile(ReconcileError::Store(
                StoreError::Backend(_)
            )))
        ));
        assert_eq!(loop_.len(), 1);
        assert_eq!(
            loop_.pending("api").unwrap().evaluation.evaluation_id,
            "eval-stable"
        );

        let turn = loop_.reconcile_next(&store, &[node(1)]).unwrap().unwrap();
        assert_eq!(turn.evaluation_id, "eval-stable");
        assert!(loop_.is_empty());
    }

    #[test]
    fn reconcile_ready_honors_explicit_turn_budget() {
        let store = MemoryControlStore::default();
        let mut loop_ = ReconcileLoop::new();
        for id in ["a", "b", "c"] {
            loop_
                .enqueue(ReconcileRequest::new(
                    format!("eval-{id}"),
                    EvaluationCause::Manual,
                    deployment(id, 1),
                ))
                .unwrap();
        }

        let turns = loop_.reconcile_ready(&store, &[node(1)], 2).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(loop_.len(), 1);
        assert_eq!(loop_.pending("c").unwrap().evaluation.evaluation_id, "eval-c");
    }
}
