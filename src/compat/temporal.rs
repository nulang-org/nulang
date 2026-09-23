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
use crate::runtime::persistence::WorkflowEvent;
use std::fmt;
use std::fmt::Write as _;

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
            TemporalCommand::ScheduleActivity(request) => Ok(
                TemporalCommandPlan::Activity(self.prepare_activity(command_ordinal, request)?),
            ),
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
        append_component(
            &mut out,
            &self.workflow_task_started_event_id.to_string(),
        );
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemporalCompatError {
    EmptyField(&'static str),
    ZeroWorkflowTaskStartedEventId,
    ZeroTransitionSequence,
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
