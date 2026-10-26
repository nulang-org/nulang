//! Read-only projection of replay-identifiable workflow outputs from the
//! existing durable journal. This is NOT an external delivery or ack service:
//! output delivery must be performed by an idempotent external consumer.

use crate::durable_effect::DurableEffectId;
use crate::runtime::{PersistedValue, PersistenceStore, WorkflowEvent, WorkflowReplayEventId};
use std::collections::HashSet;

/// Dedicated journal event name for replay-stable, textual workflow outputs.
pub const WORKFLOW_OUTPUT_EVENT: &str = "WorkflowOutput";

/// Immutable evidence that a particular workflow activation recorded output.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowOutputReceipt {
    pub sequence: u64,
    pub replay_id: WorkflowReplayEventId,
    pub value: PersistedValue,
}

impl WorkflowOutputReceipt {
    /// Stable cross-retry key for a consumer which *actually* deduplicates it.
    /// This method does not deliver, acknowledge, or mark outputs consumed.
    pub fn idempotency_key(&self) -> String {
        DurableEffectId::derive(
            self.replay_id.activation.actor_id,
            &format!(
                "workflow-activation:{}",
                self.replay_id.activation.command_sequence
            ),
            self.replay_id.ordinal,
            WORKFLOW_OUTPUT_EVENT,
        )
        .idempotency_key()
    }
}

/// Fail closed rather than silently showing partial or ambiguous replay data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowOutputError {
    InvalidLimit,
    Storage(String),
    InvalidSequence { previous: u64, current: u64 },
    MissingReplayIdentity { sequence: u64 },
    WrongActor { expected: u64, actual: u64 },
    DuplicateReplayIdentity { replay_id: WorkflowReplayEventId },
    InvalidOutputPayload { sequence: u64 },
}

impl std::fmt::Display for WorkflowOutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid durable workflow output: {self:?}")
    }
}
impl std::error::Error for WorkflowOutputError {}

/// Page through *recorded* workflow outputs after a journal sequence.
///
/// The entire returned journal is validated before any output is exposed, so
/// a conflicting replay identity cannot be hidden beyond the page boundary.
/// Only a `WorkflowOutput(String)` custom event with a replay identity counts
/// as an output. Legacy events lacking identity are **not** replay-safe.
/// No stdout, network, or provider side effect occurs while reading.
pub fn read_workflow_output_receipts(
    store: &dyn PersistenceStore,
    actor_id: u64,
    after_sequence: u64,
    max_items: usize,
) -> Result<Vec<WorkflowOutputReceipt>, WorkflowOutputError> {
    if max_items == 0 || max_items > 1024 {
        return Err(WorkflowOutputError::InvalidLimit);
    }

    let mut last_sequence = None;
    let mut seen = HashSet::<WorkflowReplayEventId>::new();
    let mut page = Vec::new();
    for event in store
        .try_read_workflow_events(actor_id)
        .map_err(|error| WorkflowOutputError::Storage(error.to_string()))?
    {
        let sequence = event.sequence();
        if let Some(previous) = last_sequence {
            if sequence <= previous {
                return Err(WorkflowOutputError::InvalidSequence {
                    previous,
                    current: sequence,
                });
            }
        }
        last_sequence = Some(sequence);
        if let Some(replay_id) = event.replay_id() {
            if replay_id.activation.actor_id != actor_id {
                return Err(WorkflowOutputError::WrongActor {
                    expected: actor_id,
                    actual: replay_id.activation.actor_id,
                });
            }
            if !seen.insert(replay_id) {
                return Err(WorkflowOutputError::DuplicateReplayIdentity { replay_id });
            }
        }
        if let WorkflowEvent::Custom {
            replay_id,
            name,
            args,
            ..
        } = event
        {
            if name != WORKFLOW_OUTPUT_EVENT {
                continue;
            }
            let replay_id =
                replay_id.ok_or(WorkflowOutputError::MissingReplayIdentity { sequence })?;
            if args.len() != 1 || !matches!(args.first(), Some(PersistedValue::String(_))) {
                return Err(WorkflowOutputError::InvalidOutputPayload { sequence });
            }
            if sequence > after_sequence && page.len() < max_items {
                page.push(WorkflowOutputReceipt {
                    sequence,
                    replay_id,
                    value: args.into_iter().next().expect("verified single argument"),
                });
            }
        }
    }
    Ok(page)
}
