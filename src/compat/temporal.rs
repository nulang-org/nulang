//! Temporal compatibility boundary.
//!
//! This module intentionally contains protocol-adapter semantics only. It does
//! not implement Temporal's gRPC `WorkflowService`, task queues, or wire
//! protobufs. A future gateway can decode Temporal requests into these types,
//! then stage the resulting Nulang workflow events and durable effects in an
//! atomic `DurableTransition`.
//!
//! The important invariant is one-way dependency:
//!
//! ```text
//! Temporal protocol -> compat::temporal -> Nulang durable primitives
//! ```
//!
//! The Nulang runtime must not depend on Temporal protocol types.

use crate::durable_effect::{DurableEffectRecord, DurableEffectSpec};
use crate::durable_effect_persistence::DurableEffectPersistenceRecord;
use crate::primitives::{DeliverySemantics, EffectBoundary};
use crate::runtime::WorkflowEvent;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;

/// Version of the semantic contract shared with managed compatibility
/// adapters such as Nulang Cloud.
///
/// Increment this only when the meaning or encoding expected by an external
/// adapter changes incompatibly. It is independent from Temporal's own API
/// version and from Nulang bytecode/artifact versions.
pub const TEMPORAL_COMPATIBILITY_CONTRACT_VERSION: u16 = 1;

/// Identity of one concrete Temporal workflow execution.
///
/// A concrete adapter should resolve an omitted Temporal run id before
/// constructing this value. Replay identity must never depend on "latest run".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TemporalWorkflowExecution {
    pub namespace: String,
    pub workflow_id: String,
    pub run_id: String,
}

impl TemporalWorkflowExecution {
    pub fn new(
        namespace: impl Into<String>,
        workflow_id: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Result<Self, TemporalCompatError> {
        let execution = Self {
            namespace: namespace.into(),
            workflow_id: workflow_id.into(),
            run_id: run_id.into(),
        };
        validate_non_empty("namespace", &execution.namespace)?;
        validate_non_empty("workflow_id", &execution.workflow_id)?;
        validate_non_empty("run_id", &execution.run_id)?;
        Ok(execution)
    }

    /// Collision-resistant textual identity for use as input to Nulang's
    /// durable-effect identity derivation.
    ///
    /// Components are length-prefixed so values containing separators cannot
    /// alias a different workflow execution.
    pub fn stable_key(&self) -> String {
        let mut out = String::from("temporal-execution/v1");
        append_component(&mut out, &self.namespace);
        append_component(&mut out, &self.workflow_id);
        append_component(&mut out, &self.run_id);
        out
    }
}

/// Replay context for one Temporal workflow task.
///
/// `workflow_task_started_event_id` is part of the stable execution key so
/// replaying the same task derives the same durable-effect IDs. A later
/// workflow task derives different IDs even when it schedules an activity with
/// the same type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalWorkflowTaskContext {
    pub execution: TemporalWorkflowExecution,
    pub nulang_actor_id: u64,
    pub workflow_task_started_event_id: u64,
    pub transition_sequence: u64,
}

impl TemporalWorkflowTaskContext {
    pub fn new(
        execution: TemporalWorkflowExecution,
        nulang_actor_id: u64,
        workflow_task_started_event_id: u64,
        transition_sequence: u64,
    ) -> Result<Self, TemporalCompatError> {
        if workflow_task_started_event_id == 0 {
            return Err(TemporalCompatError::ZeroWorkflowTaskStartedEventId);
        }
        if transition_sequence == 0 {
            return Err(TemporalCompatError::ZeroTransitionSequence);
        }

        Ok(Self {
            execution,
            nulang_actor_id,
            workflow_task_started_event_id,
            transition_sequence,
        })
    }

    /// Prepare a Temporal activity as an external Nulang durable effect.
    ///
    /// Temporal activity execution is retryable, so the default contract is
    /// at-least-once. A gateway may opt into
    /// `EffectivelyOnceWithDeduplication` only when the activity adapter has
    /// a real deduplication/idempotency contract.
    pub fn prepare_activity(
        &self,
        command_ordinal: u32,
        request: TemporalActivityRequest,
    ) -> Result<TemporalActivityPlan, TemporalCompatError> {
        self.prepare_activity_with_delivery(
            command_ordinal,
            request,
            DeliverySemantics::AtLeastOnce,
        )
    }

    pub fn prepare_activity_with_delivery(
        &self,
        command_ordinal: u32,
        request: TemporalActivityRequest,
        delivery: DeliverySemantics,
    ) -> Result<TemporalActivityPlan, TemporalCompatError> {
        request.validate()?;

        let execution_key = self.activity_execution_key(&request.activity_id);
        let effect_operation = format!("Temporal.Activity/{}", request.activity_type);
        let effect_id = crate::durable_effect::DurableEffectId::derive(
            self.nulang_actor_id,
            &execution_key,
            command_ordinal,
            &effect_operation,
        );
        let spec = DurableEffectSpec::new(
            effect_id,
            effect_operation,
            EffectBoundary::External,
            delivery,
        );
        let durable_request = request.durable_request_bytes();
        let record = DurableEffectRecord::prepare(spec, &durable_request);

        Ok(TemporalActivityPlan { request, record })
    }

    /// Translate one command emitted by a Temporal workflow task into a
    /// Nulang-side compatibility action.
    ///
    /// `command_ordinal` is the ordinal in the complete Temporal command list,
    /// not merely among activities. Keeping the global ordinal in durable
    /// effect identity makes command reordering observable during replay.
    pub fn plan_command(
        &self,
        command_ordinal: u32,
        command: TemporalCommand,
    ) -> Result<TemporalCommandPlan, TemporalCompatError> {
        match command {
            TemporalCommand::ScheduleActivity(request) => Ok(TemporalCommandPlan::Activity(
                self.prepare_activity(command_ordinal, request)?,
            )),
            TemporalCommand::StartTimer {
                timer_id,
                duration_ms,
            } => Ok(TemporalCommandPlan::WorkflowEvent(
                self.timer_started(timer_id, duration_ms)?,
            )),
            TemporalCommand::CompleteWorkflow { result } => {
                Ok(TemporalCommandPlan::CompleteWorkflow { result })
            }
            TemporalCommand::FailWorkflow { message } => {
                Ok(TemporalCommandPlan::FailWorkflow { message })
            }
            TemporalCommand::ContinueAsNew {
                workflow_type,
                input,
            } => {
                validate_non_empty("workflow_type", &workflow_type)?;
                Ok(TemporalCommandPlan::ContinueAsNew {
                    workflow_type,
                    input,
                })
            }
        }
    }

    /// Stage a Temporal timer using Nulang's existing durable workflow-event
    /// representation. The containing `DurableTransition` supplies atomicity.
    pub fn timer_started(
        &self,
        timer_id: impl Into<String>,
        duration_ms: u64,
    ) -> Result<WorkflowEvent, TemporalCompatError> {
        let timer_id = timer_id.into();
        validate_non_empty("timer_id", &timer_id)?;
        Ok(WorkflowEvent::TimerSet {
            sequence: self.transition_sequence,
            name: timer_id,
            duration_ms,
        })
    }

    pub fn timer_fired(
        &self,
        timer_id: impl Into<String>,
    ) -> Result<WorkflowEvent, TemporalCompatError> {
        let timer_id = timer_id.into();
        validate_non_empty("timer_id", &timer_id)?;
        Ok(WorkflowEvent::TimerFired {
            sequence: self.transition_sequence,
            name: timer_id,
        })
    }

    pub fn signal_received(
        &self,
        signal_name: impl Into<String>,
        payload: Option<String>,
    ) -> Result<WorkflowEvent, TemporalCompatError> {
        let signal_name = signal_name.into();
        validate_non_empty("signal_name", &signal_name)?;
        Ok(WorkflowEvent::SignalReceived {
            sequence: self.transition_sequence,
            name: signal_name,
            payload,
        })
    }

    fn activity_execution_key(&self, activity_id: &str) -> String {
        let mut out = self.execution.stable_key();
        out.push_str("/workflow-task");
        append_component(&mut out, &self.workflow_task_started_event_id.to_string());
        out.push_str("/activity");
        append_component(&mut out, activity_id);
        out
    }
}

/// Temporal `SCHEDULE_ACTIVITY_TASK` data needed by the compatibility layer.
///
/// Wire-level headers, retry policy, timeouts, and payload codecs belong in the
/// gateway crate. This core adapter keeps only data required for stable durable
/// identity and dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalActivityRequest {
    pub activity_id: String,
    pub activity_type: String,
    pub task_queue: String,
    pub input: Vec<u8>,
}

impl TemporalActivityRequest {
    pub fn new(
        activity_id: impl Into<String>,
        activity_type: impl Into<String>,
        task_queue: impl Into<String>,
        input: Vec<u8>,
    ) -> Self {
        Self {
            activity_id: activity_id.into(),
            activity_type: activity_type.into(),
            task_queue: task_queue.into(),
            input,
        }
    }

    fn validate(&self) -> Result<(), TemporalCompatError> {
        validate_non_empty("activity_id", &self.activity_id)?;
        validate_non_empty("activity_type", &self.activity_type)?;
        validate_non_empty("task_queue", &self.task_queue)?;
        Ok(())
    }

    /// Canonical request bytes bound to the durable-effect journal record.
    ///
    /// Every compatibility field that affects activity dispatch must be added
    /// here when introduced. This makes replay fail closed when a worker emits
    /// a different command for the same logical operation ID.
    pub fn durable_request_bytes(&self) -> Vec<u8> {
        let mut out = b"temporal-activity-request/v1\0".to_vec();
        append_bytes(&mut out, self.activity_id.as_bytes());
        append_bytes(&mut out, self.activity_type.as_bytes());
        append_bytes(&mut out, self.task_queue.as_bytes());
        append_bytes(&mut out, &self.input);
        out
    }
}

/// Result of translating a Temporal activity command into Nulang durability
/// semantics.
///
/// The gateway persists `record` as Prepared before dispatch. On completion
/// it commits the corresponding Completed record before acknowledging durable
/// progress to the workflow execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalActivityPlan {
    pub request: TemporalActivityRequest,
    pub record: DurableEffectRecord,
}

impl TemporalActivityPlan {
    /// Persistence envelope ready to stage in
    /// `DurableTransition::durable_effects` before external dispatch.
    pub fn prepared_persistence_record(&self) -> DurableEffectPersistenceRecord {
        DurableEffectPersistenceRecord::from_effect(self.record.clone())
    }

    /// Mark this activity completed and return the persistence envelope that
    /// should be committed before the workflow observes the result.
    pub fn complete(self, result: Vec<u8>) -> DurableEffectPersistenceRecord {
        DurableEffectPersistenceRecord::from_effect(self.record.complete(result))
    }
}

/// Core Temporal commands that can already be represented by Nulang's durable
/// primitives without importing Temporal protobuf types.
///
/// Wire adapters should preserve the original command ordering and pass the
/// zero-based ordinal to `TemporalWorkflowTaskContext::plan_command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemporalCommand {
    ScheduleActivity(TemporalActivityRequest),
    StartTimer {
        timer_id: String,
        duration_ms: u64,
    },
    CompleteWorkflow {
        result: Vec<u8>,
    },
    FailWorkflow {
        message: String,
    },
    ContinueAsNew {
        workflow_type: String,
        input: Vec<u8>,
    },
}

/// Nulang-side plan produced from a supported Temporal command.
///
/// Completion/failure/continue-as-new remain adapter actions until the
/// protocol gateway owns the corresponding workflow lifecycle records. They
/// intentionally do not create new Nulang runtime primitives.
#[derive(Debug, Clone)]
pub enum TemporalCommandPlan {
    Activity(TemporalActivityPlan),
    WorkflowEvent(WorkflowEvent),
    CompleteWorkflow {
        result: Vec<u8>,
    },
    FailWorkflow {
        message: String,
    },
    ContinueAsNew {
        workflow_type: String,
        input: Vec<u8>,
    },
}

/// Protocol-neutral subset of Temporal history consumed by the Nulang
/// compatibility worker.
///
/// The wire adapter is responsible for decoding protobuf history into this
/// representation. Event ids are retained because Temporal references earlier
/// schedule/start events by id when recording terminal activity/timer events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemporalHistoryEvent {
    WorkflowExecutionStarted {
        event_id: u64,
        workflow_type: String,
        input: Vec<u8>,
    },
    ActivityTaskScheduled {
        event_id: u64,
        activity_id: String,
        activity_type: String,
        task_queue: String,
        input: Vec<u8>,
    },
    ActivityTaskCompleted {
        event_id: u64,
        scheduled_event_id: u64,
        result: Vec<u8>,
    },
    ActivityTaskFailed {
        event_id: u64,
        scheduled_event_id: u64,
        message: String,
    },
    TimerStarted {
        event_id: u64,
        timer_id: String,
        duration_ms: u64,
    },
    TimerFired {
        event_id: u64,
        started_event_id: u64,
    },
    WorkflowExecutionSignaled {
        event_id: u64,
        signal_name: String,
        payload: Option<Vec<u8>>,
    },
    WorkflowExecutionCompleted {
        event_id: u64,
        result: Vec<u8>,
    },
    WorkflowExecutionFailed {
        event_id: u64,
        message: String,
    },
    WorkflowExecutionContinuedAsNew {
        event_id: u64,
        new_run_id: String,
    },
}

impl TemporalHistoryEvent {
    pub fn event_id(&self) -> u64 {
        match self {
            Self::WorkflowExecutionStarted { event_id, .. }
            | Self::ActivityTaskScheduled { event_id, .. }
            | Self::ActivityTaskCompleted { event_id, .. }
            | Self::ActivityTaskFailed { event_id, .. }
            | Self::TimerStarted { event_id, .. }
            | Self::TimerFired { event_id, .. }
            | Self::WorkflowExecutionSignaled { event_id, .. }
            | Self::WorkflowExecutionCompleted { event_id, .. }
            | Self::WorkflowExecutionFailed { event_id, .. }
            | Self::WorkflowExecutionContinuedAsNew { event_id, .. } => *event_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemporalReplayedActivityState {
    Scheduled,
    Completed(Vec<u8>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalReplayedActivity {
    pub scheduled_event_id: u64,
    pub activity_id: String,
    pub activity_type: String,
    pub task_queue: String,
    pub input: Vec<u8>,
    pub state: TemporalReplayedActivityState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalReplayedTimer {
    pub started_event_id: u64,
    pub timer_id: String,
    pub duration_ms: u64,
    pub fired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalReplayedSignal {
    pub event_id: u64,
    pub signal_name: String,
    pub payload: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemporalTerminalState {
    Completed(Vec<u8>),
    Failed(String),
    ContinuedAsNew { new_run_id: String },
}

/// Deterministic projection of the supported Temporal history subset.
///
/// This state is adapter-owned. It exists to validate Temporal replay and to
/// drive the compatibility worker; it is not a replacement for Nulang's own
/// durable transition journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalReplayState {
    pub workflow_type: String,
    pub input: Vec<u8>,
    pub last_event_id: u64,
    pub activities: BTreeMap<u64, TemporalReplayedActivity>,
    pub timers: BTreeMap<u64, TemporalReplayedTimer>,
    pub signals: Vec<TemporalReplayedSignal>,
    pub terminal: Option<TemporalTerminalState>,
}

impl TemporalReplayState {
    pub fn replay(history: &[TemporalHistoryEvent]) -> Result<Self, TemporalCompatError> {
        let mut workflow_type = None;
        let mut input = Vec::new();
        let mut last_event_id = 0;
        let mut activities = BTreeMap::new();
        let mut timers = BTreeMap::new();
        let mut signals = Vec::new();
        let mut terminal = None;

        for event in history {
            let event_id = event.event_id();
            if event_id == 0 || event_id <= last_event_id {
                return Err(TemporalCompatError::NonMonotonicHistory {
                    previous: last_event_id,
                    current: event_id,
                });
            }
            if terminal.is_some() {
                return Err(TemporalCompatError::EventAfterTerminal { event_id });
            }

            match event {
                TemporalHistoryEvent::WorkflowExecutionStarted {
                    workflow_type: ty,
                    input: start_input,
                    ..
                } => {
                    if workflow_type.is_some() {
                        return Err(TemporalCompatError::DuplicateWorkflowStart);
                    }
                    validate_non_empty("workflow_type", ty)?;
                    workflow_type = Some(ty.clone());
                    input = start_input.clone();
                }
                TemporalHistoryEvent::ActivityTaskScheduled {
                    event_id,
                    activity_id,
                    activity_type,
                    task_queue,
                    input,
                } => {
                    validate_non_empty("activity_id", activity_id)?;
                    validate_non_empty("activity_type", activity_type)?;
                    validate_non_empty("task_queue", task_queue)?;
                    activities.insert(
                        *event_id,
                        TemporalReplayedActivity {
                            scheduled_event_id: *event_id,
                            activity_id: activity_id.clone(),
                            activity_type: activity_type.clone(),
                            task_queue: task_queue.clone(),
                            input: input.clone(),
                            state: TemporalReplayedActivityState::Scheduled,
                        },
                    );
                }
                TemporalHistoryEvent::ActivityTaskCompleted {
                    scheduled_event_id,
                    result,
                    ..
                } => {
                    let activity = activities.get_mut(scheduled_event_id).ok_or(
                        TemporalCompatError::UnknownActivityScheduledEvent(*scheduled_event_id),
                    )?;
                    if !matches!(activity.state, TemporalReplayedActivityState::Scheduled) {
                        return Err(TemporalCompatError::ActivityAlreadyTerminal(
                            *scheduled_event_id,
                        ));
                    }
                    activity.state = TemporalReplayedActivityState::Completed(result.clone());
                }
                TemporalHistoryEvent::ActivityTaskFailed {
                    scheduled_event_id,
                    message,
                    ..
                } => {
                    let activity = activities.get_mut(scheduled_event_id).ok_or(
                        TemporalCompatError::UnknownActivityScheduledEvent(*scheduled_event_id),
                    )?;
                    if !matches!(activity.state, TemporalReplayedActivityState::Scheduled) {
                        return Err(TemporalCompatError::ActivityAlreadyTerminal(
                            *scheduled_event_id,
                        ));
                    }
                    activity.state = TemporalReplayedActivityState::Failed(message.clone());
                }
                TemporalHistoryEvent::TimerStarted {
                    event_id,
                    timer_id,
                    duration_ms,
                } => {
                    validate_non_empty("timer_id", timer_id)?;
                    timers.insert(
                        *event_id,
                        TemporalReplayedTimer {
                            started_event_id: *event_id,
                            timer_id: timer_id.clone(),
                            duration_ms: *duration_ms,
                            fired: false,
                        },
                    );
                }
                TemporalHistoryEvent::TimerFired {
                    started_event_id, ..
                } => {
                    let timer = timers.get_mut(started_event_id).ok_or(
                        TemporalCompatError::UnknownTimerStartedEvent(*started_event_id),
                    )?;
                    if timer.fired {
                        return Err(TemporalCompatError::TimerAlreadyFired(*started_event_id));
                    }
                    timer.fired = true;
                }
                TemporalHistoryEvent::WorkflowExecutionSignaled {
                    event_id,
                    signal_name,
                    payload,
                } => {
                    validate_non_empty("signal_name", signal_name)?;
                    signals.push(TemporalReplayedSignal {
                        event_id: *event_id,
                        signal_name: signal_name.clone(),
                        payload: payload.clone(),
                    });
                }
                TemporalHistoryEvent::WorkflowExecutionCompleted { result, .. } => {
                    terminal = Some(TemporalTerminalState::Completed(result.clone()));
                }
                TemporalHistoryEvent::WorkflowExecutionFailed { message, .. } => {
                    terminal = Some(TemporalTerminalState::Failed(message.clone()));
                }
                TemporalHistoryEvent::WorkflowExecutionContinuedAsNew { new_run_id, .. } => {
                    validate_non_empty("new_run_id", new_run_id)?;
                    terminal = Some(TemporalTerminalState::ContinuedAsNew {
                        new_run_id: new_run_id.clone(),
                    });
                }
            }

            last_event_id = event_id;
        }

        let workflow_type =
            workflow_type.ok_or(TemporalCompatError::HistoryMissingWorkflowStart)?;
        Ok(Self {
            workflow_type,
            input,
            last_event_id,
            activities,
            timers,
            signals,
            terminal,
        })
    }
}

/// One workflow task as presented to the protocol-neutral Nulang worker core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalWorkflowTask {
    pub execution: TemporalWorkflowExecution,
    pub nulang_actor_id: u64,
    pub workflow_task_started_event_id: u64,
    pub transition_sequence: u64,
    pub history: Vec<TemporalHistoryEvent>,
}

/// Validated/replayed workflow task ready for command generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalPreparedWorkflowTask {
    pub context: TemporalWorkflowTaskContext,
    pub replay: TemporalReplayState,
}

impl TemporalPreparedWorkflowTask {
    /// Translate a complete ordered command list into Nulang compatibility
    /// plans. The command index is the stable Temporal command ordinal.
    pub fn plan_commands(
        &self,
        commands: Vec<TemporalCommand>,
    ) -> Result<Vec<TemporalCommandPlan>, TemporalCompatError> {
        commands
            .into_iter()
            .enumerate()
            .map(|(ordinal, command)| {
                let ordinal =
                    u32::try_from(ordinal).map_err(|_| TemporalCompatError::TooManyCommands)?;
                self.context.plan_command(ordinal, command)
            })
            .collect()
    }
}

/// Protocol-independent worker core shared by a native Temporal worker
/// transport and Nulang Cloud's managed compatibility gateway.
#[derive(Debug, Default, Clone, Copy)]
pub struct TemporalWorkerCore;

impl TemporalWorkerCore {
    pub fn prepare_task(
        task: TemporalWorkflowTask,
    ) -> Result<TemporalPreparedWorkflowTask, TemporalCompatError> {
        let replay = TemporalReplayState::replay(&task.history)?;
        let context = TemporalWorkflowTaskContext::new(
            task.execution,
            task.nulang_actor_id,
            task.workflow_task_started_event_id,
            task.transition_sequence,
        )?;
        Ok(TemporalPreparedWorkflowTask { context, replay })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemporalCompatError {
    EmptyField(&'static str),
    ZeroWorkflowTaskStartedEventId,
    ZeroTransitionSequence,
    NonMonotonicHistory { previous: u64, current: u64 },
    HistoryMissingWorkflowStart,
    DuplicateWorkflowStart,
    UnknownActivityScheduledEvent(u64),
    ActivityAlreadyTerminal(u64),
    UnknownTimerStartedEvent(u64),
    TimerAlreadyFired(u64),
    EventAfterTerminal { event_id: u64 },
    TooManyCommands,
}

impl fmt::Display for TemporalCompatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(field) => write!(f, "Temporal compatibility field {field} cannot be empty"),
            Self::ZeroWorkflowTaskStartedEventId => {
                write!(f, "Temporal workflow task started event id must be non-zero")
            }
            Self::ZeroTransitionSequence => {
                write!(f, "Nulang durable transition sequence must be non-zero")
            }
            Self::NonMonotonicHistory { previous, current } => write!(
                f,
                "Temporal history event ids must increase strictly: previous {previous}, current {current}"
            ),
            Self::HistoryMissingWorkflowStart => {
                write!(f, "Temporal history is missing WorkflowExecutionStarted")
            }
            Self::DuplicateWorkflowStart => {
                write!(f, "Temporal history contains more than one WorkflowExecutionStarted")
            }
            Self::UnknownActivityScheduledEvent(event_id) => write!(
                f,
                "Temporal activity terminal event references unknown scheduled event {event_id}"
            ),
            Self::ActivityAlreadyTerminal(event_id) => write!(
                f,
                "Temporal activity scheduled at event {event_id} is already terminal"
            ),
            Self::UnknownTimerStartedEvent(event_id) => write!(
                f,
                "Temporal TimerFired references unknown TimerStarted event {event_id}"
            ),
            Self::TimerAlreadyFired(event_id) => {
                write!(f, "Temporal timer started at event {event_id} already fired")
            }
            Self::EventAfterTerminal { event_id } => write!(
                f,
                "Temporal history event {event_id} appears after workflow terminal state"
            ),
            Self::TooManyCommands => {
                write!(f, "Temporal workflow task contains more than u32::MAX commands")
            }
        }
    }
}

impl std::error::Error for TemporalCompatError {}

fn validate_non_empty(field: &'static str, value: &str) -> Result<(), TemporalCompatError> {
    if value.is_empty() {
        Err(TemporalCompatError::EmptyField(field))
    } else {
        Ok(())
    }
}

fn append_component(out: &mut String, value: &str) {
    write!(out, "/{}:{value}", value.len()).expect("writing to String cannot fail");
}

fn append_bytes(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u64).to_le_bytes());
    out.extend_from_slice(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_effect::DurableEffectRecoveryAction;

    fn context() -> TemporalWorkflowTaskContext {
        TemporalWorkflowTaskContext::new(
            TemporalWorkflowExecution::new("payments", "order-42", "run-abc").unwrap(),
            77,
            101,
            9,
        )
        .unwrap()
    }

    fn activity() -> TemporalActivityRequest {
        TemporalActivityRequest::new(
            "charge-card",
            "ChargeCard",
            "payments",
            br#"{"amount":4200}"#.to_vec(),
        )
    }

    #[test]
    fn compatibility_contract_version_is_explicit() {
        assert_eq!(TEMPORAL_COMPATIBILITY_CONTRACT_VERSION, 1);
    }

    #[test]
    fn execution_key_is_collision_resistant_for_separator_content() {
        let a = TemporalWorkflowExecution::new("a/b", "c", "d").unwrap();
        let b = TemporalWorkflowExecution::new("a", "b/c", "d").unwrap();
        assert_ne!(a.stable_key(), b.stable_key());
    }

    #[test]
    fn replay_of_same_activity_derives_same_durable_effect_id() {
        let ctx = context();
        let first = ctx.prepare_activity(0, activity()).unwrap();
        let replay = ctx.prepare_activity(0, activity()).unwrap();

        assert_eq!(first.record.spec().id, replay.record.spec().id);
        let durable_request = first.request.durable_request_bytes();
        assert_eq!(
            first
                .record
                .recovery_action_for_request(&durable_request)
                .unwrap(),
            DurableEffectRecoveryAction::RetryAtLeastOnce {
                operation_id: first.record.spec().id,
            }
        );
    }

    #[test]
    fn different_workflow_task_or_activity_id_changes_effect_identity() {
        let first_ctx = context();
        let later_ctx = TemporalWorkflowTaskContext::new(
            first_ctx.execution.clone(),
            first_ctx.nulang_actor_id,
            102,
            10,
        )
        .unwrap();

        let first = first_ctx.prepare_activity(0, activity()).unwrap();
        let later = later_ctx.prepare_activity(0, activity()).unwrap();

        let mut other_activity = activity();
        other_activity.activity_id = "charge-card-2".into();
        let other = first_ctx.prepare_activity(0, other_activity).unwrap();

        assert_ne!(first.record.spec().id, later.record.spec().id);
        assert_ne!(first.record.spec().id, other.record.spec().id);
    }

    #[test]
    fn deduplicated_activity_uses_nulang_stable_operation_id() {
        let plan = context()
            .prepare_activity_with_delivery(
                3,
                activity(),
                DeliverySemantics::EffectivelyOnceWithDeduplication,
            )
            .unwrap();
        let id = plan.record.spec().id;

        let durable_request = plan.request.durable_request_bytes();
        assert_eq!(
            plan.record
                .recovery_action_for_request(&durable_request)
                .unwrap(),
            DurableEffectRecoveryAction::RetryWithDeduplication { operation_id: id }
        );
        assert_eq!(id.idempotency_key(), plan.record.spec().id.to_string());
    }

    #[test]
    fn replay_with_changed_dispatch_metadata_fails_closed() {
        let ctx = context();
        let plan = ctx.prepare_activity(0, activity()).unwrap();

        let mut changed = activity();
        changed.task_queue = "other-payments".into();

        assert!(plan
            .record
            .recovery_action_for_request(&changed.durable_request_bytes())
            .is_err());
    }

    #[test]
    fn command_planner_preserves_global_command_ordinal_in_activity_identity() {
        let ctx = context();

        let first = match ctx
            .plan_command(0, TemporalCommand::ScheduleActivity(activity()))
            .unwrap()
        {
            TemporalCommandPlan::Activity(plan) => plan,
            _ => panic!("expected activity plan"),
        };
        let reordered = match ctx
            .plan_command(1, TemporalCommand::ScheduleActivity(activity()))
            .unwrap()
        {
            TemporalCommandPlan::Activity(plan) => plan,
            _ => panic!("expected activity plan"),
        };

        assert_ne!(first.record.spec().id, reordered.record.spec().id);
    }

    #[test]
    fn activity_plan_produces_prepared_and_completed_persistence_records() {
        let plan = context().prepare_activity(0, activity()).unwrap();
        let prepared = plan.prepared_persistence_record();
        assert_eq!(prepared.effect().spec().id, plan.record.spec().id);

        let completed = plan.complete(b"charged".to_vec());
        match completed.effect() {
            DurableEffectRecord::Completed { result, .. } => {
                assert_eq!(result.as_slice(), b"charged");
            }
            _ => panic!("expected completed durable effect"),
        }
    }

    #[test]
    fn command_planner_keeps_lifecycle_actions_outside_runtime_primitives() {
        let ctx = context();

        match ctx
            .plan_command(
                0,
                TemporalCommand::CompleteWorkflow {
                    result: b"done".to_vec(),
                },
            )
            .unwrap()
        {
            TemporalCommandPlan::CompleteWorkflow { result } => {
                assert_eq!(result, b"done");
            }
            _ => panic!("expected complete workflow plan"),
        }

        match ctx
            .plan_command(
                1,
                TemporalCommand::ContinueAsNew {
                    workflow_type: "OrderWorkflow".into(),
                    input: b"next".to_vec(),
                },
            )
            .unwrap()
        {
            TemporalCommandPlan::ContinueAsNew {
                workflow_type,
                input,
            } => {
                assert_eq!(workflow_type, "OrderWorkflow");
                assert_eq!(input.as_slice(), b"next");
            }
            _ => panic!("expected continue-as-new plan"),
        }

        assert_eq!(
            ctx.plan_command(
                2,
                TemporalCommand::ContinueAsNew {
                    workflow_type: String::new(),
                    input: Vec::new(),
                },
            )
            .unwrap_err(),
            TemporalCompatError::EmptyField("workflow_type")
        );
    }

    #[test]
    fn timer_and_signal_events_use_atomic_transition_sequence() {
        let ctx = context();

        match ctx.timer_started("retry", 1_500).unwrap() {
            WorkflowEvent::TimerSet {
                sequence,
                name,
                duration_ms,
            } => {
                assert_eq!(sequence, 9);
                assert_eq!(name, "retry");
                assert_eq!(duration_ms, 1_500);
            }
            _ => panic!("expected TimerSet"),
        }

        match ctx.signal_received("approved", Some("yes".into())).unwrap() {
            WorkflowEvent::SignalReceived {
                sequence,
                name,
                payload,
            } => {
                assert_eq!(sequence, 9);
                assert_eq!(name, "approved");
                assert_eq!(payload.as_deref(), Some("yes"));
            }
            _ => panic!("expected SignalReceived"),
        }
    }

    fn replay_history() -> Vec<TemporalHistoryEvent> {
        vec![
            TemporalHistoryEvent::WorkflowExecutionStarted {
                event_id: 1,
                workflow_type: "OrderWorkflow".into(),
                input: b"order-42".to_vec(),
            },
            TemporalHistoryEvent::ActivityTaskScheduled {
                event_id: 2,
                activity_id: "reserve".into(),
                activity_type: "ReserveInventory".into(),
                task_queue: "orders".into(),
                input: b"sku-1".to_vec(),
            },
            TemporalHistoryEvent::ActivityTaskCompleted {
                event_id: 3,
                scheduled_event_id: 2,
                result: b"reserved".to_vec(),
            },
            TemporalHistoryEvent::TimerStarted {
                event_id: 4,
                timer_id: "payment-timeout".into(),
                duration_ms: 30_000,
            },
            TemporalHistoryEvent::TimerFired {
                event_id: 5,
                started_event_id: 4,
            },
            TemporalHistoryEvent::WorkflowExecutionSignaled {
                event_id: 6,
                signal_name: "approved".into(),
                payload: Some(b"yes".to_vec()),
            },
        ]
    }

    #[test]
    fn replay_projects_supported_history_deterministically() {
        let state = TemporalReplayState::replay(&replay_history()).unwrap();
        assert_eq!(state.workflow_type, "OrderWorkflow");
        assert_eq!(state.last_event_id, 6);
        assert_eq!(state.activities.len(), 1);
        assert_eq!(state.timers.len(), 1);
        assert_eq!(state.signals.len(), 1);
        assert!(matches!(
            state.activities.get(&2).unwrap().state,
            TemporalReplayedActivityState::Completed(ref result)
                if result.as_slice() == b"reserved"
        ));
        assert!(state.timers.get(&4).unwrap().fired);
    }

    #[test]
    fn replay_rejects_unknown_activity_completion_and_non_monotonic_ids() {
        let unknown = vec![
            TemporalHistoryEvent::WorkflowExecutionStarted {
                event_id: 1,
                workflow_type: "OrderWorkflow".into(),
                input: Vec::new(),
            },
            TemporalHistoryEvent::ActivityTaskCompleted {
                event_id: 2,
                scheduled_event_id: 999,
                result: Vec::new(),
            },
        ];
        assert_eq!(
            TemporalReplayState::replay(&unknown).unwrap_err(),
            TemporalCompatError::UnknownActivityScheduledEvent(999)
        );

        let non_monotonic = vec![
            TemporalHistoryEvent::WorkflowExecutionStarted {
                event_id: 2,
                workflow_type: "OrderWorkflow".into(),
                input: Vec::new(),
            },
            TemporalHistoryEvent::WorkflowExecutionSignaled {
                event_id: 2,
                signal_name: "duplicate".into(),
                payload: None,
            },
        ];
        assert_eq!(
            TemporalReplayState::replay(&non_monotonic).unwrap_err(),
            TemporalCompatError::NonMonotonicHistory {
                previous: 2,
                current: 2,
            }
        );
    }

    #[test]
    fn replay_rejects_events_after_terminal_history() {
        let history = vec![
            TemporalHistoryEvent::WorkflowExecutionStarted {
                event_id: 1,
                workflow_type: "OrderWorkflow".into(),
                input: Vec::new(),
            },
            TemporalHistoryEvent::WorkflowExecutionCompleted {
                event_id: 2,
                result: b"done".to_vec(),
            },
            TemporalHistoryEvent::WorkflowExecutionSignaled {
                event_id: 3,
                signal_name: "late".into(),
                payload: None,
            },
        ];
        assert_eq!(
            TemporalReplayState::replay(&history).unwrap_err(),
            TemporalCompatError::EventAfterTerminal { event_id: 3 }
        );
    }

    #[test]
    fn worker_core_replays_task_and_plans_ordered_commands() {
        let task = TemporalWorkflowTask {
            execution: TemporalWorkflowExecution::new("payments", "order-42", "run-abc").unwrap(),
            nulang_actor_id: 77,
            workflow_task_started_event_id: 101,
            transition_sequence: 9,
            history: replay_history(),
        };
        let prepared = TemporalWorkerCore::prepare_task(task).unwrap();
        assert_eq!(prepared.replay.last_event_id, 6);

        let plans = prepared
            .plan_commands(vec![
                TemporalCommand::StartTimer {
                    timer_id: "retry".into(),
                    duration_ms: 1_000,
                },
                TemporalCommand::ScheduleActivity(activity()),
            ])
            .unwrap();
        assert_eq!(plans.len(), 2);
        assert!(matches!(plans[0], TemporalCommandPlan::WorkflowEvent(_)));
        match &plans[1] {
            TemporalCommandPlan::Activity(plan) => {
                let ordinal_one = context().prepare_activity(1, activity()).unwrap();
                assert_eq!(plan.record.spec().id, ordinal_one.record.spec().id);
            }
            _ => panic!("expected activity plan"),
        }
    }

    #[test]
    fn invalid_ambiguous_identity_is_rejected() {
        assert_eq!(
            TemporalWorkflowExecution::new("default", "workflow", ""),
            Err(TemporalCompatError::EmptyField("run_id"))
        );
        assert_eq!(
            TemporalWorkflowTaskContext::new(
                TemporalWorkflowExecution::new("default", "workflow", "run").unwrap(),
                1,
                0,
                1,
            ),
            Err(TemporalCompatError::ZeroWorkflowTaskStartedEventId)
        );
    }
}
