use crate::store::{AllocationCommand, AllocationCommandKind, ControlStore, StoreError};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Drain the durable outbox with generation-fenced command leases.
///
/// Each command is claimed atomically before delivery. A claim carries a
/// durable generation; if its lease expires and another dispatcher reclaims
/// the command, the stale dispatcher can no longer acknowledge or release the
/// newer claim. Pending Stops block Starts for the same logical replica at the
/// claim boundary, so active-active dispatchers preserve Stop-before-Start
/// without relying on process-local ordering.
pub fn dispatch_pending(
    store: &dyn ControlStore,
    sink: &dyn AllocationCommandSink,
) -> Result<DispatchReport, StoreError> {
    let dispatcher_id = format!("controller-{}", std::process::id());
    dispatch_pending_as(store, sink, &dispatcher_id, 30_000)
}

/// Dispatch using an explicit owner id and lease duration.
///
/// This is useful for long-running controller processes that already have a
/// stable instance identity. The clock is sampled before each claim so a batch
/// does not progressively shorten later command leases.
pub fn dispatch_pending_as(
    store: &dyn ControlStore,
    sink: &dyn AllocationCommandSink,
    dispatcher_id: &str,
    lease_ms: u64,
) -> Result<DispatchReport, StoreError> {
    let mut report = DispatchReport::default();

    loop {
        let now_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                StoreError::Backend(format!("system clock before UNIX epoch: {error}"))
            })?
            .as_millis()
            .try_into()
            .map_err(|_| StoreError::Backend("system clock millisecond value overflow".into()))?;

        let Some(claim) = store.claim_next_command(dispatcher_id, now_unix_ms, lease_ms)? else {
            break;
        };
        let command = &claim.command;

        match command.kind {
            AllocationCommandKind::Stop => match sink.apply(command) {
                Ok(()) => {
                    store.acknowledge_claimed_command(&claim)?;
                    report.records.push(DispatchRecord {
                        command_id: command.command_id.clone(),
                        outcome: DispatchOutcome::Applied,
                    });
                }
                Err(error) => {
                    store.release_command_claim(&claim)?;
                    report.records.push(DispatchRecord {
                        command_id: command.command_id.clone(),
                        outcome: DispatchOutcome::Failed {
                            message: error.message,
                            retryable: error.retryable,
                        },
                    });
                    break;
                }
            },
            AllocationCommandKind::Start => {
                if !store.start_command_is_authoritative(command)? {
                    let fence = compensating_stop(command);
                    match sink.apply(&fence) {
                        Ok(()) => {
                            store.acknowledge_claimed_command(&claim)?;
                            report.records.push(DispatchRecord {
                                command_id: command.command_id.clone(),
                                outcome: DispatchOutcome::StaleStartFenced,
                            });
                        }
                        Err(error) => {
                            store.release_command_claim(&claim)?;
                            report.records.push(DispatchRecord {
                                command_id: command.command_id.clone(),
                                outcome: DispatchOutcome::Failed {
                                    message: error.message,
                                    retryable: error.retryable,
                                },
                            });
                            break;
                        }
                    }
                    continue;
                }

                match sink.apply(command) {
                    Ok(()) => {
                        if store.start_command_is_authoritative(command)? {
                            store.acknowledge_claimed_command(&claim)?;
                            report.records.push(DispatchRecord {
                                command_id: command.command_id.clone(),
                                outcome: DispatchOutcome::Applied,
                            });
                        } else {
                            let fence = compensating_stop(command);
                            match sink.apply(&fence) {
                                Ok(()) => {
                                    store.acknowledge_claimed_command(&claim)?;
                                    report.records.push(DispatchRecord {
                                        command_id: command.command_id.clone(),
                                        outcome: DispatchOutcome::StaleStartFenced,
                                    });
                                }
                                Err(error) => {
                                    store.release_command_claim(&claim)?;
                                    report.records.push(DispatchRecord {
                                        command_id: command.command_id.clone(),
                                        outcome: DispatchOutcome::Failed {
                                            message: error.message,
                                            retryable: error.retryable,
                                        },
                                    });
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        store.release_command_claim(&claim)?;
                        report.records.push(DispatchRecord {
                            command_id: command.command_id.clone(),
                            outcome: DispatchOutcome::Failed {
                                message: error.message,
                                retryable: error.retryable,
                            },
                        });
                        break;
                    }
                }
            }
        }
    }

    Ok(report)
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
        reconcile_once(&store, &evaluation("eval-1", 1), &deployment(1), &[node()]).unwrap();

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
        reconcile_once(&store, &evaluation("eval-1", 1), &deployment(1), &[node()]).unwrap();
        reconcile_once(&store, &evaluation("eval-2", 2), &deployment(2), &[node()]).unwrap();

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
        reconcile_once(&store, &evaluation("eval-1", 1), &deployment(1), &[node()]).unwrap();
        reconcile_once(&store, &evaluation("eval-2", 2), &deployment(2), &[node()]).unwrap();

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
        assert_eq!(report.records.len(), 1);
        assert!(sink.applied.lock().unwrap().is_empty());
        assert!(!store.pending_commands().unwrap().is_empty());
    }
}
