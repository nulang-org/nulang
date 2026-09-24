//! Canonical staging boundary for one runtime turn.
//!
//! Runtime code stages all Nulang-owned durable mutations in a `TurnOutcome`
//! and lowers that outcome to exactly one `DurableTransition`.  Backends with
//! atomic-transition support commit it all-or-nothing.  During the RFC 0022
//! migration, legacy backends may use the compatibility fallback below for
//! snapshot/journal/workflow/domain records only; durable effects and outbox
//! messages never fall back because doing so would create false atomicity.

use std::io;

use crate::durable_effect_persistence::DurableEffectPersistenceRecord;

use super::persistence::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, EventEntry, JournalEntry,
    PersistenceStore, WorkflowEvent, DURABLE_TRANSITION_VERSION,
};

/// An incoming command staged before its durable sequence is assigned.
///
/// The transition boundary owns sequence allocation, so runtime execution code
/// should not manufacture a `JournalEntry` early just to fill its sequence.
#[derive(Debug, Clone)]
pub(crate) struct StagedCommand {
    pub(crate) behavior_id: u16,
    pub(crate) payload: Vec<super::persistence::PersistedValue>,
}

/// Everything one execution turn wants to make durable.
///
/// This type deliberately contains semantic outputs rather than backend
/// operations. Storage backends see one `DurableTransition`; they do not get
/// called piecemeal by workflow/runtime code.
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnOutcome {
    pub(crate) command: Option<StagedCommand>,
    pub(crate) workflow_events: Vec<WorkflowEvent>,
    pub(crate) domain_events: Vec<EventEntry>,
    pub(crate) durable_effects: Vec<DurableEffectPersistenceRecord>,
    pub(crate) outbox: Vec<DurableOutboxMessage>,
}

impl TurnOutcome {
    pub(crate) fn workflow_event(event: WorkflowEvent) -> Self {
        Self {
            workflow_events: vec![event],
            ..Self::default()
        }
    }

    pub(crate) fn into_transition(
        self,
        actor_id: u64,
        activation_epoch: u64,
        expected_previous_sequence: u64,
        snapshot: Option<ActorSnapshot>,
    ) -> io::Result<DurableTransition> {
        let sequence = expected_previous_sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "durable transition sequence overflow",
            )
        })?;

        let command = self.command.map(|command| JournalEntry {
            sequence,
            behavior_id: command.behavior_id,
            payload: command.payload,
        });

        Ok(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch,
            sequence,
            expected_previous_sequence,
            command,
            snapshot,
            workflow_events: self.workflow_events,
            domain_events: self.domain_events,
            durable_effects: self.durable_effects,
            outbox: self.outbox,
            inbox: Vec::new(),
        })
    }
}

/// Commit one staged turn through the RFC 0022 atomic contract.
///
/// Every built-in persistence backend implements `commit_transition`.
/// Custom stores that do not support the contract fail closed with
/// `Unsupported` instead of silently degrading to several independent
/// writes with weaker crash semantics.
pub(crate) fn commit_turn(
    store: &mut dyn PersistenceStore,
    transition: DurableTransition,
) -> io::Result<()> {
    transition.validate_structure()?;
    store.commit_transition(transition).map(|_| ())
}
