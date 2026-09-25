use crate::store::{AllocationCommand, AllocationCommandKind, ControlStore, StoreError};
use std::cmp::Ordering;
use std::fmt;

/// Node-local state for one logical deployment replica.
///
/// The epoch is a durable fencing token. A node runtime must never allow work
/// from an epoch lower than the value returned here to become live again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeAllocationObservation {
    pub revision: u64,
    pub epoch: u64,
    pub status: NodeAllocationStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeAllocationStatus {
    Running,
    Stopped,
}

/// Provider/runtime boundary used by the Cloud node executor.
///
/// Implementations own the actual process/WASM/microVM lifecycle and MUST make
/// both operations idempotent by `(deployment_id, replica, epoch)`.
///
/// Before returning success:
/// - `start_allocation` must durably fence every lower epoch and make this
///   epoch the live owner.
/// - `stop_allocation` must durably install at least this epoch as a fence and
///   ensure no workload at this or any lower epoch remains live.
///
/// Those requirements close the crash window between local execution and the
/// control-plane outbox ACK. If the ACK fails, retry observes the durable local
/// epoch and does not repeat a non-idempotent runtime mutation.
pub trait AllocationRuntime {
    fn observe(
        &self,
        deployment_id: &str,
        replica: u32,
    ) -> Result<Option<NodeAllocationObservation>, String>;

    fn start_allocation(&mut self, command: &AllocationCommand) -> Result<(), String>;

    fn stop_allocation(&mut self, command: &AllocationCommand) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecutionReport {
    /// Total pending outbox commands returned by the control store.
    pub pending_seen: usize,
    /// Commands addressed to this node.
    pub local_commands: usize,
    /// Commands addressed to other nodes and left untouched.
    pub skipped_other_nodes: usize,
    /// Runtime start/stop operations actually invoked.
    pub runtime_actions: usize,
    /// Commands already reflected exactly in local durable runtime state.
    pub idempotent_acks: usize,
    /// Commands fenced by a newer local epoch and safely acknowledged as stale.
    pub stale_acks: usize,
    /// Commands acknowledged in the control-plane outbox.
    pub acknowledged: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutorError {
    Control(StoreError),
    Runtime {
        command_id: String,
        message: String,
    },
    SameEpochRevisionConflict {
        command_id: String,
        command_revision: u64,
        observed_revision: u64,
        epoch: u64,
    },
    FencedStart {
        command_id: String,
        epoch: u64,
    },
    RuntimeDidNotApply {
        command_id: String,
        expected_epoch: u64,
        observed: Option<NodeAllocationObservation>,
    },
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Control(error) => write!(f, "control-store error: {error}"),
            Self::Runtime {
                command_id,
                message,
            } => write!(f, "allocation command {command_id} failed: {message}"),
            Self::SameEpochRevisionConflict {
                command_id,
                command_revision,
                observed_revision,
                epoch,
            } => write!(
                f,
                "allocation command {command_id} epoch {epoch} has revision {command_revision}, but node durable state has revision {observed_revision}"
            ),
            Self::FencedStart { command_id, epoch } => write!(
                f,
                "allocation command {command_id} cannot restart stopped epoch {epoch}"
            ),
            Self::RuntimeDidNotApply {
                command_id,
                expected_epoch,
                observed,
            } => write!(
                f,
                "allocation command {command_id} returned success without installing epoch {expected_epoch}; observed {observed:?}"
            ),
        }
    }
}

impl std::error::Error for ExecutorError {}

impl From<StoreError> for ExecutorError {
    fn from(error: StoreError) -> Self {
        Self::Control(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Applied,
    Idempotent,
    Stale,
}

/// Execute one deterministic pass over the durable allocation-command outbox
/// for `node_id`.
///
/// Commands for other nodes remain pending. Local commands are ordered by
/// deployment, replica, revision, epoch, and then Start-before-Stop so retries
/// are deterministic and a later Stop at the same ownership epoch wins.
///
/// The control-plane command is acknowledged only after the local runtime
/// mutation is verified. A newer local epoch makes an older command stale and
/// therefore safe to acknowledge without touching the newer workload.
pub fn execute_pending_for_node(
    control: &dyn ControlStore,
    node_id: u64,
    runtime: &mut dyn AllocationRuntime,
) -> Result<ExecutionReport, ExecutorError> {
    let mut commands = control.pending_commands()?;
    let mut report = ExecutionReport {
        pending_seen: commands.len(),
        ..ExecutionReport::default()
    };

    commands.sort_by(command_order);

    for command in commands {
        if command.node_id != node_id {
            report.skipped_other_nodes += 1;
            continue;
        }
        report.local_commands += 1;

        let disposition = execute_one(runtime, &command)?;
        match disposition {
            Disposition::Applied => report.runtime_actions += 1,
            Disposition::Idempotent => report.idempotent_acks += 1,
            Disposition::Stale => report.stale_acks += 1,
        }

        // ACK is deliberately last. If it fails, the durable local fence makes
        // the next executor pass idempotent.
        control.acknowledge_command(&command.command_id)?;
        report.acknowledged += 1;
    }

    Ok(report)
}

fn command_order(left: &AllocationCommand, right: &AllocationCommand) -> Ordering {
    left.deployment_id
        .cmp(&right.deployment_id)
        .then_with(|| left.replica.cmp(&right.replica))
        .then_with(|| left.revision.cmp(&right.revision))
        .then_with(|| left.epoch.cmp(&right.epoch))
        .then_with(|| command_kind_order(left.kind).cmp(&command_kind_order(right.kind)))
        .then_with(|| left.command_id.cmp(&right.command_id))
}

fn command_kind_order(kind: AllocationCommandKind) -> u8 {
    match kind {
        AllocationCommandKind::Start => 0,
        AllocationCommandKind::Stop => 1,
    }
}

fn execute_one(
    runtime: &mut dyn AllocationRuntime,
    command: &AllocationCommand,
) -> Result<Disposition, ExecutorError> {
    let before = runtime
        .observe(&command.deployment_id, command.replica)
        .map_err(|message| ExecutorError::Runtime {
            command_id: command.command_id.clone(),
            message,
        })?;

    if let Some(observed) = before {
        if observed.epoch > command.epoch {
            return Ok(Disposition::Stale);
        }
        if observed.epoch == command.epoch && observed.revision != command.revision {
            return Err(ExecutorError::SameEpochRevisionConflict {
                command_id: command.command_id.clone(),
                command_revision: command.revision,
                observed_revision: observed.revision,
                epoch: command.epoch,
            });
        }

        if observed.epoch == command.epoch {
            match (command.kind, observed.status) {
                (AllocationCommandKind::Start, NodeAllocationStatus::Running)
                | (AllocationCommandKind::Stop, NodeAllocationStatus::Stopped) => {
                    return Ok(Disposition::Idempotent);
                }
                (AllocationCommandKind::Start, NodeAllocationStatus::Stopped) => {
                    // A stop at an epoch is a fence. Reusing that epoch would
                    // allow a delayed Start to resurrect an allocation.
                    return Err(ExecutorError::FencedStart {
                        command_id: command.command_id.clone(),
                        epoch: command.epoch,
                    });
                }
                (AllocationCommandKind::Stop, NodeAllocationStatus::Running) => {}
            }
        }
    }

    match command.kind {
        AllocationCommandKind::Start => runtime.start_allocation(command),
        AllocationCommandKind::Stop => runtime.stop_allocation(command),
    }
    .map_err(|message| ExecutorError::Runtime {
        command_id: command.command_id.clone(),
        message,
    })?;

    let after = runtime
        .observe(&command.deployment_id, command.replica)
        .map_err(|message| ExecutorError::Runtime {
            command_id: command.command_id.clone(),
            message,
        })?;

    if runtime_state_proves_applied(command, after) {
        Ok(Disposition::Applied)
    } else {
        Err(ExecutorError::RuntimeDidNotApply {
            command_id: command.command_id.clone(),
            expected_epoch: command.epoch,
            observed: after,
        })
    }
}

fn runtime_state_proves_applied(
    command: &AllocationCommand,
    observed: Option<NodeAllocationObservation>,
) -> bool {
    let Some(observed) = observed else {
        return false;
    };

    // A concurrently installed newer durable fence also proves this command can
    // never become authoritative again.
    if observed.epoch > command.epoch {
        return true;
    }
    if observed.epoch != command.epoch || observed.revision != command.revision {
        return false;
    }

    matches!(
        (command.kind, observed.status),
        (AllocationCommandKind::Start, NodeAllocationStatus::Running)
            | (AllocationCommandKind::Stop, NodeAllocationStatus::Stopped)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Evaluation, ObservedAllocation, PlacementPlan};
    use crate::store::{AllocationCommandState, CommitOutcome, EvaluationRecord};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct TestControlStore {
        commands: Mutex<Vec<AllocationCommand>>,
        fail_next_ack: AtomicBool,
    }

    impl TestControlStore {
        fn with_commands(commands: Vec<AllocationCommand>) -> Self {
            Self {
                commands: Mutex::new(commands),
                fail_next_ack: AtomicBool::new(false),
            }
        }

        fn fail_one_ack(&self) {
            self.fail_next_ack.store(true, AtomicOrdering::SeqCst);
        }

        fn pending_ids(&self) -> Vec<String> {
            self.commands
                .lock()
                .unwrap()
                .iter()
                .filter(|command| command.state == AllocationCommandState::Pending)
                .map(|command| command.command_id.clone())
                .collect()
        }
    }

    impl ControlStore for TestControlStore {
        fn record_evaluation(&self, _evaluation: &Evaluation) -> Result<(), StoreError> {
            Ok(())
        }

        fn evaluation(&self, _evaluation_id: &str) -> Result<Option<EvaluationRecord>, StoreError> {
            Ok(None)
        }

        fn committed_plan(
            &self,
            _evaluation_id: &str,
        ) -> Result<Option<PlacementPlan>, StoreError> {
            Ok(None)
        }

        fn allocations_for(
            &self,
            _deployment_id: &str,
        ) -> Result<Vec<ObservedAllocation>, StoreError> {
            Ok(Vec::new())
        }

        fn commit_plan(
            &self,
            _evaluation: &Evaluation,
            _plan: &PlacementPlan,
        ) -> Result<CommitOutcome, StoreError> {
            unreachable!("executor tests do not commit placement plans")
        }

        fn pending_commands(&self) -> Result<Vec<AllocationCommand>, StoreError> {
            Ok(self
                .commands
                .lock()
                .unwrap()
                .iter()
                .filter(|command| command.state == AllocationCommandState::Pending)
                .cloned()
                .collect())
        }

        fn acknowledge_command(&self, command_id: &str) -> Result<(), StoreError> {
            if self.fail_next_ack.swap(false, AtomicOrdering::SeqCst) {
                return Err(StoreError::Backend("injected ACK failure".into()));
            }
            let mut commands = self.commands.lock().unwrap();
            let command = commands
                .iter_mut()
                .find(|command| command.command_id == command_id)
                .ok_or_else(|| StoreError::CommandNotFound(command_id.to_owned()))?;
            command.state = AllocationCommandState::Acknowledged;
            Ok(())
        }
    }

    #[derive(Default)]
    struct TestRuntime {
        allocations: BTreeMap<(String, u32), NodeAllocationObservation>,
        starts: usize,
        stops: usize,
    }

    impl TestRuntime {
        fn seed(
            &mut self,
            deployment_id: &str,
            replica: u32,
            revision: u64,
            epoch: u64,
            status: NodeAllocationStatus,
        ) {
            self.allocations.insert(
                (deployment_id.to_owned(), replica),
                NodeAllocationObservation {
                    revision,
                    epoch,
                    status,
                },
            );
        }
    }

    impl AllocationRuntime for TestRuntime {
        fn observe(
            &self,
            deployment_id: &str,
            replica: u32,
        ) -> Result<Option<NodeAllocationObservation>, String> {
            Ok(self
                .allocations
                .get(&(deployment_id.to_owned(), replica))
                .copied())
        }

        fn start_allocation(&mut self, command: &AllocationCommand) -> Result<(), String> {
            self.starts += 1;
            self.seed(
                &command.deployment_id,
                command.replica,
                command.revision,
                command.epoch,
                NodeAllocationStatus::Running,
            );
            Ok(())
        }

        fn stop_allocation(&mut self, command: &AllocationCommand) -> Result<(), String> {
            self.stops += 1;
            self.seed(
                &command.deployment_id,
                command.replica,
                command.revision,
                command.epoch,
                NodeAllocationStatus::Stopped,
            );
            Ok(())
        }
    }

    fn command(
        id: &str,
        node_id: u64,
        revision: u64,
        epoch: u64,
        kind: AllocationCommandKind,
    ) -> AllocationCommand {
        AllocationCommand {
            command_id: id.into(),
            evaluation_id: format!("eval-{id}"),
            deployment_id: "orders".into(),
            revision,
            replica: 0,
            node_id,
            epoch,
            kind,
            state: AllocationCommandState::Pending,
        }
    }

    #[test]
    fn starts_local_allocation_then_acknowledges() {
        let control = TestControlStore::with_commands(vec![command(
            "start",
            7,
            3,
            1,
            AllocationCommandKind::Start,
        )]);
        let mut runtime = TestRuntime::default();

        let report = execute_pending_for_node(&control, 7, &mut runtime).unwrap();

        assert_eq!(runtime.starts, 1);
        assert_eq!(report.runtime_actions, 1);
        assert_eq!(report.acknowledged, 1);
        assert!(control.pending_ids().is_empty());
    }

    #[test]
    fn ack_failure_retries_without_restarting_same_epoch() {
        let control = TestControlStore::with_commands(vec![command(
            "start",
            7,
            3,
            4,
            AllocationCommandKind::Start,
        )]);
        control.fail_one_ack();
        let mut runtime = TestRuntime::default();

        assert!(matches!(
            execute_pending_for_node(&control, 7, &mut runtime),
            Err(ExecutorError::Control(StoreError::Backend(_)))
        ));
        assert_eq!(runtime.starts, 1);
        assert_eq!(control.pending_ids(), vec!["start".to_string()]);

        let report = execute_pending_for_node(&control, 7, &mut runtime).unwrap();
        assert_eq!(
            runtime.starts, 1,
            "retry must not start the same epoch twice"
        );
        assert_eq!(report.idempotent_acks, 1);
        assert!(control.pending_ids().is_empty());
    }

    #[test]
    fn stale_start_is_acknowledged_without_touching_newer_epoch() {
        let control = TestControlStore::with_commands(vec![command(
            "old-start",
            7,
            2,
            4,
            AllocationCommandKind::Start,
        )]);
        let mut runtime = TestRuntime::default();
        runtime.seed("orders", 0, 3, 5, NodeAllocationStatus::Running);

        let report = execute_pending_for_node(&control, 7, &mut runtime).unwrap();

        assert_eq!(runtime.starts, 0);
        assert_eq!(report.stale_acks, 1);
        assert_eq!(runtime.observe("orders", 0).unwrap().unwrap().epoch, 5);
    }

    #[test]
    fn stale_stop_never_kills_newer_owner() {
        let control = TestControlStore::with_commands(vec![command(
            "old-stop",
            7,
            2,
            4,
            AllocationCommandKind::Stop,
        )]);
        let mut runtime = TestRuntime::default();
        runtime.seed("orders", 0, 3, 5, NodeAllocationStatus::Running);

        let report = execute_pending_for_node(&control, 7, &mut runtime).unwrap();

        assert_eq!(runtime.stops, 0);
        assert_eq!(report.stale_acks, 1);
        assert_eq!(
            runtime.observe("orders", 0).unwrap().unwrap().status,
            NodeAllocationStatus::Running
        );
    }

    #[test]
    fn stop_advances_fence_even_when_node_has_no_local_allocation() {
        let control = TestControlStore::with_commands(vec![command(
            "stop",
            7,
            3,
            6,
            AllocationCommandKind::Stop,
        )]);
        let mut runtime = TestRuntime::default();

        let report = execute_pending_for_node(&control, 7, &mut runtime).unwrap();

        assert_eq!(runtime.stops, 1);
        assert_eq!(report.runtime_actions, 1);
        assert_eq!(
            runtime.observe("orders", 0).unwrap().unwrap(),
            NodeAllocationObservation {
                revision: 3,
                epoch: 6,
                status: NodeAllocationStatus::Stopped,
            }
        );
    }

    #[test]
    fn stopped_epoch_cannot_be_resurrected_by_delayed_start() {
        let control = TestControlStore::with_commands(vec![command(
            "late-start",
            7,
            3,
            6,
            AllocationCommandKind::Start,
        )]);
        let mut runtime = TestRuntime::default();
        runtime.seed("orders", 0, 3, 6, NodeAllocationStatus::Stopped);

        assert!(matches!(
            execute_pending_for_node(&control, 7, &mut runtime),
            Err(ExecutorError::FencedStart { epoch: 6, .. })
        ));
        assert_eq!(runtime.starts, 0);
        assert_eq!(control.pending_ids(), vec!["late-start".to_string()]);
    }

    #[test]
    fn commands_for_other_nodes_remain_pending() {
        let control = TestControlStore::with_commands(vec![command(
            "remote",
            9,
            3,
            1,
            AllocationCommandKind::Start,
        )]);
        let mut runtime = TestRuntime::default();

        let report = execute_pending_for_node(&control, 7, &mut runtime).unwrap();

        assert_eq!(report.skipped_other_nodes, 1);
        assert_eq!(runtime.starts, 0);
        assert_eq!(control.pending_ids(), vec!["remote".to_string()]);
    }

    #[test]
    fn start_before_stop_at_same_epoch_leaves_fence_stopped() {
        let control = TestControlStore::with_commands(vec![
            command("z-stop", 7, 3, 8, AllocationCommandKind::Stop),
            command("a-start", 7, 3, 8, AllocationCommandKind::Start),
        ]);
        let mut runtime = TestRuntime::default();

        let report = execute_pending_for_node(&control, 7, &mut runtime).unwrap();

        assert_eq!(report.runtime_actions, 2);
        assert_eq!(
            runtime.observe("orders", 0).unwrap().unwrap().status,
            NodeAllocationStatus::Stopped
        );
        assert!(control.pending_ids().is_empty());
    }
}
