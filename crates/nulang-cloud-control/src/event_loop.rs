use crate::model::{DeploymentSpec, Evaluation, NodeDescriptor, ObservedAllocation};
use crate::reconciler::{reconcile_once, ReconcileError, ReconcileResult};
use crate::store::{AllocationObservationOutcome, ControlStore, StoreError};
use std::collections::{HashMap, HashSet};
use std::fmt;

/// One durable reconciliation trigger.
///
/// Upstream adapters should translate deployment, node, allocation, capacity,
/// and manual changes into this complete snapshot. The event source owns
/// ordering and stable IDs; the control plane deliberately does not embed a
/// second message broker or invent offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileEvent {
    pub event_id: String,
    pub evaluation: Evaluation,
    pub deployment: DeploymentSpec,
    pub nodes: Vec<NodeDescriptor>,
    pub allocation_observation: Option<ObservedAllocation>,
}

impl ReconcileEvent {
    fn validate(&self) -> Result<(), ReconcileLoopError> {
        if self.event_id.trim().is_empty() {
            return Err(ReconcileLoopError::InvalidEvent {
                event_id: self.event_id.clone(),
                message: "event id must not be empty".into(),
            });
        }
        if self.evaluation.evaluation_id.trim().is_empty() {
            return Err(ReconcileLoopError::InvalidEvent {
                event_id: self.event_id.clone(),
                message: "evaluation id must not be empty".into(),
            });
        }
        if self.evaluation.deployment_id != self.deployment.deployment_id {
            return Err(ReconcileLoopError::InvalidEvent {
                event_id: self.event_id.clone(),
                message: format!(
                    "evaluation deployment {} does not match deployment snapshot {}",
                    self.evaluation.deployment_id, self.deployment.deployment_id
                ),
            });
        }
        if self.evaluation.revision != self.deployment.revision {
            return Err(ReconcileLoopError::InvalidEvent {
                event_id: self.event_id.clone(),
                message: format!(
                    "evaluation revision {} does not match deployment revision {}",
                    self.evaluation.revision, self.deployment.revision
                ),
            });
        }
        if let Some(observation) = &self.allocation_observation {
            if observation.deployment_id != self.deployment.deployment_id {
                return Err(ReconcileLoopError::InvalidEvent {
                    event_id: self.event_id.clone(),
                    message: format!(
                        "allocation observation deployment {} does not match event deployment {}",
                        observation.deployment_id, self.deployment.deployment_id
                    ),
                });
            }
        }

        let mut node_ids = HashSet::new();
        for node in &self.nodes {
            if !node_ids.insert(node.node_id) {
                return Err(ReconcileLoopError::InvalidEvent {
                    event_id: self.event_id.clone(),
                    message: format!("node snapshot contains duplicate node id {}", node.node_id),
                });
            }
        }

        Ok(())
    }

    fn same_payload(&self, other: &Self) -> bool {
        self.evaluation == other.evaluation
            && self.deployment == other.deployment
            && self.nodes == other.nodes
            && self.allocation_observation == other.allocation_observation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileEventSourceError {
    pub message: String,
    pub retryable: bool,
}

impl ReconcileEventSourceError {
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }

    pub fn terminal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

impl fmt::Display for ReconcileEventSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ReconcileEventSourceError {}

/// Durable/at-least-once source boundary for control-plane changes.
///
/// poll must return events oldest-to-newest for one source partition. ACKs
/// must be idempotent. A crash after plan commit but before source ACK is safe:
/// the same stable evaluation ID is replayed through reconcile_once, which
/// returns the already committed plan.
pub trait ReconcileEventSource: Send + Sync {
    fn poll(&self, limit: usize) -> Result<Vec<ReconcileEvent>, ReconcileEventSourceError>;
    fn acknowledge(&self, event_id: &str) -> Result<(), ReconcileEventSourceError>;
}

#[derive(Debug)]
pub enum ReconcileLoopError {
    Source(ReconcileEventSourceError),
    InvalidEvent { event_id: String, message: String },
    Store(StoreError),
    Reconcile(ReconcileError),
}

impl fmt::Display for ReconcileLoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(f, "reconcile event source error: {error}"),
            Self::InvalidEvent { event_id, message } => {
                write!(f, "invalid reconcile event {event_id}: {message}")
            }
            Self::Store(error) => write!(f, "{error}"),
            Self::Reconcile(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ReconcileLoopError {}

impl From<ReconcileEventSourceError> for ReconcileLoopError {
    fn from(value: ReconcileEventSourceError) -> Self {
        Self::Source(value)
    }
}

impl From<StoreError> for ReconcileLoopError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<ReconcileError> for ReconcileLoopError {
    fn from(value: ReconcileError) -> Self {
        Self::Reconcile(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileBatchRecord {
    pub event_ids: Vec<String>,
    pub evaluation_id: String,
    pub observation: Option<AllocationObservationOutcome>,
    pub result: ReconcileResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileBatchReport {
    pub polled_events: usize,
    pub reconciled_evaluations: usize,
    pub acknowledged_events: usize,
    pub records: Vec<ReconcileBatchRecord>,
}

#[derive(Debug)]
struct CoalescedEvent {
    event: ReconcileEvent,
    event_ids: Vec<String>,
}

/// Process one bounded reconciliation batch.
///
/// Only equivalent logical redeliveries with the same stable evaluation ID and
/// same typed payload are coalesced. Distinct evaluations are processed in
/// source order. This is intentionally conservative: coalescing unrelated
/// deployment/node/allocation changes would require a durable source checkpoint
/// or atomic batch ACK to remain correct across controller crashes.
pub fn process_reconcile_batch(
    store: &dyn ControlStore,
    source: &dyn ReconcileEventSource,
    limit: usize,
) -> Result<ReconcileBatchReport, ReconcileLoopError> {
    if limit == 0 {
        return Ok(ReconcileBatchReport::default());
    }

    let polled = source.poll(limit)?;
    let events: Vec<_> = polled.into_iter().take(limit).collect();
    let mut report = ReconcileBatchReport {
        polled_events: events.len(),
        ..ReconcileBatchReport::default()
    };

    let mut groups: Vec<CoalescedEvent> = Vec::new();
    let mut evaluation_to_group = HashMap::<String, usize>::new();
    let mut event_payloads = HashMap::<String, ReconcileEvent>::new();

    for event in events {
        event.validate()?;

        if let Some(existing) = event_payloads.get(&event.event_id) {
            if !existing.same_payload(&event) {
                return Err(ReconcileLoopError::InvalidEvent {
                    event_id: event.event_id.clone(),
                    message: "event id was reused with different content".into(),
                });
            }
            continue;
        }
        event_payloads.insert(event.event_id.clone(), event.clone());

        let evaluation_id = event.evaluation.evaluation_id.clone();
        if let Some(index) = evaluation_to_group.get(&evaluation_id).copied() {
            let group = &mut groups[index];
            if !group.event.same_payload(&event) {
                return Err(ReconcileLoopError::InvalidEvent {
                    event_id: event.event_id.clone(),
                    message: format!(
                        "evaluation id {evaluation_id} was reused with different event content"
                    ),
                });
            }
            group.event_ids.push(event.event_id);
        } else {
            evaluation_to_group.insert(evaluation_id, groups.len());
            groups.push(CoalescedEvent {
                event_ids: vec![event.event_id.clone()],
                event,
            });
        }
    }

    for group in groups {
        let observation = match &group.event.allocation_observation {
            Some(allocation) => Some(store.observe_allocation(allocation)?),
            None => None,
        };

        let result = reconcile_once(
            store,
            &group.event.evaluation,
            &group.event.deployment,
            &group.event.nodes,
        )?;

        for event_id in &group.event_ids {
            source.acknowledge(event_id)?;
            report.acknowledged_events += 1;
        }

        report.reconciled_evaluations += 1;
        report.records.push(ReconcileBatchRecord {
            event_ids: group.event_ids,
            evaluation_id: group.event.evaluation.evaluation_id,
            observation,
            result,
        });
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AllocationState, EvaluationCause, NodeResources, NodeState, PlacementConstraints,
        PlacementPreferences, ResourceRequest,
    };
    use crate::store::{AllocationCommandKind, CommitOutcome, MemoryControlStore};
    use nulang_capacity::{Architecture, TrustTier};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };

    #[derive(Default)]
    struct MemoryEventSource {
        queue: Mutex<Vec<ReconcileEvent>>,
        acknowledged: Mutex<Vec<String>>,
        fail_next_ack: AtomicBool,
    }

    impl MemoryEventSource {
        fn new(events: Vec<ReconcileEvent>) -> Self {
            Self {
                queue: Mutex::new(events),
                acknowledged: Mutex::new(Vec::new()),
                fail_next_ack: AtomicBool::new(false),
            }
        }

        fn fail_next_ack(&self) {
            self.fail_next_ack.store(true, Ordering::SeqCst);
        }
    }

    impl ReconcileEventSource for MemoryEventSource {
        fn poll(&self, limit: usize) -> Result<Vec<ReconcileEvent>, ReconcileEventSourceError> {
            Ok(self
                .queue
                .lock()
                .unwrap()
                .iter()
                .take(limit)
                .cloned()
                .collect())
        }

        fn acknowledge(&self, event_id: &str) -> Result<(), ReconcileEventSourceError> {
            if self.fail_next_ack.swap(false, Ordering::SeqCst) {
                return Err(ReconcileEventSourceError::retryable(
                    "injected acknowledgement failure",
                ));
            }

            let mut queue = self.queue.lock().unwrap();
            if let Some(index) = queue.iter().position(|event| event.event_id == event_id) {
                queue.remove(index);
            }
            self.acknowledged.lock().unwrap().push(event_id.to_owned());
            Ok(())
        }
    }

    fn deployment(revision: u64) -> DeploymentSpec {
        DeploymentSpec {
            deployment_id: "api".into(),
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

    fn event(event_id: &str, evaluation_id: &str, revision: u64) -> ReconcileEvent {
        ReconcileEvent {
            event_id: event_id.into(),
            evaluation: Evaluation {
                evaluation_id: evaluation_id.into(),
                deployment_id: "api".into(),
                revision,
                cause: EvaluationCause::DeploymentChanged,
            },
            deployment: deployment(revision),
            nodes: vec![node(1)],
            allocation_observation: None,
        }
    }

    #[test]
    fn duplicate_logical_redeliveries_coalesce_without_duplicate_plan_or_outbox() {
        let store = MemoryControlStore::default();
        let first = event("source-1", "eval-1", 1);
        let mut duplicate = first.clone();
        duplicate.event_id = "source-2".into();
        let source = MemoryEventSource::new(vec![first, duplicate]);

        let report = process_reconcile_batch(&store, &source, 16).unwrap();

        assert_eq!(report.polled_events, 2);
        assert_eq!(report.reconciled_evaluations, 1);
        assert_eq!(report.acknowledged_events, 2);
        assert_eq!(report.records[0].event_ids, vec!["source-1", "source-2"]);
        assert_eq!(report.records[0].result.commit, CommitOutcome::Applied);
        assert_eq!(store.pending_commands().unwrap().len(), 1);
        assert!(source.queue.lock().unwrap().is_empty());
    }

    #[test]
    fn source_ack_failure_replays_already_committed_evaluation_safely() {
        let store = MemoryControlStore::default();
        let source = MemoryEventSource::new(vec![event("source-1", "eval-1", 1)]);
        source.fail_next_ack();

        assert!(matches!(
            process_reconcile_batch(&store, &source, 16),
            Err(ReconcileLoopError::Source(ReconcileEventSourceError {
                retryable: true,
                ..
            }))
        ));
        assert_eq!(store.pending_commands().unwrap().len(), 1);

        let retry = process_reconcile_batch(&store, &source, 16).unwrap();
        assert_eq!(retry.records.len(), 1);
        assert_eq!(
            retry.records[0].result.commit,
            CommitOutcome::AlreadyCommitted
        );
        assert!(source.queue.lock().unwrap().is_empty());
        assert_eq!(store.pending_commands().unwrap().len(), 1);
    }

    #[test]
    fn failed_allocation_observation_drives_replacement_epoch() {
        let store = MemoryControlStore::default();
        let initial = event("source-initial", "eval-initial", 1);
        let initial_source = MemoryEventSource::new(vec![initial]);
        process_reconcile_batch(&store, &initial_source, 16).unwrap();

        let original_command = store.pending_commands().unwrap().pop().unwrap();
        assert_eq!(original_command.kind, AllocationCommandKind::Start);
        store
            .acknowledge_command(&original_command.command_id)
            .unwrap();

        let mut failed = store.allocations_for("api").unwrap().pop().unwrap();
        failed.state = AllocationState::Failed;

        let source = MemoryEventSource::new(vec![ReconcileEvent {
            event_id: "source-failed".into(),
            evaluation: Evaluation {
                evaluation_id: "eval-failed".into(),
                deployment_id: "api".into(),
                revision: 1,
                cause: EvaluationCause::AllocationFailed,
            },
            deployment: deployment(1),
            nodes: vec![node(1)],
            allocation_observation: Some(failed),
        }]);

        let report = process_reconcile_batch(&store, &source, 16).unwrap();
        assert_eq!(
            report.records[0].observation,
            Some(AllocationObservationOutcome::Applied)
        );
        assert_eq!(report.records[0].result.plan.placements.len(), 1);
        assert_eq!(report.records[0].result.plan.placements[0].epoch, 2);

        let commands = store.pending_commands().unwrap();
        assert!(commands
            .iter()
            .any(|command| { command.kind == AllocationCommandKind::Start && command.epoch == 2 }));
    }

    #[test]
    fn evaluation_id_reuse_with_different_snapshot_fails_closed() {
        let store = MemoryControlStore::default();
        let first = event("source-1", "eval-1", 1);
        let mut conflicting = first.clone();
        conflicting.event_id = "source-2".into();
        conflicting.nodes = vec![node(2)];
        let source = MemoryEventSource::new(vec![first, conflicting]);

        assert!(matches!(
            process_reconcile_batch(&store, &source, 16),
            Err(ReconcileLoopError::InvalidEvent { .. })
        ));
        assert!(store.pending_commands().unwrap().is_empty());
    }
}
