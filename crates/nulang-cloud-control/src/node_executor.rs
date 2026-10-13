use crate::executor::{AllocationCommandSink, CommandApplyError};
use crate::store::{AllocationCommand, AllocationCommandKind, ControlStore};
use crate::workload_revision::WorkloadRevisionSpec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeAllocationPhase {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

impl NodeAllocationPhase {
    fn blocks_new_epoch(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAllocationRecord {
    pub deployment_id: String,
    pub revision: u64,
    pub replica: u32,
    pub node_id: u64,
    pub epoch: u64,
    pub phase: NodeAllocationPhase,
}

/// Side-effect boundary implemented by the actual node runtime/container launcher.
///
/// Implementations MUST be idempotent for the exact allocation identity in the
/// command. Stop(epoch=N) must never terminate epoch N+1 of the same logical
/// replica. The durable executor can replay either operation after a crash
/// between the external side effect and its local completion record.
pub trait WorkloadLifecycle: Send + Sync {
    fn start(&self, command: &AllocationCommand) -> Result<(), CommandApplyError>;
    fn stop(&self, command: &AllocationCommand) -> Result<(), CommandApplyError>;
}

/// Resolver for immutable deployment revision identity.
///
/// Production implementations may read a local admission cache or remote
/// control-plane API. The key invariant is that one deployment revision cannot
/// resolve to different identities over time.
pub trait WorkloadRevisionResolver: Send + Sync {
    fn resolve(
        &self,
        deployment_id: &str,
        revision: u64,
    ) -> Result<Option<WorkloadRevisionSpec>, CommandApplyError>;
}

/// Direct resolver over a ControlStore. Useful for single-node deployments,
/// tests, and controller/node-agent co-location.
#[derive(Clone, Copy)]
pub struct ControlStoreWorkloadResolver<'a> {
    store: &'a dyn ControlStore,
}

impl<'a> ControlStoreWorkloadResolver<'a> {
    pub fn new(store: &'a dyn ControlStore) -> Self {
        Self { store }
    }
}

impl WorkloadRevisionResolver for ControlStoreWorkloadResolver<'_> {
    fn resolve(
        &self,
        deployment_id: &str,
        revision: u64,
    ) -> Result<Option<WorkloadRevisionSpec>, CommandApplyError> {
        self.store
            .workload_revision(deployment_id, revision)
            .map_err(|error| {
                CommandApplyError::retryable(format!(
                    "workload revision lookup failed for {deployment_id} revision {revision}: {error}"
                ))
            })
    }
}

/// Launch boundary that has received the immutable workload revision contract.
pub trait ResolvedWorkloadLifecycle: Send + Sync {
    fn start_resolved(
        &self,
        command: &AllocationCommand,
        workload: &WorkloadRevisionSpec,
    ) -> Result<(), CommandApplyError>;

    fn stop(&self, command: &AllocationCommand) -> Result<(), CommandApplyError>;
}

/// Adapter that prevents Start from reaching a launcher unless the deployment
/// revision resolves to an immutable admitted workload identity.
///
/// Stop deliberately does not depend on revision lookup: cleanup/fencing must
/// remain available during registry or artifact-store outages.
pub struct RevisionBoundLifecycle<R, L> {
    resolver: R,
    lifecycle: L,
}

impl<R, L> RevisionBoundLifecycle<R, L> {
    pub fn new(resolver: R, lifecycle: L) -> Self {
        Self {
            resolver,
            lifecycle,
        }
    }

    pub fn into_inner(self) -> (R, L) {
        (self.resolver, self.lifecycle)
    }
}

impl<R, L> WorkloadLifecycle for RevisionBoundLifecycle<R, L>
where
    R: WorkloadRevisionResolver,
    L: ResolvedWorkloadLifecycle,
{
    fn start(&self, command: &AllocationCommand) -> Result<(), CommandApplyError> {
        let Some(workload) = self
            .resolver
            .resolve(&command.deployment_id, command.revision)?
        else {
            return Err(CommandApplyError::terminal(format!(
                "no immutable workload revision registered for {} revision {}",
                command.deployment_id, command.revision
            )));
        };

        workload.validate().map_err(|error| {
            CommandApplyError::terminal(format!(
                "registered workload revision for {} revision {} is invalid: {error}",
                command.deployment_id, command.revision
            ))
        })?;
        if workload.deployment_id != command.deployment_id || workload.revision != command.revision
        {
            return Err(CommandApplyError::terminal(format!(
                "resolved workload identity does not match allocation command {} revision {}",
                command.deployment_id, command.revision
            )));
        }

        self.lifecycle.start_resolved(command, &workload)
    }

    fn stop(&self, command: &AllocationCommand) -> Result<(), CommandApplyError> {
        self.lifecycle.stop(command)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedNodeExecutionState {
    #[serde(default)]
    allocations: BTreeMap<String, NodeAllocationRecord>,
}

/// Node-side implementation of the controller allocation-command sink.
///
/// The local state file is not the source of cluster ownership; the control
/// plane is. Its purpose is a second durable fencing boundary at the node where
/// side effects occur. Starting/Stopping are persisted before side effects,
/// crash recovery replays the exact operation idempotently, stopped epochs
/// cannot be resurrected, and newer epochs wait for prior local ownership to
/// become terminal.
pub struct FencedNodeExecutor<L: WorkloadLifecycle> {
    node_id: u64,
    path: PathBuf,
    state: Mutex<PersistedNodeExecutionState>,
    lifecycle: L,
}

impl<L: WorkloadLifecycle> FencedNodeExecutor<L> {
    pub fn open(node_id: u64, path: impl Into<PathBuf>, lifecycle: L) -> std::io::Result<Self> {
        let path = path.into();
        let state = if path.exists() {
            let bytes = fs::read(&path)?;
            serde_json::from_slice(&bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
        } else {
            PersistedNodeExecutionState::default()
        };

        Ok(Self {
            node_id,
            path,
            state: Mutex::new(state),
            lifecycle,
        })
    }

    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    pub fn allocation(&self, deployment_id: &str, replica: u32) -> Option<NodeAllocationRecord> {
        self.state
            .lock()
            .ok()?
            .allocations
            .get(&allocation_key(deployment_id, replica))
            .cloned()
    }

    fn validate_node(&self, command: &AllocationCommand) -> Result<(), CommandApplyError> {
        if command.node_id != self.node_id {
            return Err(CommandApplyError::terminal(format!(
                "allocation command {} targets node {}, but executor owns node {}",
                command.command_id, command.node_id, self.node_id
            )));
        }
        Ok(())
    }

    fn persist_update(
        &self,
        guard: &mut PersistedNodeExecutionState,
        update: impl FnOnce(&mut PersistedNodeExecutionState),
    ) -> Result<(), CommandApplyError> {
        let mut next = guard.clone();
        update(&mut next);
        persist_state(&self.path, &next).map_err(|error| {
            CommandApplyError::retryable(format!(
                "node execution state persistence failed: {error}"
            ))
        })?;
        *guard = next;
        Ok(())
    }

    fn apply_start(&self, command: &AllocationCommand) -> Result<(), CommandApplyError> {
        self.validate_node(command)?;
        let key = allocation_key(&command.deployment_id, command.replica);
        let mut guard = self
            .state
            .lock()
            .map_err(|_| CommandApplyError::terminal("node execution state mutex poisoned"))?;

        let current = guard.allocations.get(&key).cloned();
        if let Some(existing) = &current {
            if existing.epoch > command.epoch {
                return Ok(());
            }
            if existing.epoch == command.epoch {
                if existing.revision != command.revision || existing.node_id != command.node_id {
                    return Err(CommandApplyError::terminal(format!(
                        "allocation epoch {} for {} replica {} was reused with conflicting identity",
                        command.epoch, command.deployment_id, command.replica
                    )));
                }
                match existing.phase {
                    NodeAllocationPhase::Running => return Ok(()),
                    NodeAllocationPhase::Stopped | NodeAllocationPhase::Stopping => {
                        return Err(CommandApplyError::terminal(format!(
                            "allocation {} replica {} epoch {} is already stopping/stopped and cannot be resurrected",
                            command.deployment_id, command.replica, command.epoch
                        )));
                    }
                    NodeAllocationPhase::Failed => {
                        return Err(CommandApplyError::terminal(format!(
                            "allocation {} replica {} epoch {} previously failed terminally",
                            command.deployment_id, command.replica, command.epoch
                        )));
                    }
                    NodeAllocationPhase::Starting => {}
                }
            } else if existing.phase.blocks_new_epoch() {
                return Err(CommandApplyError::retryable(format!(
                    "allocation {} replica {} epoch {} cannot start while prior epoch {} is {:?}",
                    command.deployment_id,
                    command.replica,
                    command.epoch,
                    existing.epoch,
                    existing.phase
                )));
            }
        }

        if current
            .as_ref()
            .is_none_or(|existing| existing.epoch < command.epoch)
        {
            let starting = record(command, NodeAllocationPhase::Starting);
            self.persist_update(&mut guard, |state| {
                state.allocations.insert(key.clone(), starting);
            })?;
        }

        match self.lifecycle.start(command) {
            Ok(()) => {
                let running = record(command, NodeAllocationPhase::Running);
                self.persist_update(&mut guard, |state| {
                    state.allocations.insert(key, running);
                })
            }
            Err(error) if error.retryable => Err(error),
            Err(error) => {
                let failed = record(command, NodeAllocationPhase::Failed);
                self.persist_update(&mut guard, |state| {
                    state.allocations.insert(key, failed);
                })?;
                Err(error)
            }
        }
    }

    fn apply_stop(&self, command: &AllocationCommand) -> Result<(), CommandApplyError> {
        self.validate_node(command)?;
        let key = allocation_key(&command.deployment_id, command.replica);
        let mut guard = self
            .state
            .lock()
            .map_err(|_| CommandApplyError::terminal("node execution state mutex poisoned"))?;

        let current = guard.allocations.get(&key).cloned();
        if let Some(existing) = &current {
            if existing.epoch > command.epoch {
                return self.lifecycle.stop(command);
            }
            if existing.epoch == command.epoch {
                if existing.revision != command.revision || existing.node_id != command.node_id {
                    return Err(CommandApplyError::terminal(format!(
                        "allocation epoch {} for {} replica {} was reused with conflicting identity",
                        command.epoch, command.deployment_id, command.replica
                    )));
                }
                if existing.phase == NodeAllocationPhase::Stopped {
                    return Ok(());
                }
            }
        }

        let stopping = record(command, NodeAllocationPhase::Stopping);
        self.persist_update(&mut guard, |state| {
            state.allocations.insert(key.clone(), stopping);
        })?;

        self.lifecycle.stop(command)?;

        let stopped = record(command, NodeAllocationPhase::Stopped);
        self.persist_update(&mut guard, |state| {
            state.allocations.insert(key, stopped);
        })
    }
}

impl<L: WorkloadLifecycle> AllocationCommandSink for FencedNodeExecutor<L> {
    fn apply(&self, command: &AllocationCommand) -> Result<(), CommandApplyError> {
        match command.kind {
            AllocationCommandKind::Start => self.apply_start(command),
            AllocationCommandKind::Stop => self.apply_stop(command),
        }
    }
}

fn record(command: &AllocationCommand, phase: NodeAllocationPhase) -> NodeAllocationRecord {
    NodeAllocationRecord {
        deployment_id: command.deployment_id.clone(),
        revision: command.revision,
        replica: command.replica,
        node_id: command.node_id,
        epoch: command.epoch,
        phase,
    }
}

fn allocation_key(deployment_id: &str, replica: u32) -> String {
    format!("{}:{}:{}", deployment_id.len(), deployment_id, replica)
}

fn persist_state(path: &Path, state: &PersistedNodeExecutionState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let bytes = serde_json::to_vec(state)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));

    {
        let mut file = File::create(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;

    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        File::open(parent)?.sync_all()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::AllocationCommandState;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Clone, Default)]
    struct RecordingResolvedLifecycle {
        starts: Arc<Mutex<Vec<(u64, String)>>>,
        stops: Arc<Mutex<Vec<u64>>>,
    }

    impl ResolvedWorkloadLifecycle for RecordingResolvedLifecycle {
        fn start_resolved(
            &self,
            command: &AllocationCommand,
            workload: &WorkloadRevisionSpec,
        ) -> Result<(), CommandApplyError> {
            self.starts
                .lock()
                .unwrap()
                .push((command.epoch, workload.digest().unwrap()));
            Ok(())
        }

        fn stop(&self, command: &AllocationCommand) -> Result<(), CommandApplyError> {
            self.stops.lock().unwrap().push(command.epoch);
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingLifecycle {
        starts: Arc<Mutex<Vec<u64>>>,
        stops: Arc<Mutex<Vec<u64>>>,
        fail_next_start_retryable: Arc<AtomicBool>,
        fail_next_stop_retryable: Arc<AtomicBool>,
    }

    impl WorkloadLifecycle for RecordingLifecycle {
        fn start(&self, command: &AllocationCommand) -> Result<(), CommandApplyError> {
            self.starts.lock().unwrap().push(command.epoch);
            if self.fail_next_start_retryable.swap(false, Ordering::SeqCst) {
                return Err(CommandApplyError::retryable(
                    "launcher temporarily unavailable",
                ));
            }
            Ok(())
        }

        fn stop(&self, command: &AllocationCommand) -> Result<(), CommandApplyError> {
            self.stops.lock().unwrap().push(command.epoch);
            if self.fail_next_stop_retryable.swap(false, Ordering::SeqCst) {
                return Err(CommandApplyError::retryable(
                    "stop transport temporarily unavailable",
                ));
            }
            Ok(())
        }
    }

    fn path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "nulang-node-executor-{name}-{}-{unique}.json",
            std::process::id()
        ))
    }

    fn command(kind: AllocationCommandKind, epoch: u64) -> AllocationCommand {
        AllocationCommand {
            command_id: format!("cmd-{kind:?}-{epoch}"),
            evaluation_id: format!("eval-{epoch}"),
            deployment_id: "api".into(),
            revision: epoch,
            replica: 0,
            node_id: 7,
            epoch,
            kind,
            state: AllocationCommandState::Pending,
        }
    }

    #[test]
    fn successful_start_is_idempotent_across_node_agent_restart() {
        let state_path = path("restart");
        let lifecycle = RecordingLifecycle::default();
        let start = command(AllocationCommandKind::Start, 1);

        {
            let executor = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();
            executor.apply(&start).unwrap();
            assert_eq!(
                executor.allocation("api", 0).unwrap().phase,
                NodeAllocationPhase::Running
            );
        }

        let reopened = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();
        reopened.apply(&start).unwrap();
        assert_eq!(lifecycle.starts.lock().unwrap().as_slice(), &[1]);

        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn retryable_start_failure_replays_from_persisted_starting_state() {
        let state_path = path("retry");
        let lifecycle = RecordingLifecycle::default();
        lifecycle
            .fail_next_start_retryable
            .store(true, Ordering::SeqCst);
        let executor = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();
        let start = command(AllocationCommandKind::Start, 3);

        let first = executor.apply(&start).unwrap_err();
        assert!(first.retryable);
        assert_eq!(
            executor.allocation("api", 0).unwrap().phase,
            NodeAllocationPhase::Starting
        );

        executor.apply(&start).unwrap();
        assert_eq!(
            executor.allocation("api", 0).unwrap().phase,
            NodeAllocationPhase::Running
        );
        assert_eq!(lifecycle.starts.lock().unwrap().as_slice(), &[3, 3]);

        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn newer_epoch_cannot_start_until_prior_epoch_is_stopped() {
        let state_path = path("replace");
        let lifecycle = RecordingLifecycle::default();
        let executor = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();

        executor
            .apply(&command(AllocationCommandKind::Start, 1))
            .unwrap();
        let blocked = executor
            .apply(&command(AllocationCommandKind::Start, 2))
            .unwrap_err();
        assert!(blocked.retryable);
        assert_eq!(lifecycle.starts.lock().unwrap().as_slice(), &[1]);

        executor
            .apply(&command(AllocationCommandKind::Stop, 1))
            .unwrap();
        executor
            .apply(&command(AllocationCommandKind::Start, 2))
            .unwrap();

        assert_eq!(
            executor.allocation("api", 0).unwrap(),
            record(
                &command(AllocationCommandKind::Start, 2),
                NodeAllocationPhase::Running
            )
        );
        assert_eq!(lifecycle.starts.lock().unwrap().as_slice(), &[1, 2]);

        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn retryable_stop_failure_leaves_fence_and_blocks_replacement_until_retry() {
        let state_path = path("stop-retry");
        let lifecycle = RecordingLifecycle::default();
        let executor = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();

        executor
            .apply(&command(AllocationCommandKind::Start, 1))
            .unwrap();
        lifecycle
            .fail_next_stop_retryable
            .store(true, Ordering::SeqCst);

        let stop_error = executor
            .apply(&command(AllocationCommandKind::Stop, 1))
            .unwrap_err();
        assert!(stop_error.retryable);
        assert_eq!(
            executor.allocation("api", 0).unwrap().phase,
            NodeAllocationPhase::Stopping
        );

        let blocked = executor
            .apply(&command(AllocationCommandKind::Start, 2))
            .unwrap_err();
        assert!(blocked.retryable);
        assert_eq!(lifecycle.starts.lock().unwrap().as_slice(), &[1]);

        executor
            .apply(&command(AllocationCommandKind::Stop, 1))
            .unwrap();
        executor
            .apply(&command(AllocationCommandKind::Start, 2))
            .unwrap();

        assert_eq!(
            executor.allocation("api", 0).unwrap().phase,
            NodeAllocationPhase::Running
        );
        assert_eq!(executor.allocation("api", 0).unwrap().epoch, 2);
        assert_eq!(lifecycle.stops.lock().unwrap().as_slice(), &[1, 1]);

        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn stop_before_start_persists_tombstone_across_restart() {
        let state_path = path("stop-before-start");
        let lifecycle = RecordingLifecycle::default();
        let stop = command(AllocationCommandKind::Stop, 7);

        {
            let executor = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();
            executor.apply(&stop).unwrap();
            assert_eq!(
                executor.allocation("api", 0).unwrap().phase,
                NodeAllocationPhase::Stopped
            );
        }

        let reopened = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();
        let same_epoch = reopened
            .apply(&command(AllocationCommandKind::Start, 7))
            .unwrap_err();
        assert!(!same_epoch.retryable);

        reopened
            .apply(&command(AllocationCommandKind::Start, 6))
            .unwrap();
        assert!(lifecycle.starts.lock().unwrap().is_empty());

        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn stopped_epoch_cannot_be_resurrected_after_restart() {
        let state_path = path("tombstone");
        let lifecycle = RecordingLifecycle::default();

        {
            let executor = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();
            executor
                .apply(&command(AllocationCommandKind::Start, 5))
                .unwrap();
            executor
                .apply(&command(AllocationCommandKind::Stop, 5))
                .unwrap();
        }

        let reopened = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();
        let same_epoch = reopened
            .apply(&command(AllocationCommandKind::Start, 5))
            .unwrap_err();
        assert!(!same_epoch.retryable);

        reopened
            .apply(&command(AllocationCommandKind::Start, 4))
            .unwrap();
        assert_eq!(lifecycle.starts.lock().unwrap().as_slice(), &[5]);

        let _ = fs::remove_file(state_path);
    }

    #[test]
    fn stale_stop_cleans_old_epoch_without_mutating_newer_record() {
        let state_path = path("stale-stop");
        let lifecycle = RecordingLifecycle::default();
        let executor = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();

        executor
            .apply(&command(AllocationCommandKind::Start, 1))
            .unwrap();
        executor
            .apply(&command(AllocationCommandKind::Stop, 1))
            .unwrap();
        executor
            .apply(&command(AllocationCommandKind::Start, 2))
            .unwrap();

        executor
            .apply(&command(AllocationCommandKind::Stop, 1))
            .unwrap();

        let current = executor.allocation("api", 0).unwrap();
        assert_eq!(current.epoch, 2);
        assert_eq!(current.phase, NodeAllocationPhase::Running);
        assert_eq!(lifecycle.stops.lock().unwrap().as_slice(), &[1, 1]);

        let _ = fs::remove_file(state_path);
    }

    fn registered_revision(revision: u64) -> WorkloadRevisionSpec {
        use crate::workload_revision::{
            WorkloadArtifactIdentity, WorkloadLaunchConfig, WORKLOAD_ARTIFACT_KIND_NBC_V1,
        };

        WorkloadRevisionSpec::new(
            "api",
            revision,
            "api-package",
            format!("1.0.{revision}"),
            WorkloadArtifactIdentity {
                kind: WORKLOAD_ARTIFACT_KIND_NBC_V1.into(),
                artifact_id: format!("artifact:api-{revision}"),
                digest: format!("blake3:{}", "a".repeat(64)),
                behavior_manifest_digest: format!("blake3:{}", "b".repeat(64)),
                target: "x86_64-unknown-linux-gnu".into(),
                abi: "nulang-v1".into(),
                backend: "bytecode".into(),
            },
            WorkloadLaunchConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn revision_bound_lifecycle_fails_closed_before_unregistered_start() {
        use crate::store::MemoryControlStore;

        let store = MemoryControlStore::default();
        let lifecycle = RecordingResolvedLifecycle::default();
        let bound = RevisionBoundLifecycle::new(
            ControlStoreWorkloadResolver::new(&store),
            lifecycle.clone(),
        );

        let error = bound
            .start(&command(AllocationCommandKind::Start, 1))
            .unwrap_err();
        assert!(!error.retryable);
        assert!(lifecycle.starts.lock().unwrap().is_empty());
    }

    #[test]
    fn revision_bound_lifecycle_passes_registered_identity_to_launcher() {
        use crate::store::MemoryControlStore;

        let store = MemoryControlStore::default();
        let revision = registered_revision(1);
        store.register_workload_revision(&revision).unwrap();

        let lifecycle = RecordingResolvedLifecycle::default();
        let bound = RevisionBoundLifecycle::new(
            ControlStoreWorkloadResolver::new(&store),
            lifecycle.clone(),
        );
        bound
            .start(&command(AllocationCommandKind::Start, 1))
            .unwrap();

        let starts = lifecycle.starts.lock().unwrap();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].0, 1);
        assert_eq!(starts[0].1, revision.digest().unwrap());
    }

    #[test]
    fn revision_bound_lifecycle_stop_does_not_require_registry_lookup() {
        use crate::store::MemoryControlStore;

        let store = MemoryControlStore::default();
        let lifecycle = RecordingResolvedLifecycle::default();
        let bound = RevisionBoundLifecycle::new(
            ControlStoreWorkloadResolver::new(&store),
            lifecycle.clone(),
        );

        bound
            .stop(&command(AllocationCommandKind::Stop, 9))
            .unwrap();
        assert_eq!(lifecycle.stops.lock().unwrap().as_slice(), &[9]);
    }

    #[test]
    fn command_for_different_node_fails_closed_before_side_effect() {
        let state_path = path("wrong-node");
        let lifecycle = RecordingLifecycle::default();
        let executor = FencedNodeExecutor::open(7, &state_path, lifecycle.clone()).unwrap();
        let mut start = command(AllocationCommandKind::Start, 1);
        start.node_id = 8;

        let error = executor.apply(&start).unwrap_err();
        assert!(!error.retryable);
        assert!(lifecycle.starts.lock().unwrap().is_empty());
        assert!(executor.allocation("api", 0).is_none());

        let _ = fs::remove_file(state_path);
    }
}
