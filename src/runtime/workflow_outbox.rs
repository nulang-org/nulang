//! Opt-in same-store workflow outbox delivery on the owning runtime shard.

use super::{
    workflow, ActorBackend, ActorRunState, ActorState, DurableOutboxId, Message, MessagePayload,
    MessagePriority, Runtime, WorkflowActivationId, MAX_WORKFLOW_OUTBOX_PAGE,
};
use crate::primitives::ActorRole;
use std::io;
use std::time::{Duration, Instant};

/// A dispatch result describes receiver admission, not handler completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowOutboxDispatch {
    Admitted(WorkflowActivationId),
    AlreadyAdmitted(WorkflowActivationId),
    /// Receiver is unavailable, not qualified, or has work in progress.
    Deferred,
}

/// Opt-in delivery cadence. A scheduler turn attempts at most one page;
/// quiescent calls scan at most four pages without executing an actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowOutboxPumpConfig {
    pub page_size: usize,
    pub retry_interval: Duration,
}

impl Default for WorkflowOutboxPumpConfig {
    fn default() -> Self {
        Self {
            page_size: 64,
            retry_interval: Duration::from_millis(100),
        }
    }
}

/// Counts admission outcomes; none of these counts implies handler completion.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowOutboxPumpReport {
    pub examined: usize,
    pub admitted: usize,
    pub already_admitted: usize,
    pub deferred: usize,
    pub failed: usize,
    pub sweep_complete: bool,
}

pub(crate) const OUTBOX_IDLE_PAGE_BUDGET: usize = 4;

pub(crate) struct WorkflowOutboxPump {
    config: WorkflowOutboxPumpConfig,
    after: Option<DurableOutboxId>,
    watermark: Option<DurableOutboxId>,
    cooldown_since: Option<Instant>,
    retry_needed: bool,
}

impl WorkflowOutboxPump {
    fn back_off(&mut self, now: Instant) {
        // Compare elapsed time rather than overflowing a future Instant.
        self.cooldown_since = Some(now);
    }
}

impl Runtime {
    /// Enable the scheduler's opt-in same-store outbox pump. Probe the
    /// backend first; unsupported stores are never silently enabled.
    /// Re-enable after replacing the persistence store.
    pub fn enable_workflow_outbox_pump(
        &mut self,
        config: WorkflowOutboxPumpConfig,
    ) -> io::Result<()> {
        if self.current_actor.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "enable workflow outbox pump between actor turns",
            ));
        }
        if config.page_size == 0
            || config.page_size > MAX_WORKFLOW_OUTBOX_PAGE
            || config.retry_interval < Duration::from_millis(1)
            || config.retry_interval > Duration::from_secs(300)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid workflow outbox pump page size or retry interval",
            ));
        }
        self.persistence.scan_pending_workflow_outbox(
            self.shard_idx,
            self.shard_count,
            None,
            None,
            1,
        )?;
        self.workflow_outbox_pump = Some(Box::new(WorkflowOutboxPump {
            config,
            after: None,
            watermark: None,
            cooldown_since: None,
            retry_needed: false,
        }));
        Ok(())
    }

    pub fn disable_workflow_outbox_pump(&mut self) {
        self.workflow_outbox_pump = None;
    }

    /// A newly committed message wakes an otherwise idle sweep. Do not bypass
    /// cooldown after deferred/failed delivery; a hot sender must not cause
    /// repeated retries of an unavailable receiver.
    pub(crate) fn note_workflow_outbox_staged(&mut self) {
        if let Some(pump) = &mut self.workflow_outbox_pump {
            if pump.watermark.is_none() && !pump.retry_needed {
                pump.cooldown_since = None;
            }
        }
    }

    /// Attempt one due page between actor turns. None means disabled, busy,
    /// or cooling down. Individual failures are counted and logged; only
    /// committed receipts suppress future polling. A scan failure is returned
    /// and rate-limited. No runtime sleep
    /// or receiver reconstruction occurs here.
    pub fn poll_workflow_outbox(&mut self) -> io::Result<Option<WorkflowOutboxPumpReport>> {
        if self.current_actor.is_some() || self.workflow_outbox_pump.is_none() {
            return Ok(None);
        }
        let now = self.now();
        if self.workflow_outbox_pump.as_ref().is_some_and(|pump| {
            pump.cooldown_since.is_some_and(|start| {
                now.saturating_duration_since(start) < pump.config.retry_interval
            })
        }) {
            return Ok(None);
        }
        let mut pump = self
            .workflow_outbox_pump
            .take()
            .expect("enabled outbox pump");
        if pump.watermark.is_none() {
            pump.retry_needed = false;
        }
        let page = match self.persistence.scan_pending_workflow_outbox(
            self.shard_idx,
            self.shard_count,
            pump.after,
            pump.watermark,
            pump.config.page_size,
        ) {
            Ok(page) => page,
            Err(error) => {
                pump.retry_needed = true;
                pump.back_off(self.now());
                self.workflow_outbox_pump = Some(pump);
                return Err(error);
            }
        };
        let mut report = WorkflowOutboxPumpReport {
            examined: page.identities.len(),
            ..Default::default()
        };
        for id in &page.identities {
            match self.dispatch_workflow_outbox(*id) {
                Ok(WorkflowOutboxDispatch::Admitted(_)) => report.admitted += 1,
                Ok(WorkflowOutboxDispatch::AlreadyAdmitted(_)) => report.already_admitted += 1,
                Ok(WorkflowOutboxDispatch::Deferred) => report.deferred += 1,
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(?id, %error, "workflow outbox admission failed; inspect receipt and recover receiver if commit was uncertain");
                }
            }
        }
        pump.retry_needed |= report.deferred > 0 || report.failed > 0;
        report.sweep_complete = page.identities.len() < pump.config.page_size
            || page.identities.last().copied() == page.watermark;
        if report.sweep_complete {
            pump.after = None;
            pump.watermark = None;
            pump.back_off(self.now());
        } else {
            pump.after = page.identities.last().copied();
            pump.watermark = page.watermark;
            pump.cooldown_since = None;
        }
        self.workflow_outbox_pump = Some(pump);
        Ok(Some(report))
    }

    /// Return true only when another page in the current sweep is due.
    pub(crate) fn tick_workflow_outbox_pump(&mut self) -> bool {
        match self.poll_workflow_outbox() {
            Ok(Some(report)) => !report.sweep_complete,
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(%error, "workflow outbox scan failed; cooling down");
                false
            }
        }
    }

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
