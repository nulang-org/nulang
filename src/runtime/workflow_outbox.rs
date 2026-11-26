//! Opt-in same-store workflow outbox delivery on the owning runtime shard.

use super::{
    workflow, ActorBackend, ActorRunState, ActorState, DurableOutboxId, Message, MessagePayload,
    MessagePriority, Runtime, WorkflowActivationId,
};
use crate::primitives::ActorRole;
use std::io;

/// A dispatch result describes receiver admission, not handler completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowOutboxDispatch {
    Admitted(WorkflowActivationId),
    AlreadyAdmitted(WorkflowActivationId),
    /// Receiver is unavailable, not qualified, or has work in progress.
    Deferred,
}

impl Runtime {
    pub(crate) fn is_workflow_outbox_destination(&self, destination: u64, behavior: u16) -> bool {
        let behavior = behavior as usize;
        let valid = self.actors.get(&destination).is_some_and(|actor| {
            actor.persistent
                && actor.backend == ActorBackend::Native
                && actor.state != ActorState::Terminated
                && matches!(actor.role(), Ok(ActorRole::Workflow))
                && !actor.is_hibernated()
        }) && self.has_native_handler(destination, behavior)
            && !self.is_internal_behavior(destination, behavior)
            && !self.migrated_actors.contains_key(&destination)
            && !self.actor_grain_id.contains_key(&destination)
            && destination % self.shard_count as u64 == self.shard_idx as u64
            && self.persistence.load_snapshot(destination).is_some();
        #[cfg(feature = "native-codegen")]
        let valid = valid
            && self
                .actors
                .get(&destination)
                .and_then(|actor| actor.aot_targets.get(behavior))
                .is_none_or(|target| target.is_none());
        valid
    }

    pub(crate) fn admitted_workflow_handler_ready(&self, actor_id: u64) -> bool {
        self.actors
            .get(&actor_id)
            .and_then(|actor| actor.mailbox.admitted_workflow_behavior())
            .is_none_or(|behavior| {
                self.has_native_handler(actor_id, behavior as usize)
                    || self.has_bytecode_handler(actor_id, behavior as usize)
            })
    }

    /// Discover up to `limit` unadmitted tracked outbox messages for one sender.
    /// The exclusive (sequence, ordinal) cursor allows hosts to skip a blocked
    /// receiver during a scan and revisit deferred messages on the next scan.
    pub fn pending_workflow_outbox(
        &self,
        sender: u64,
        after: Option<(u64, u32)>,
        limit: usize,
    ) -> io::Result<Vec<DurableOutboxId>> {
        if sender % self.shard_count as u64 != self.shard_idx as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow outbox sender is not on this shard",
            ));
        }
        self.persistence
            .pending_workflow_outbox(sender, after, limit)
    }

    /// Admit one committed tracked message, then make the original command
    /// runnable on this shard. Only idle ordinary Rust workflow receivers are
    /// eligible. Call between scheduler turns, not from an executing handler.
    ///
    /// A retry returns the original receipt; it never creates another command.
    /// After process loss, recover the receiver and register its native
    /// handlers before scheduling; an open admission replays from the journal.
    /// An error after an uncertain commit requires checking the receipt and
    /// recovering the receiver, not sending an ordinary message.
    pub fn dispatch_workflow_outbox(
        &mut self,
        id: DurableOutboxId,
    ) -> io::Result<WorkflowOutboxDispatch> {
        if self.current_actor.is_some() {
            return Ok(WorkflowOutboxDispatch::Deferred);
        }
        if id.sender_actor_id % self.shard_count as u64 != self.shard_idx as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow outbox sender is not on this shard",
            ));
        }
        let message = self
            .persistence
            .load_workflow_outbox_message(id)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no committed tracked workflow outbox message",
                )
            })?;
        let destination = message.destination_actor_id;
        if let Some(activation) = self.persistence.load_workflow_inbox_admission(id)? {
            // A recovered command may have been parked pending host handler
            // registration. Wake only that original command, never enqueue a
            // copy. A missing live command requires explicit actor recovery.
            if self.is_workflow_outbox_destination(destination, message.behavior_id)
                && self.actors.get(&destination).is_some_and(|actor| {
                    actor.current_workflow_activation == Some(activation)
                        && actor.mailbox.admitted_workflow_behavior() == Some(message.behavior_id)
                })
                && self.admitted_workflow_handler_ready(destination)
            {
                self.enqueue_actor(destination);
            }
            return Ok(WorkflowOutboxDispatch::AlreadyAdmitted(activation));
        }
        if !self.is_workflow_outbox_destination(destination, message.behavior_id)
            || !self.actors.get(&destination).is_some_and(|actor| {
                actor.run_state != ActorRunState::Running
                    && matches!(
                        actor.state,
                        ActorState::Created | ActorState::Running | ActorState::Waiting
                    )
                    && actor.mailbox.is_empty()
                    && actor.current_workflow_activation.is_none()
                    && actor.waiting_signal.is_none()
                    && !actor.is_mid_execution()
            })
            || self
                .persistence
                .load_open_workflow_command(destination)?
                .is_some()
        {
            return Ok(WorkflowOutboxDispatch::Deferred);
        }

        // Do not execute on an in-memory state older than its safe checkpoint.
        let snapshot = self.persistence.load_snapshot(destination).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "receiver snapshot disappeared")
        })?;
        let actor = &self.actors[&destination];
        if actor.sequence != snapshot.sequence
            || actor.activation_epoch != snapshot.activation_epoch
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow receiver must be recovered from its current snapshot",
            ));
        }

        // Materialize in the receiver heap before admission; reject an
        // unrepresentable value instead of durably accepting a different one.
        let actor = self
            .actors
            .get_mut(&destination)
            .expect("qualified live receiver");
        let payload: Vec<_> = message
            .payload
            .iter()
            .map(|value| value.to_value_on_heap(actor))
            .collect();
        for (original, value) in message.payload.iter().zip(&payload) {
            if serde_json::to_vec(original).map_err(io::Error::other)?
                != serde_json::to_vec(&actor.persist_value(value)).map_err(io::Error::other)?
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "workflow outbox payload is not representable in the receiver",
                ));
            }
        }
        let activation = workflow::commit_workflow_command_with_inbox(
            self,
            destination,
            message.behavior_id,
            message.payload,
            Some(id),
        )?;
        // No ordinary send occurs. This slot has precedence over later
        // system messages and cannot fail because of mailbox backpressure.
        self.actors
            .get_mut(&destination)
            .expect("admitted live receiver")
            .mailbox
            .push_admitted_workflow(Message {
                behavior_id: message.behavior_id,
                payload: MessagePayload::from_vec(payload),
                // Same as journal replay: source identity lives in the inbox.
                sender: 0,
                priority: MessagePriority::Normal,
                trace_id: None,
            });
        self.enqueue_actor(destination);
        Ok(WorkflowOutboxDispatch::Admitted(activation))
    }
}
