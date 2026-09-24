use crate::model::{
    AllocationState, Evaluation, ObservedAllocation, PlacementPlan, SupersededReason,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationStatus {
    Pending,
    Committed,
    CommittedPartial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationRecord {
    pub evaluation: Evaluation,
    pub status: EvaluationStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationCommandKind {
    Start,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllocationCommandState {
    Pending,
    Acknowledged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationCommand {
    pub command_id: String,
    pub evaluation_id: String,
    pub deployment_id: String,
    pub revision: u64,
    pub replica: u32,
    pub node_id: u64,
    pub epoch: u64,
    pub kind: AllocationCommandKind,
    pub state: AllocationCommandState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    Applied,
    AlreadyCommitted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    Serialization(String),
    UnknownEvaluation(String),
    EvaluationConflict(String),
    InvalidPlan(String),
    StalePlan {
        deployment_id: String,
        replica: u32,
        expected_epoch: u64,
        actual_epoch: u64,
    },
    CommandNotFound(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "control-store I/O error: {message}"),
            Self::Serialization(message) => {
                write!(f, "control-store serialization error: {message}")
            }
            Self::UnknownEvaluation(id) => write!(f, "unknown evaluation {id}"),
            Self::EvaluationConflict(id) => {
                write!(f, "evaluation {id} was reused with different content")
            }
            Self::InvalidPlan(message) => write!(f, "invalid placement plan: {message}"),
            Self::StalePlan {
                deployment_id,
                replica,
                expected_epoch,
                actual_epoch,
            } => write!(
                f,
                "stale placement plan for {deployment_id} replica {replica}: expected epoch {expected_epoch}, current epoch {actual_epoch}"
            ),
            Self::CommandNotFound(id) => write!(f, "allocation command {id} was not found"),
        }
    }
}

impl std::error::Error for StoreError {}

pub trait ControlStore: Send + Sync {
    /// Persist an evaluation before planning. Exact retries are idempotent.
    fn record_evaluation(&self, evaluation: &Evaluation) -> Result<(), StoreError>;

    fn evaluation(&self, evaluation_id: &str) -> Result<Option<EvaluationRecord>, StoreError>;

    fn committed_plan(&self, evaluation_id: &str) -> Result<Option<PlacementPlan>, StoreError>;

    fn allocations_for(&self, deployment_id: &str) -> Result<Vec<ObservedAllocation>, StoreError>;

    /// Atomically commit the plan, allocation ownership changes, and execution
    /// outbox commands. Implementations must compare current allocation epochs
    /// before making any mutation visible.
    fn commit_plan(
        &self,
        evaluation: &Evaluation,
        plan: &PlacementPlan,
    ) -> Result<CommitOutcome, StoreError>;

    fn pending_commands(&self) -> Result<Vec<AllocationCommand>, StoreError>;

    /// Idempotently mark a durable outbox command acknowledged.
    fn acknowledge_command(&self, command_id: &str) -> Result<(), StoreError>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    evaluations: BTreeMap<String, EvaluationRecord>,
    #[serde(default)]
    plans: BTreeMap<String, PlacementPlan>,
    #[serde(default)]
    allocations: Vec<ObservedAllocation>,
    #[serde(default)]
    commands: BTreeMap<String, AllocationCommand>,
}

impl PersistedState {
    fn record_evaluation(&mut self, evaluation: &Evaluation) -> Result<(), StoreError> {
        match self.evaluations.get(&evaluation.evaluation_id) {
            Some(existing) if existing.evaluation == *evaluation => Ok(()),
            Some(_) => Err(StoreError::EvaluationConflict(
                evaluation.evaluation_id.clone(),
            )),
            None => {
                self.evaluations.insert(
                    evaluation.evaluation_id.clone(),
                    EvaluationRecord {
                        evaluation: evaluation.clone(),
                        status: EvaluationStatus::Pending,
                    },
                );
                Ok(())
            }
        }
    }

    fn commit_plan(
        &mut self,
        evaluation: &Evaluation,
        plan: &PlacementPlan,
    ) -> Result<CommitOutcome, StoreError> {
        validate_plan_identity(evaluation, plan)?;

        let Some(record) = self.evaluations.get(&evaluation.evaluation_id) else {
            return Err(StoreError::UnknownEvaluation(
                evaluation.evaluation_id.clone(),
            ));
        };
        if record.evaluation != *evaluation {
            return Err(StoreError::EvaluationConflict(
                evaluation.evaluation_id.clone(),
            ));
        }

        if let Some(existing) = self.plans.get(&evaluation.evaluation_id) {
            return if existing == plan {
                Ok(CommitOutcome::AlreadyCommitted)
            } else {
                Err(StoreError::EvaluationConflict(
                    evaluation.evaluation_id.clone(),
                ))
            };
        }

        self.validate_epochs(plan)?;

        for superseded in &plan.superseded {
            if let Some(existing) = self
                .allocations
                .iter_mut()
                .find(|allocation| same_allocation_identity(allocation, &superseded.allocation))
            {
                if existing.state.is_active() {
                    existing.state = AllocationState::Stopped;
                }
            }

            if superseded.allocation.state.is_active() {
                let command = stop_command(evaluation, &superseded.allocation);
                self.commands
                    .entry(command.command_id.clone())
                    .or_insert(command);
            }
        }

        for placement in &plan.placements {
            let allocation = ObservedAllocation {
                deployment_id: plan.deployment_id.clone(),
                revision: plan.revision,
                replica: placement.replica,
                node_id: placement.node_id,
                epoch: placement.epoch,
                state: AllocationState::Starting,
            };
            self.allocations.push(allocation.clone());
            let command = start_command(evaluation, &allocation);
            self.commands
                .entry(command.command_id.clone())
                .or_insert(command);
        }

        self.plans
            .insert(evaluation.evaluation_id.clone(), plan.clone());
        let record = self
            .evaluations
            .get_mut(&evaluation.evaluation_id)
            .expect("evaluation existence validated before commit");
        record.status = if plan.blocked.is_some() {
            EvaluationStatus::CommittedPartial
        } else {
            EvaluationStatus::Committed
        };

        Ok(CommitOutcome::Applied)
    }

    fn validate_epochs(&self, plan: &PlacementPlan) -> Result<(), StoreError> {
        for retained in &plan.retained {
            let actual_epoch = self.max_epoch(&plan.deployment_id, retained.replica);
            if actual_epoch != retained.epoch
                || !self.allocations.iter().any(|allocation| {
                    same_allocation_identity(allocation, retained) && allocation.state.is_active()
                })
            {
                return Err(StoreError::StalePlan {
                    deployment_id: plan.deployment_id.clone(),
                    replica: retained.replica,
                    expected_epoch: retained.epoch,
                    actual_epoch,
                });
            }
        }

        for placement in &plan.placements {
            if placement.epoch == 0 {
                return Err(StoreError::InvalidPlan(format!(
                    "replica {} has epoch 0",
                    placement.replica
                )));
            }
            let expected_prior = placement.epoch - 1;
            let actual_epoch = self.max_epoch(&plan.deployment_id, placement.replica);
            if actual_epoch != expected_prior {
                return Err(StoreError::StalePlan {
                    deployment_id: plan.deployment_id.clone(),
                    replica: placement.replica,
                    expected_epoch: expected_prior,
                    actual_epoch,
                });
            }
        }

        for superseded in &plan.superseded {
            if superseded.reason == SupersededReason::Duplicate {
                continue;
            }
            let actual_epoch = self.max_epoch(&plan.deployment_id, superseded.allocation.replica);
            if actual_epoch != superseded.allocation.epoch {
                return Err(StoreError::StalePlan {
                    deployment_id: plan.deployment_id.clone(),
                    replica: superseded.allocation.replica,
                    expected_epoch: superseded.allocation.epoch,
                    actual_epoch,
                });
            }
        }

        Ok(())
    }

    fn max_epoch(&self, deployment_id: &str, replica: u32) -> u64 {
        self.allocations
            .iter()
            .filter(|allocation| {
                allocation.deployment_id == deployment_id && allocation.replica == replica
            })
            .map(|allocation| allocation.epoch)
            .max()
            .unwrap_or(0)
    }

    fn pending_commands(&self) -> Vec<AllocationCommand> {
        self.commands
            .values()
            .filter(|command| command.state == AllocationCommandState::Pending)
            .cloned()
            .collect()
    }

    fn acknowledge_command(&mut self, command_id: &str) -> Result<(), StoreError> {
        let Some(command) = self.commands.get_mut(command_id) else {
            return Err(StoreError::CommandNotFound(command_id.to_owned()));
        };
        command.state = AllocationCommandState::Acknowledged;
        Ok(())
    }
}

fn validate_plan_identity(evaluation: &Evaluation, plan: &PlacementPlan) -> Result<(), StoreError> {
    if plan.evaluation_id != evaluation.evaluation_id
        || plan.deployment_id != evaluation.deployment_id
        || plan.revision != evaluation.revision
    {
        return Err(StoreError::InvalidPlan(
            "evaluation id, deployment id, and revision must match".into(),
        ));
    }
    Ok(())
}

fn same_allocation_identity(left: &ObservedAllocation, right: &ObservedAllocation) -> bool {
    left.deployment_id == right.deployment_id
        && left.revision == right.revision
        && left.replica == right.replica
        && left.node_id == right.node_id
        && left.epoch == right.epoch
}

fn start_command(evaluation: &Evaluation, allocation: &ObservedAllocation) -> AllocationCommand {
    AllocationCommand {
        command_id: format!(
            "{}:start:{}:{}",
            evaluation.evaluation_id, allocation.replica, allocation.epoch
        ),
        evaluation_id: evaluation.evaluation_id.clone(),
        deployment_id: allocation.deployment_id.clone(),
        revision: allocation.revision,
        replica: allocation.replica,
        node_id: allocation.node_id,
        epoch: allocation.epoch,
        kind: AllocationCommandKind::Start,
        state: AllocationCommandState::Pending,
    }
}

fn stop_command(evaluation: &Evaluation, allocation: &ObservedAllocation) -> AllocationCommand {
    AllocationCommand {
        command_id: format!(
            "{}:stop:{}:{}:{}",
            evaluation.evaluation_id, allocation.replica, allocation.epoch, allocation.node_id
        ),
        evaluation_id: evaluation.evaluation_id.clone(),
        deployment_id: allocation.deployment_id.clone(),
        revision: allocation.revision,
        replica: allocation.replica,
        node_id: allocation.node_id,
        epoch: allocation.epoch,
        kind: AllocationCommandKind::Stop,
        state: AllocationCommandState::Pending,
    }
}

#[derive(Debug, Default)]
pub struct MemoryControlStore {
    state: Mutex<PersistedState>,
}

impl MemoryControlStore {
    pub fn seed_allocation(&self, allocation: ObservedAllocation) {
        self.state
            .lock()
            .expect("control-store mutex poisoned")
            .allocations
            .push(allocation);
    }
}

impl ControlStore for MemoryControlStore {
    fn record_evaluation(&self, evaluation: &Evaluation) -> Result<(), StoreError> {
        self.state
            .lock()
            .expect("control-store mutex poisoned")
            .record_evaluation(evaluation)
    }

    fn evaluation(&self, evaluation_id: &str) -> Result<Option<EvaluationRecord>, StoreError> {
        Ok(self
            .state
            .lock()
            .expect("control-store mutex poisoned")
            .evaluations
            .get(evaluation_id)
            .cloned())
    }

    fn committed_plan(&self, evaluation_id: &str) -> Result<Option<PlacementPlan>, StoreError> {
        Ok(self
            .state
            .lock()
            .expect("control-store mutex poisoned")
            .plans
            .get(evaluation_id)
            .cloned())
    }

    fn allocations_for(&self, deployment_id: &str) -> Result<Vec<ObservedAllocation>, StoreError> {
        Ok(self
            .state
            .lock()
            .expect("control-store mutex poisoned")
            .allocations
            .iter()
            .filter(|allocation| allocation.deployment_id == deployment_id)
            .cloned()
            .collect())
    }

    fn commit_plan(
        &self,
        evaluation: &Evaluation,
        plan: &PlacementPlan,
    ) -> Result<CommitOutcome, StoreError> {
        self.state
            .lock()
            .expect("control-store mutex poisoned")
            .commit_plan(evaluation, plan)
    }

    fn pending_commands(&self) -> Result<Vec<AllocationCommand>, StoreError> {
        Ok(self
            .state
            .lock()
            .expect("control-store mutex poisoned")
            .pending_commands())
    }

    fn acknowledge_command(&self, command_id: &str) -> Result<(), StoreError> {
        self.state
            .lock()
            .expect("control-store mutex poisoned")
            .acknowledge_command(command_id)
    }
}

/// Single-process durable control-store backend for development, local Cloud
/// controllers, and deterministic recovery tests.
///
/// Every mutation serializes a complete next state to a sibling temporary file,
/// fsyncs it, atomically renames it over the prior state, then fsyncs the parent
/// directory on Unix. Production multi-controller deployments should implement
/// ControlStore with a transactional database and compare-and-set semantics.
#[derive(Debug)]
pub struct JsonFileControlStore {
    path: PathBuf,
    state: Mutex<PersistedState>,
}

impl JsonFileControlStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        let state = if path.exists() {
            let bytes = fs::read(&path).map_err(|error| StoreError::Io(error.to_string()))?;
            serde_json::from_slice(&bytes)
                .map_err(|error| StoreError::Serialization(error.to_string()))?
        } else {
            PersistedState::default()
        };

        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    fn mutate<T>(
        &self,
        mutate: impl FnOnce(&mut PersistedState) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut guard = self.state.lock().expect("control-store mutex poisoned");
        let mut next = guard.clone();
        let output = mutate(&mut next)?;
        persist_state(&self.path, &next)?;
        *guard = next;
        Ok(output)
    }
}

impl ControlStore for JsonFileControlStore {
    fn record_evaluation(&self, evaluation: &Evaluation) -> Result<(), StoreError> {
        self.mutate(|state| state.record_evaluation(evaluation))
    }

    fn evaluation(&self, evaluation_id: &str) -> Result<Option<EvaluationRecord>, StoreError> {
        Ok(self
            .state
            .lock()
            .expect("control-store mutex poisoned")
            .evaluations
            .get(evaluation_id)
            .cloned())
    }

    fn committed_plan(&self, evaluation_id: &str) -> Result<Option<PlacementPlan>, StoreError> {
        Ok(self
            .state
            .lock()
            .expect("control-store mutex poisoned")
            .plans
            .get(evaluation_id)
            .cloned())
    }

    fn allocations_for(&self, deployment_id: &str) -> Result<Vec<ObservedAllocation>, StoreError> {
        Ok(self
            .state
            .lock()
            .expect("control-store mutex poisoned")
            .allocations
            .iter()
            .filter(|allocation| allocation.deployment_id == deployment_id)
            .cloned()
            .collect())
    }

    fn commit_plan(
        &self,
        evaluation: &Evaluation,
        plan: &PlacementPlan,
    ) -> Result<CommitOutcome, StoreError> {
        self.mutate(|state| state.commit_plan(evaluation, plan))
    }

    fn pending_commands(&self) -> Result<Vec<AllocationCommand>, StoreError> {
        Ok(self
            .state
            .lock()
            .expect("control-store mutex poisoned")
            .pending_commands())
    }

    fn acknowledge_command(&self, command_id: &str) -> Result<(), StoreError> {
        self.mutate(|state| state.acknowledge_command(command_id))
    }
}

fn persist_state(path: &Path, state: &PersistedState) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
        }
    }

    let bytes =
        serde_json::to_vec(state).map_err(|error| StoreError::Serialization(error.to_string()))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));

    {
        let mut file =
            File::create(&temporary).map_err(|error| StoreError::Io(error.to_string()))?;
        file.write_all(&bytes)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.sync_all()
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }

    fs::rename(&temporary, path).map_err(|error| StoreError::Io(error.to_string()))?;

    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        DeploymentSpec, EvaluationCause, NodeDescriptor, NodeResources, NodeState,
        PlacementConstraints, PlacementPreferences, ResourceRequest,
    };
    use crate::scheduler::plan_evaluation;
    use nulang_capacity::{Architecture, TrustTier};
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn deployment() -> DeploymentSpec {
        DeploymentSpec {
            deployment_id: "api".into(),
            revision: 2,
            replicas: 1,
            resources: ResourceRequest {
                cpu_millis: 500,
                memory_mib: 256,
                accelerator: None,
            },
            constraints: PlacementConstraints::default(),
            preferences: PlacementPreferences::default(),
        }
    }

    fn evaluation(id: &str) -> Evaluation {
        Evaluation {
            evaluation_id: id.into(),
            deployment_id: "api".into(),
            revision: 2,
            cause: EvaluationCause::DeploymentChanged,
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
                cpu_millis_total: 4_000,
                cpu_millis_available: 4_000,
                memory_mib_total: 4_096,
                memory_mib_available: 4_096,
                accelerators: BTreeMap::new(),
            },
        }
    }

    #[test]
    fn test_exact_commit_retry_is_idempotent_and_does_not_duplicate_commands() {
        let store = MemoryControlStore::default();
        let eval = evaluation("eval-1");
        store.record_evaluation(&eval).unwrap();
        let plan = plan_evaluation(&eval, &deployment(), &[node(1)], &[]).unwrap();

        assert_eq!(
            store.commit_plan(&eval, &plan).unwrap(),
            CommitOutcome::Applied
        );
        assert_eq!(
            store.commit_plan(&eval, &plan).unwrap(),
            CommitOutcome::AlreadyCommitted
        );

        assert_eq!(store.allocations_for("api").unwrap().len(), 1);
        assert_eq!(store.pending_commands().unwrap().len(), 1);
    }

    #[test]
    fn test_stale_plan_cannot_commit_after_another_epoch_wins() {
        let store = MemoryControlStore::default();
        let spec = deployment();
        let first = evaluation("eval-first");
        let second = evaluation("eval-second");
        store.record_evaluation(&first).unwrap();
        store.record_evaluation(&second).unwrap();

        let stale_plan = plan_evaluation(&second, &spec, &[node(2)], &[]).unwrap();
        let winning_plan = plan_evaluation(&first, &spec, &[node(1)], &[]).unwrap();
        store.commit_plan(&first, &winning_plan).unwrap();

        assert_eq!(
            store.commit_plan(&second, &stale_plan),
            Err(StoreError::StalePlan {
                deployment_id: "api".into(),
                replica: 0,
                expected_epoch: 0,
                actual_epoch: 1,
            })
        );
        assert_eq!(store.allocations_for("api").unwrap().len(), 1);
    }

    #[test]
    fn test_replacement_atomically_stops_old_epoch_and_enqueues_start_stop() {
        let store = MemoryControlStore::default();
        let old = ObservedAllocation {
            deployment_id: "api".into(),
            revision: 1,
            replica: 0,
            node_id: 1,
            epoch: 7,
            state: AllocationState::Running,
        };
        store.seed_allocation(old.clone());

        let eval = evaluation("eval-replace");
        store.record_evaluation(&eval).unwrap();
        let plan = plan_evaluation(&eval, &deployment(), &[node(1), node(2)], &[old]).unwrap();
        store.commit_plan(&eval, &plan).unwrap();

        let allocations = store.allocations_for("api").unwrap();
        assert!(allocations.iter().any(
            |allocation| allocation.epoch == 7 && allocation.state == AllocationState::Stopped
        ));
        assert!(allocations.iter().any(|allocation| {
            allocation.epoch == 8
                && allocation.state == AllocationState::Starting
                && allocation.node_id == plan.placements[0].node_id
        }));

        let commands = store.pending_commands().unwrap();
        assert_eq!(commands.len(), 2);
        assert!(commands
            .iter()
            .any(|command| command.kind == AllocationCommandKind::Stop && command.epoch == 7));
        assert!(commands
            .iter()
            .any(|command| command.kind == AllocationCommandKind::Start && command.epoch == 8));
    }

    #[test]
    fn test_command_ack_is_idempotent() {
        let store = MemoryControlStore::default();
        let eval = evaluation("eval-ack");
        store.record_evaluation(&eval).unwrap();
        let plan = plan_evaluation(&eval, &deployment(), &[node(1)], &[]).unwrap();
        store.commit_plan(&eval, &plan).unwrap();

        let command = store.pending_commands().unwrap().pop().unwrap();
        store.acknowledge_command(&command.command_id).unwrap();
        store.acknowledge_command(&command.command_id).unwrap();
        assert!(store.pending_commands().unwrap().is_empty());
    }

    #[test]
    fn test_json_store_recovers_committed_plan_allocations_and_outbox() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nulang-cloud-control-{}-{unique}.json",
            std::process::id()
        ));

        let eval = evaluation("eval-durable");
        let plan = plan_evaluation(&eval, &deployment(), &[node(9)], &[]).unwrap();

        {
            let store = JsonFileControlStore::open(&path).unwrap();
            store.record_evaluation(&eval).unwrap();
            store.commit_plan(&eval, &plan).unwrap();
        }

        let reopened = JsonFileControlStore::open(&path).unwrap();
        assert_eq!(reopened.committed_plan("eval-durable").unwrap(), Some(plan));
        assert_eq!(reopened.allocations_for("api").unwrap().len(), 1);
        assert_eq!(reopened.pending_commands().unwrap().len(), 1);
        assert_eq!(
            reopened.evaluation("eval-durable").unwrap().unwrap().status,
            EvaluationStatus::Committed
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_evaluation_id_cannot_be_reused_for_different_revision() {
        let store = MemoryControlStore::default();
        let original = evaluation("same-id");
        store.record_evaluation(&original).unwrap();

        let mut changed = original.clone();
        changed.revision = 3;
        assert_eq!(
            store.record_evaluation(&changed),
            Err(StoreError::EvaluationConflict("same-id".into()))
        );
    }
}
