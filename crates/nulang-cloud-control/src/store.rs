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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationCommandClaim {
    pub command: AllocationCommand,
    pub owner: String,
    pub generation: u64,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AllocationCommandClaimRecord {
    owner: String,
    generation: u64,
    expires_at_unix_ms: u64,
    active: bool,
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
    Backend(String),
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
    CommandClaim(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "control-store I/O error: {message}"),
            Self::Serialization(message) => {
                write!(f, "control-store serialization error: {message}")
            }
            Self::Backend(message) => write!(f, "control-store backend error: {message}"),
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
            Self::CommandClaim(message) => {
                write!(f, "allocation command claim error: {message}")
            }
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

    /// True only when a Start command still names the highest authoritative
    /// epoch for its logical deployment replica.
    ///
    /// Keep this derivation storage-neutral: every backend already exposes the
    /// authoritative allocation snapshot through `allocations_for`, so the
    /// dispatcher does not need backend-specific fencing logic.
    fn start_command_is_authoritative(
        &self,
        command: &AllocationCommand,
    ) -> Result<bool, StoreError> {
        if command.kind != AllocationCommandKind::Start {
            return Ok(false);
        }

        let allocations = self.allocations_for(&command.deployment_id)?;
        let max_epoch = allocations
            .iter()
            .filter(|allocation| {
                allocation.deployment_id == command.deployment_id
                    && allocation.replica == command.replica
            })
            .map(|allocation| allocation.epoch)
            .max()
            .unwrap_or(0);

        Ok(max_epoch == command.epoch
            && allocations.iter().any(|allocation| {
                allocation.deployment_id == command.deployment_id
                    && allocation.revision == command.revision
                    && allocation.replica == command.replica
                    && allocation.node_id == command.node_id
                    && allocation.epoch == command.epoch
                    && allocation.state.is_active()
            }))
    }

    /// Atomically claim the next safe command for one dispatcher.
    ///
    /// An unresolved Stop for a logical replica blocks claiming a Start for
    /// that same replica, even when another dispatcher currently owns the Stop
    /// lease. Reclaims increment a durable generation so a stale lease holder
    /// cannot ACK after losing ownership.
    fn claim_next_command(
        &self,
        owner: &str,
        now_unix_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<AllocationCommandClaim>, StoreError>;

    /// ACK only when the durable claim generation still belongs to this
    /// dispatcher. An already-acknowledged command is idempotently successful.
    fn acknowledge_claimed_command(&self, claim: &AllocationCommandClaim)
        -> Result<(), StoreError>;

    /// Release a matching active claim after a delivery failure. A stale owner
    /// cannot release a newer dispatcher's claim.
    fn release_command_claim(&self, claim: &AllocationCommandClaim) -> Result<(), StoreError>;

    /// Legacy single-dispatcher acknowledgement path.
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
    #[serde(default)]
    command_claims: BTreeMap<String, AllocationCommandClaimRecord>,
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

    fn claim_next_command(
        &mut self,
        owner: &str,
        now_unix_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<AllocationCommandClaim>, StoreError> {
        if owner.trim().is_empty() {
            return Err(StoreError::CommandClaim(
                "dispatcher owner must not be empty".into(),
            ));
        }
        if lease_ms == 0 {
            return Err(StoreError::CommandClaim(
                "dispatcher lease must be greater than zero".into(),
            ));
        }
        let expires_at_unix_ms = now_unix_ms
            .checked_add(lease_ms)
            .ok_or_else(|| StoreError::CommandClaim("dispatcher lease expiry overflow".into()))?;

        let mut candidates = self.pending_commands();
        candidates.sort_by(|left, right| {
            command_priority(left.kind)
                .cmp(&command_priority(right.kind))
                .then_with(|| left.deployment_id.cmp(&right.deployment_id))
                .then_with(|| left.replica.cmp(&right.replica))
                .then_with(|| left.epoch.cmp(&right.epoch))
                .then_with(|| left.command_id.cmp(&right.command_id))
        });

        for command in candidates {
            if command.kind == AllocationCommandKind::Start
                && self.commands.values().any(|candidate| {
                    candidate.state == AllocationCommandState::Pending
                        && candidate.kind == AllocationCommandKind::Stop
                        && candidate.deployment_id == command.deployment_id
                        && candidate.replica == command.replica
                })
            {
                continue;
            }

            let claim_available = self
                .command_claims
                .get(&command.command_id)
                .map(|claim| !claim.active || claim.expires_at_unix_ms <= now_unix_ms)
                .unwrap_or(true);
            if !claim_available {
                continue;
            }

            let record = self
                .command_claims
                .entry(command.command_id.clone())
                .or_default();
            record.generation = record.generation.checked_add(1).ok_or_else(|| {
                StoreError::CommandClaim(format!(
                    "claim generation overflow for {}",
                    command.command_id
                ))
            })?;
            record.owner = owner.to_owned();
            record.expires_at_unix_ms = expires_at_unix_ms;
            record.active = true;

            return Ok(Some(AllocationCommandClaim {
                command,
                owner: record.owner.clone(),
                generation: record.generation,
                expires_at_unix_ms,
            }));
        }

        Ok(None)
    }

    fn acknowledge_claimed_command(
        &mut self,
        claim: &AllocationCommandClaim,
    ) -> Result<(), StoreError> {
        let Some(command) = self.commands.get(&claim.command.command_id) else {
            return Err(StoreError::CommandNotFound(
                claim.command.command_id.clone(),
            ));
        };
        if command.state == AllocationCommandState::Acknowledged {
            return Ok(());
        }

        let Some(current) = self.command_claims.get_mut(&claim.command.command_id) else {
            return Err(StoreError::CommandClaim(format!(
                "{} has no durable claim",
                claim.command.command_id
            )));
        };
        if !current.active || current.owner != claim.owner || current.generation != claim.generation
        {
            return Err(StoreError::CommandClaim(format!(
                "{} claim {} owned by {} is stale; current claim is generation {} owned by {}",
                claim.command.command_id,
                claim.generation,
                claim.owner,
                current.generation,
                current.owner
            )));
        }

        self.commands
            .get_mut(&claim.command.command_id)
            .expect("command existence checked above")
            .state = AllocationCommandState::Acknowledged;
        current.active = false;
        Ok(())
    }

    fn release_command_claim(&mut self, claim: &AllocationCommandClaim) -> Result<(), StoreError> {
        let Some(command) = self.commands.get(&claim.command.command_id) else {
            return Err(StoreError::CommandNotFound(
                claim.command.command_id.clone(),
            ));
        };
        if command.state == AllocationCommandState::Acknowledged {
            return Ok(());
        }

        let Some(current) = self.command_claims.get_mut(&claim.command.command_id) else {
            return Err(StoreError::CommandClaim(format!(
                "{} has no durable claim",
                claim.command.command_id
            )));
        };
        if !current.active || current.owner != claim.owner || current.generation != claim.generation
        {
            return Err(StoreError::CommandClaim(format!(
                "{} claim {} owned by {} cannot release current generation {} owned by {}",
                claim.command.command_id,
                claim.generation,
                claim.owner,
                current.generation,
                current.owner
            )));
        }

        current.active = false;
        Ok(())
    }

    fn acknowledge_command(&mut self, command_id: &str) -> Result<(), StoreError> {
        let Some(command) = self.commands.get_mut(command_id) else {
            return Err(StoreError::CommandNotFound(command_id.to_owned()));
        };
        command.state = AllocationCommandState::Acknowledged;
        if let Some(claim) = self.command_claims.get_mut(command_id) {
            claim.active = false;
        }
        Ok(())
    }
}

fn command_priority(kind: AllocationCommandKind) -> u8 {
    match kind {
        AllocationCommandKind::Stop => 0,
        AllocationCommandKind::Start => 1,
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

    fn claim_next_command(
        &self,
        owner: &str,
        now_unix_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<AllocationCommandClaim>, StoreError> {
        self.state
            .lock()
            .expect("control-store mutex poisoned")
            .claim_next_command(owner, now_unix_ms, lease_ms)
    }

    fn acknowledge_claimed_command(
        &self,
        claim: &AllocationCommandClaim,
    ) -> Result<(), StoreError> {
        self.state
            .lock()
            .expect("control-store mutex poisoned")
            .acknowledge_claimed_command(claim)
    }

    fn release_command_claim(&self, claim: &AllocationCommandClaim) -> Result<(), StoreError> {
        self.state
            .lock()
            .expect("control-store mutex poisoned")
            .release_command_claim(claim)
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

    fn claim_next_command(
        &self,
        owner: &str,
        now_unix_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<AllocationCommandClaim>, StoreError> {
        self.mutate(|state| state.claim_next_command(owner, now_unix_ms, lease_ms))
    }

    fn acknowledge_claimed_command(
        &self,
        claim: &AllocationCommandClaim,
    ) -> Result<(), StoreError> {
        self.mutate(|state| state.acknowledge_claimed_command(claim))
    }

    fn release_command_claim(&self, claim: &AllocationCommandClaim) -> Result<(), StoreError> {
        self.mutate(|state| state.release_command_claim(claim))
    }

    fn acknowledge_command(&self, command_id: &str) -> Result<(), StoreError> {
        self.mutate(|state| state.acknowledge_command(command_id))
    }
}

#[cfg(feature = "postgres")]
pub struct PostgresControlStore {
    conn: Mutex<postgres::Client>,
    scope: String,
}

#[cfg(feature = "postgres")]
impl PostgresControlStore {
    /// Connect without TLS. This is intended for local development or a
    /// network path whose encryption is provided externally. Production code
    /// that requires PostgreSQL TLS should construct a connected
    /// postgres::Client with the desired TLS connector and call from_client.
    pub fn connect_no_tls(config: &str, scope: impl Into<String>) -> Result<Self, StoreError> {
        let client = postgres::Client::connect(config, postgres::NoTls)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Self::from_client(client, scope)
    }

    /// Build a store from an already-connected PostgreSQL client.
    ///
    /// Scope is the serialization boundary for control-plane state. A region
    /// or scheduling cell should normally use its own scope so independent
    /// cells do not contend on one row.
    pub fn from_client(
        client: postgres::Client,
        scope: impl Into<String>,
    ) -> Result<Self, StoreError> {
        let scope = scope.into();
        if scope.trim().is_empty() {
            return Err(StoreError::Backend(
                "PostgreSQL control-store scope must not be empty".into(),
            ));
        }

        let store = Self {
            conn: Mutex::new(client),
            scope,
        };
        store.ensure_schema()?;
        Ok(store)
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Monotonic state-row version, useful for controller diagnostics.
    pub fn version(&self) -> Result<i64, StoreError> {
        let mut conn = self.conn.lock().expect("control-store mutex poisoned");
        let row = conn
            .query_one(
                "SELECT version FROM nulang_cloud_control_state WHERE scope = $1",
                &[&self.scope],
            )
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(row.get(0))
    }

    fn ensure_schema(&self) -> Result<(), StoreError> {
        let default_json = serde_json::to_string(&PersistedState::default())
            .map_err(|error| StoreError::Serialization(error.to_string()))?;
        let mut conn = self.conn.lock().expect("control-store mutex poisoned");

        conn.batch_execute(
            "CREATE TABLE IF NOT EXISTS nulang_cloud_control_state (
                scope TEXT PRIMARY KEY,
                version BIGINT NOT NULL,
                state_json TEXT NOT NULL
            )",
        )
        .map_err(|error| StoreError::Backend(error.to_string()))?;

        conn.execute(
            "INSERT INTO nulang_cloud_control_state (scope, version, state_json)
             VALUES ($1, 0, $2)
             ON CONFLICT (scope) DO NOTHING",
            &[&self.scope, &default_json],
        )
        .map_err(|error| StoreError::Backend(error.to_string()))?;

        Ok(())
    }

    fn load_state(&self) -> Result<PersistedState, StoreError> {
        let mut conn = self.conn.lock().expect("control-store mutex poisoned");
        let row = conn
            .query_one(
                "SELECT state_json FROM nulang_cloud_control_state WHERE scope = $1",
                &[&self.scope],
            )
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let encoded: String = row.get(0);
        serde_json::from_str(&encoded).map_err(|error| StoreError::Serialization(error.to_string()))
    }

    fn mutate<T>(
        &self,
        mutate: impl FnOnce(&mut PersistedState) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut conn = self.conn.lock().expect("control-store mutex poisoned");
        let mut transaction = conn
            .transaction()
            .map_err(|error| StoreError::Backend(error.to_string()))?;

        // The row lock is the compare-and-set serialization point across
        // controller processes. Validation and the resulting state write occur
        // in this same database transaction.
        let row = transaction
            .query_one(
                "SELECT state_json FROM nulang_cloud_control_state
                 WHERE scope = $1
                 FOR UPDATE",
                &[&self.scope],
            )
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let encoded: String = row.get(0);
        let mut state: PersistedState = serde_json::from_str(&encoded)
            .map_err(|error| StoreError::Serialization(error.to_string()))?;

        let output = mutate(&mut state)?;
        let next_json = serde_json::to_string(&state)
            .map_err(|error| StoreError::Serialization(error.to_string()))?;

        let updated = transaction
            .execute(
                "UPDATE nulang_cloud_control_state
                 SET version = version + 1, state_json = $2
                 WHERE scope = $1",
                &[&self.scope, &next_json],
            )
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        if updated != 1 {
            return Err(StoreError::Backend(format!(
                "expected to update one control-state row for scope {}, updated {updated}",
                self.scope
            )));
        }

        transaction
            .commit()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(output)
    }
}

#[cfg(feature = "postgres")]
impl ControlStore for PostgresControlStore {
    fn record_evaluation(&self, evaluation: &Evaluation) -> Result<(), StoreError> {
        self.mutate(|state| state.record_evaluation(evaluation))
    }

    fn evaluation(&self, evaluation_id: &str) -> Result<Option<EvaluationRecord>, StoreError> {
        Ok(self.load_state()?.evaluations.get(evaluation_id).cloned())
    }

    fn committed_plan(&self, evaluation_id: &str) -> Result<Option<PlacementPlan>, StoreError> {
        Ok(self.load_state()?.plans.get(evaluation_id).cloned())
    }

    fn allocations_for(&self, deployment_id: &str) -> Result<Vec<ObservedAllocation>, StoreError> {
        Ok(self
            .load_state()?
            .allocations
            .into_iter()
            .filter(|allocation| allocation.deployment_id == deployment_id)
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
        Ok(self.load_state()?.pending_commands())
    }

    fn claim_next_command(
        &self,
        owner: &str,
        now_unix_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<AllocationCommandClaim>, StoreError> {
        self.mutate(|state| state.claim_next_command(owner, now_unix_ms, lease_ms))
    }

    fn acknowledge_claimed_command(
        &self,
        claim: &AllocationCommandClaim,
    ) -> Result<(), StoreError> {
        self.mutate(|state| state.acknowledge_claimed_command(claim))
    }

    fn release_command_claim(&self, claim: &AllocationCommandClaim) -> Result<(), StoreError> {
        self.mutate(|state| state.release_command_claim(claim))
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
    fn test_claim_generation_fences_stale_dispatcher_and_stop_blocks_start() {
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

        let eval = evaluation("eval-claims");
        store.record_evaluation(&eval).unwrap();
        let plan = plan_evaluation(&eval, &deployment(), &[node(1), node(2)], &[old]).unwrap();
        store.commit_plan(&eval, &plan).unwrap();

        let first = store
            .claim_next_command("dispatcher-a", 100, 10)
            .unwrap()
            .expect("first dispatcher should claim Stop");
        assert_eq!(first.command.kind, AllocationCommandKind::Stop);
        assert_eq!(first.generation, 1);

        assert!(
            store
                .claim_next_command("dispatcher-b", 105, 10)
                .unwrap()
                .is_none(),
            "Start must remain blocked while the Stop is unresolved"
        );

        let replacement = store
            .claim_next_command("dispatcher-b", 111, 10)
            .unwrap()
            .expect("expired Stop claim should be reclaimable");
        assert_eq!(replacement.command.command_id, first.command.command_id);
        assert_eq!(replacement.generation, 2);
        assert!(matches!(
            store.acknowledge_claimed_command(&first),
            Err(StoreError::CommandClaim(_))
        ));

        store.acknowledge_claimed_command(&replacement).unwrap();
        let start = store
            .claim_next_command("dispatcher-b", 112, 10)
            .unwrap()
            .expect("Start should become claimable after Stop ACK");
        assert_eq!(start.command.kind, AllocationCommandKind::Start);
    }

    #[test]
    fn test_json_store_persists_command_claim_generation() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "nulang-cloud-claims-{}-{unique}.json",
            std::process::id()
        ));

        let first_generation;
        {
            let store = JsonFileControlStore::open(&path).unwrap();
            let eval = evaluation("eval-claim-durable");
            store.record_evaluation(&eval).unwrap();
            let plan = plan_evaluation(&eval, &deployment(), &[node(9)], &[]).unwrap();
            store.commit_plan(&eval, &plan).unwrap();
            let claim = store
                .claim_next_command("dispatcher-a", 100, 10)
                .unwrap()
                .unwrap();
            first_generation = claim.generation;
        }

        let reopened = JsonFileControlStore::open(&path).unwrap();
        assert!(
            reopened
                .claim_next_command("dispatcher-b", 105, 10)
                .unwrap()
                .is_none(),
            "unexpired claim must survive controller restart"
        );
        let reclaimed = reopened
            .claim_next_command("dispatcher-b", 111, 10)
            .unwrap()
            .expect("expired durable claim must be reclaimable");
        assert_eq!(reclaimed.generation, first_generation + 1);

        let _ = fs::remove_file(path);
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

    #[cfg(feature = "postgres")]
    #[test]
    fn test_postgres_store_round_trip_when_configured() {
        let Ok(url) = std::env::var("NULANG_TEST_POSTGRES_URL") else {
            return;
        };
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let scope = format!("cloud-control-test-{}-{unique}", std::process::id());
        let store = PostgresControlStore::connect_no_tls(&url, scope.clone()).unwrap();

        let eval = evaluation("eval-postgres");
        let plan = plan_evaluation(&eval, &deployment(), &[node(7)], &[]).unwrap();
        store.record_evaluation(&eval).unwrap();
        assert_eq!(
            store.commit_plan(&eval, &plan).unwrap(),
            CommitOutcome::Applied
        );
        assert_eq!(store.committed_plan("eval-postgres").unwrap(), Some(plan));
        assert_eq!(store.allocations_for("api").unwrap().len(), 1);
        assert_eq!(store.pending_commands().unwrap().len(), 1);
        assert!(store.version().unwrap() >= 2);

        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM nulang_cloud_control_state WHERE scope = $1",
                &[&scope],
            )
            .unwrap();
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
