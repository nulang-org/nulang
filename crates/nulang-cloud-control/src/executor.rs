use crate::store::{
    AllocationCommand, AllocationCommandKind, ControlStore, StoreError,
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandApplyError {
    pub message: String,
    pub retryable: bool,
}

impl CommandApplyError {
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

/// Node/runtime delivery boundary for allocation commands.
///
/// Implementations MUST be idempotent by the allocation identity carried in the
/// command. A controller can crash after apply() succeeds but before the outbox
/// ACK is durable, so the same Start or Stop may be delivered again.
pub trait AllocationCommandSink: Send + Sync {
    fn apply(&self, command: &AllocationCommand) -> Result<(), CommandApplyError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    Applied,
    /// The Start lost authority before delivery (or after a previous
    /// crash-after-start). A Stop was confirmed before the Start command was
    /// acknowledged, so a stale workload cannot be left running by recovery.
    StaleStartFenced,
    /// A replacement Stop for this logical replica failed in this dispatch.
    /// New Starts for the same replica are left pending rather than risking
    /// overlapping live owners.
    BlockedByStopFailure,
    Failed {
        message: String,
        retryable: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchRecord {
    pub command_id: String,
    pub outcome: DispatchOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DispatchReport {
    pub records: Vec<DispatchRecord>,
}

/// Drain the current durable outbox snapshot safely.
///
/// Stops are attempted before Starts. A stale pending Start is treated as
/// ambiguous: it may have executed before a controller crash, so the dispatcher
/// sends an idempotent compensating Stop before acknowledging it. A Start that
/// becomes stale between its pre-check and post-apply check is handled the same
/// way.
///
/// This function is safe to retry after crashes when the sink obeys the
/// idempotency contract. Until durable command claiming is added, deployments
/// should run one logical dispatcher per control-store scope; multiple
/// concurrent dispatchers remain functionally idempotent but do not provide a
/// global Stop-before-Start ordering guarantee.
pub fn dispatch_pending(
    store: &dyn ControlStore,
    sink: &dyn AllocationCommandSink,
) -> Result<DispatchReport, StoreError> {
    let mut commands = store.pending_commands()?;
    commands.sort_by(|left, right| {
        command_priority(left.kind)
            .cmp(&command_priority(right.kind))
            .then_with(|| left.deployment_id.cmp(&right.deployment_id))
            .then_with(|| left.replica.cmp(&right.replica))
            .then_with(|| left.epoch.cmp(&right.epoch))
            .then_with(|| left.command_id.cmp(&right.command_id))
    });

    let mut report = DispatchReport::default();
    let mut failed_stops = BTreeSet::new();
    let mut confirmed_stops = BTreeSet::new();

    for command in commands {
        let replica_key = (command.deployment_id.clone(), command.replica);
        let allocation_key = (
            command.deployment_id.clone(),
            command.revision,
            command.replica,
            command.node_id,
            command.epoch,
        );

        match command.kind {
            AllocationCommandKind::Stop => match sink.apply(&command) {
                Ok(()) => {
                    store.acknowledge_command(&command.command_id)?;
                    confirmed_stops.insert(allocation_key);
                    report.records.push(DispatchRecord {
                        command_id: command.command_id,
                        outcome: DispatchOutcome::Applied,
                    });
                }
                Err(error) => {
                    failed_stops.insert(replica_key);
                    report.records.push(DispatchRecord {
                        command_id: command.command_id,
                        outcome: DispatchOutcome::Failed {
                            message: error.message,
                            retryable: error.retryable,
                        },
                    });
                }
            },
            AllocationCommandKind::Start => {
                if failed_stops.contains(&replica_key) {
                    report.records.push(DispatchRecord {
                        command_id: command.command_id,
                        outcome: DispatchOutcome::BlockedByStopFailure,
                    });
                    continue;
                }

                if !store.start_command_is_authoritative(&command)? {
                    if confirmed_stops.contains(&allocation_key) {
                        store.acknowledge_command(&command.command_id)?;
                        report.records.push(DispatchRecord {
                            command_id: command.command_id,
                            outcome: DispatchOutcome::StaleStartFenced,
                        });
                        continue;
                    }

                    let fence = compensating_stop(&command);
                    match sink.apply(&fence) {
                        Ok(()) => {
                            store.acknowledge_command(&command.command_id)?;
                            confirmed_stops.insert(allocation_key);
                            report.records.push(DispatchRecord {
                                command_id: command.command_id,
                                outcome: DispatchOutcome::StaleStartFenced,
                            });
                        }
                        Err(error) => {
                            report.records.push(DispatchRecord {
                                command_id: command.command_id,
                                outcome: DispatchOutcome::Failed {
                                    message: error.message,
                                    retryable: error.retryable,
                                },
                            });
                        }
                    }
                    continue;
                }

                match sink.apply(&command) {
                    Ok(()) => {
                        if store.start_command_is_authoritative(&command)? {
                            store.acknowledge_command(&command.command_id)?;
                            report.records.push(DispatchRecord {
                                command_id: command.command_id,
                                outcome: DispatchOutcome::Applied,
                            });
                        } else {
                            let fence = compensating_stop(&command);
                            match sink.apply(&fence) {
                                Ok(()) => {
                                    store.acknowledge_command(&command.command_id)?;
                                    confirmed_stops.insert(allocation_key);
                                    report.records.push(DispatchRecord {
                                        command_id: command.command_id,
                                        outcome: DispatchOutcome::StaleStartFenced,
                                    });
                                }
                                Err(error) => {
                                    report.records.push(DispatchRecord {
                                        command_id: command.command_id,
                                        outcome: DispatchOutcome::Failed {
                                            message: error.message,
                                            retryable: error.retryable,
                                        },
                                    });
                                }
                            }
                        }
                    }
                    Err(error) => report.records.push(DispatchRecord {
                        command_id: command.command_id,
                        outcome: DispatchOutcome::Failed {
                            message: error.message,
                            retryable: error.retryable,
                        },
                    }),
                }
            }
        }
    }

    Ok(report)
}

fn command_priority(kind: AllocationCommandKind) -> u8 {
    match kind {
        AllocationCommandKind::Stop => 0,
        AllocationCommandKind::Start => 1,
    }
}

fn compensating_stop(start: &AllocationCommand) -> AllocationCommand {
    let mut stop = start.clone();
    stop.command_id = format!("{}:fence", start.command_id);
    stop.kind = AllocationCommandKind::Stop;
    stop
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        DeploymentSpec, Evaluation, EvaluationCause, NodeDescriptor, NodeResources, NodeState,
        PlacementConstraints, PlacementPreferences, ResourceRequest,
    };
    use crate::reconcile_once;
    use crate::store::MemoryControlStore;
    use nulang_capacity::{Architecture, TrustTier};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        applied: Mutex<Vec<(AllocationCommandKind, u64)>>,
        fail_stop_epoch: Option<u64>,
    }

    impl AllocationCommandSink for RecordingSink {
        fn apply(&self, command: &AllocationCommand) -> Result<(), CommandApplyError> {
            if command.kind == AllocationCommandKind::Stop
                && self.fail_stop_epoch == Some(command.epoch)
            {
                return Err(CommandApplyError::retryable("stop transport unavailable"));
            }
            self.applied
                .lock()
                .unwrap()
                .push((command.kind, command.epoch));
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

    fn evaluation(id: &str, revision: u64) -> Evaluation {
        Evaluation {
            evaluation_id: id.into(),
            deployment_id: "api".into(),
            revision,
            cause: EvaluationCause::DeploymentChanged,
        }
    }

    fn node() -> NodeDescriptor {
        NodeDescriptor {
            node_id: 1,
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
    fn test_fresh_start_is_applied_and_acknowledged() {
        let store = MemoryControlStore::default();
        reconcile_once(
            &store,
            &evaluation("eval-1", 1),
            &deployment(1),
            &[node()],
        )
        .unwrap();

        let sink = RecordingSink::default();
        let report = dispatch_pending(&store, &sink).unwrap();

        assert_eq!(report.records.len(), 1);
        assert_eq!(report.records[0].outcome, DispatchOutcome::Applied);
        assert!(store.pending_commands().unwrap().is_empty());
        assert_eq!(
            sink.applied.lock().unwrap().as_slice(),
            &[(AllocationCommandKind::Start, 1)]
        );
    }

    #[test]
    fn test_superseding_evaluation_stops_old_epoch_before_new_start() {
        let store = MemoryControlStore::default();
        reconcile_once(
            &store,
            &evaluation("eval-1", 1),
            &deployment(1),
            &[node()],
        )
        .unwrap();
        reconcile_once(
            &store,
            &evaluation("eval-2", 2),
            &deployment(2),
            &[node()],
        )
        .unwrap();

        let sink = RecordingSink::default();
        dispatch_pending(&store, &sink).unwrap();

        assert_eq!(
            sink.applied.lock().unwrap().as_slice(),
            &[
                (AllocationCommandKind::Stop, 1),
                (AllocationCommandKind::Start, 2),
            ]
        );
        assert!(store.pending_commands().unwrap().is_empty());
    }

    #[test]
    fn test_failed_stop_blocks_replacement_start() {
        let store = MemoryControlStore::default();
        reconcile_once(
            &store,
            &evaluation("eval-1", 1),
            &deployment(1),
            &[node()],
        )
        .unwrap();
        reconcile_once(
            &store,
            &evaluation("eval-2", 2),
            &deployment(2),
            &[node()],
        )
        .unwrap();

        let sink = RecordingSink {
            applied: Mutex::new(Vec::new()),
            fail_stop_epoch: Some(1),
        };
        let report = dispatch_pending(&store, &sink).unwrap();

        assert!(report.records.iter().any(|record| {
            matches!(
                record.outcome,
                DispatchOutcome::Failed {
                    retryable: true,
                    ..
                }
            )
        }));
        assert!(report.records.iter().any(|record| {
            record.outcome == DispatchOutcome::BlockedByStopFailure
        }));
        assert!(sink.applied.lock().unwrap().is_empty());
        assert!(!store.pending_commands().unwrap().is_empty());
    }
}
