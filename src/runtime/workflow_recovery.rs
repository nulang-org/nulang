//! Opt-in recovery of storage-proven open commands using trusted native recipes.
//! The host must keep a recipe revision immutable across process restarts.

use super::{
    persistence::NativeWorkflowRecoveryBinding, Actor, ActorBackend, ActorRunState, ActorSnapshot,
    ActorState, NativeWorkflowRecoveryAuditEvent, NativeWorkflowRecoveryFailure,
    NativeWorkflowRetryStatus, PersistedValue, Runtime, StateModel, WorkflowActivationId,
    WorkflowEvent, MAX_WORKFLOW_OUTBOX_PAGE,
};
use crate::{primitives::ActorRole, vm::Value};
use std::{
    collections::HashSet,
    io,
    time::{Duration, Instant},
};

pub type NativeWorkflowHandler = (String, fn(&mut Actor, &[Value]));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeWorkflowRecoveryConfig {
    pub page_size: usize,
    pub retry_interval: Duration,
    /// Maximum automatic recovery reservations per open activation.
    pub max_attempts: u32,
}

impl Default for NativeWorkflowRecoveryConfig {
    fn default() -> Self {
        Self {
            page_size: 64,
            retry_interval: Duration::from_millis(100),
            max_attempts: 3,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NativeWorkflowRecoveryReport {
    pub examined: usize,
    pub restored: usize,
    pub deferred: usize,
    pub failed: usize,
    pub sweep_complete: bool,
}

pub(crate) struct NativeWorkflowRecovery {
    config: NativeWorkflowRecoveryConfig,
    after: Option<u64>,
    watermark: Option<u64>,
    cooldown_since: Option<Instant>,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn ordinary_snapshot(snapshot: &ActorSnapshot) -> bool {
    snapshot.activation_epoch > 0
        && snapshot.schema_name.is_none()
        && snapshot.waiting_signal.is_none()
        && snapshot.authority_tokens.is_empty()
        && snapshot.crdt_snapshot.as_ref().is_none_or(Vec::is_empty)
        && snapshot
            .crdt_field_map
            .as_ref()
            .is_none_or(|m| m.is_empty())
}

fn faithfully_persisted(actor: &Actor, value: &Value, expected: &PersistedValue) -> bool {
    let actual = actor.persist_value(value);
    if matches!(actual, PersistedValue::Nil) && !value.is_nil()
        || matches!(expected, PersistedValue::Float(f) if !f.is_finite())
    {
        return false;
    }
    matches!((serde_json::to_vec(expected), serde_json::to_vec(&actual)),
        (Ok(left), Ok(right)) if left == right)
}

fn layout_matches(actor: &Actor, recipe: &[NativeWorkflowHandler]) -> bool {
    actor.behavior_table.len() == recipe.len() + 1
        && actor.behavior_table[0].name == "__timer_fired"
        && actor.behavior_table[1..]
            .iter()
            .zip(recipe)
            .all(|(entry, (name, handler))| {
                entry.name == *name && std::ptr::fn_addr_eq(entry.handler_fn, *handler)
            })
}

impl Runtime {
    /// Register trusted Rust handlers in their exact behavior-ID order (after
    /// the runtime's timer slot). Reusing a revision for changed code is a
    /// host contract violation; the runtime cannot hash Rust function bodies.
    pub fn register_native_workflow_recovery_recipe(
        &mut self,
        name: &str,
        revision: &str,
        handlers: Vec<NativeWorkflowHandler>,
    ) -> io::Result<()> {
        let mut names = HashSet::new();
        if self.current_actor.is_some()
            || name.is_empty()
            || revision.is_empty()
            || handlers.is_empty()
            || handlers.len() > MAX_WORKFLOW_OUTBOX_PAGE
            || handlers.iter().any(|(name, _)| {
                name.is_empty() || name == "__timer_fired" || !names.insert(name.clone())
            })
        {
            return Err(invalid("invalid native workflow recovery recipe"));
        }
        let key = (name.to_owned(), revision.to_owned());
        if self.native_workflow_recovery_recipes.contains_key(&key) {
            return Err(invalid(
                "native workflow recovery revision already registered",
            ));
        }
        self.native_workflow_recovery_recipes.insert(key, handlers);
        Ok(())
    }

    /// Opt an ordinary native workflow into recovery before its first command.
    /// The host must not subsequently change its handler layout or use timers,
    /// signal suspensions, external effects, local/event-sourced/CRDT state,
    /// migration, or destructive actor-ID reuse.
    pub fn bind_native_workflow_recovery(
        &mut self,
        actor_id: u64,
        name: &str,
        revision: &str,
    ) -> io::Result<()> {
        let recipe = self
            .native_workflow_recovery_recipes
            .get(&(name.to_owned(), revision.to_owned()))
            .ok_or_else(|| invalid("native recovery recipe not registered"))?;
        let actor = self
            .actors
            .get(&actor_id)
            .ok_or_else(|| invalid("native workflow not resident"))?;
        let snapshot = self
            .persistence
            .load_snapshot(actor_id)
            .ok_or_else(|| invalid("native workflow snapshot missing"))?;
        if self.current_actor.is_some()
            || !self.is_workflow_outbox_destination(actor_id, 1)
            || actor.name != name
            || !layout_matches(actor, recipe)
            || actor.bytecode_module.is_some()
            || self.recovery_modules.contains_key(&actor_id)
            || actor.run_state == ActorRunState::Running
            || actor.state == ActorState::Terminated
            || actor.is_mid_execution()
            || !actor.mailbox.is_empty()
            || actor.current_workflow_activation.is_some()
            || actor.waiting_signal.is_some()
            || actor.parent.is_some()
            || !actor.children.is_empty()
            || !actor.links.is_empty()
            || !actor.monitors.is_empty()
            || actor
                .state_models
                .values()
                .any(|m| *m != StateModel::Durable)
            || actor.state_data.iter().any(|(key, value)| {
                !snapshot
                    .state
                    .get(key)
                    .is_some_and(|expected| faithfully_persisted(actor, value, expected))
            })
            || snapshot.state.len() != actor.state_data.len()
            || !ordinary_snapshot(&snapshot)
            || snapshot.actor_id != actor_id
            || snapshot.sequence != actor.sequence
            || snapshot.activation_epoch != actor.activation_epoch
            || !self.persistence.read_journal(actor_id).is_empty()
            || !self.persistence.read_events(actor_id).is_empty()
            || !matches!(self.persistence.read_workflow_events(actor_id).as_slice(),
                [WorkflowEvent::WorkflowStarted { name: started, .. }] if started == name)
        {
            return Err(invalid(
                "native workflow is not at a qualifying initial boundary",
            ));
        }
        #[cfg(feature = "native-codegen")]
        if actor.aot_targets.iter().any(Option::is_some) {
            return Err(invalid("native recovery excludes AOT handlers"));
        }
        let binding = NativeWorkflowRecoveryBinding {
            name: name.to_owned(),
            revision: revision.to_owned(),
            behaviors: recipe.iter().map(|(name, _)| name.clone()).collect(),
            activation_epoch: actor.activation_epoch,
        };
        self.persistence
            .save_native_workflow_recovery_binding(actor_id, &binding)?;
        self.native_workflow_recovery_bound.insert(actor_id);
        Ok(())
    }

    pub fn enable_native_workflow_recovery(
        &mut self,
        config: NativeWorkflowRecoveryConfig,
    ) -> io::Result<()> {
        if self.current_actor.is_some()
            || config.page_size == 0
            || config.page_size > MAX_WORKFLOW_OUTBOX_PAGE
            || config.max_attempts == 0
            || config.max_attempts > super::MAX_NATIVE_WORKFLOW_RECOVERY_ATTEMPTS
            || config.retry_interval < Duration::from_millis(1)
            || config.retry_interval > Duration::from_secs(300)
        {
            return Err(invalid("invalid native workflow recovery configuration"));
        }
        let probe = self.persistence.scan_open_native_workflow_recovery(
            self.shard_idx,
            self.shard_count,
            None,
            None,
            1,
        )?;
        // Before the host can spawn a new actor, reserve the open bound IDs
        // visible on this shard. This is not a general durable ID allocator.
        if let Some(max) = probe.watermark {
            super::ACTOR_ID_COUNTER.fetch_max(max + 1, std::sync::atomic::Ordering::Relaxed);
        }
        self.native_workflow_recovery = Some(Box::new(NativeWorkflowRecovery {
            config,
            after: None,
            watermark: None,
            cooldown_since: None,
        }));
        Ok(())
    }

    /// Inspect the persisted reservation budget for the current open command.
    /// Exhausted means no more automatic replay; the last reserved execution
    /// may still be resident/runnable. Closed commands have no open status.
    pub fn native_workflow_retry_status(
        &self,
        actor_id: u64,
    ) -> io::Result<Option<super::NativeWorkflowRetryStatus>> {
        if actor_id == 0 || actor_id % u64::from(self.shard_count) != u64::from(self.shard_idx) {
            return Err(invalid("native recovery status requires the owning shard"));
        }
        self.persistence.load_native_workflow_retry_status(actor_id)
    }

    /// Bounded live inspection of exhausted open reservations on this shard.
    /// The last reserved attempt may still be runnable; redrive checks residency.
    pub fn native_workflow_quarantine(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> io::Result<Vec<NativeWorkflowRetryStatus>> {
        self.persistence.scan_native_workflow_quarantine(
            self.shard_idx,
            self.shard_count,
            after,
            limit,
        )
    }

    /// Audit for an exact activation and ownership epoch, retained after closure.
    pub fn native_workflow_recovery_audit(
        &self,
        activation: WorkflowActivationId,
        activation_epoch: u64,
    ) -> io::Result<Vec<NativeWorkflowRecoveryAuditEvent>> {
        if activation.actor_id == 0
            || activation.actor_id % u64::from(self.shard_count) != u64::from(self.shard_idx)
        {
            return Err(invalid("native recovery audit requires the owning shard"));
        }
        self.persistence
            .load_native_workflow_recovery_audit(activation, activation_epoch)
    }

    /// Trusted host operation: authorize a finite additional budget for an
    /// exact exhausted, nonresident command. This never executes the handler.
    /// Do not reset counters, change recipe identity, or bypass scheduler cooldown.
    pub fn redrive_quarantined_native_workflow(
        &mut self,
        expected: NativeWorkflowRetryStatus,
        additional_attempts: u32,
        operator: &str,
        reason: &str,
    ) -> io::Result<NativeWorkflowRetryStatus> {
        let id = expected.activation.actor_id;
        if self.current_actor.is_some()
            || self.native_workflow_recovery.is_none()
            || self.actors.contains_key(&id)
            || self.recovery_modules.contains_key(&id)
            || self.migrated_actors.contains_key(&id)
            || self.actor_grain_id.contains_key(&id)
            || !expected.is_exhausted()
            || additional_attempts == 0
            || id == 0
            || id % u64::from(self.shard_count) != u64::from(self.shard_idx)
        {
            return Err(invalid(
                "redrive requires an exhausted, nonresident workflow on the owning shard",
            ));
        }
        let binding = self
            .persistence
            .load_native_workflow_recovery_binding(id)?
            .ok_or_else(|| invalid("native workflow recovery binding missing"))?;
        let recipe = self
            .native_workflow_recovery_recipes
            .get(&(binding.name, binding.revision))
            .ok_or_else(|| invalid("native workflow recovery recipe not registered"))?;
        if binding.activation_epoch != expected.activation_epoch
            || !binding
                .behaviors
                .iter()
                .eq(recipe.iter().map(|(name, _)| name))
        {
            return Err(invalid("native workflow recovery layout or epoch mismatch"));
        }
        let new_max_attempts = expected
            .max_attempts
            .checked_add(additional_attempts)
            .ok_or_else(|| invalid("native workflow recovery budget overflow"))?;
        self.persistence.append_native_workflow_recovery_audit(
            NativeWorkflowRecoveryAuditEvent::RedriveAuthorized {
                status: expected,
                new_max_attempts,
                operator: operator.to_owned(),
                reason: reason.to_owned(),
            },
        )
    }

    fn record_native_recovery_failure(
        &mut self,
        status: NativeWorkflowRetryStatus,
        failure: NativeWorkflowRecoveryFailure,
    ) {
        // Diagnostics are best-effort. A storage outage must not turn failure
        // into success or undo the committed attempt reservation.
        if let Err(error) = self.persistence.append_native_workflow_recovery_audit(
            NativeWorkflowRecoveryAuditEvent::AttemptFailed { status, failure },
        ) {
            tracing::warn!(actor_id = status.activation.actor_id, %error, "native workflow failure diagnostic could not be persisted");
        }
    }

    pub fn disable_native_workflow_recovery(&mut self) {
        self.native_workflow_recovery = None;
    }

    /// Attempt one due page between actor turns. Never replace a resident
    /// actor. Missing recipes are deferred; invalid history fails closed.
    pub fn poll_native_workflow_recovery(
        &mut self,
    ) -> io::Result<Option<NativeWorkflowRecoveryReport>> {
        if self.current_actor.is_some() {
            return Ok(None);
        }
        let now = self.now();
        let Some(mut recovery) = self.native_workflow_recovery.take() else {
            return Ok(None);
        };
        if recovery.cooldown_since.is_some_and(|start| {
            now.saturating_duration_since(start) < recovery.config.retry_interval
        }) {
            self.native_workflow_recovery = Some(recovery);
            return Ok(None);
        }
        let page = match self.persistence.scan_open_native_workflow_recovery(
            self.shard_idx,
            self.shard_count,
            recovery.after,
            recovery.watermark,
            recovery.config.page_size,
        ) {
            Ok(page) => page,
            Err(error) => {
                recovery.cooldown_since = Some(self.now());
                self.native_workflow_recovery = Some(recovery);
                return Err(error);
            }
        };
        let mut report = NativeWorkflowRecoveryReport {
            examined: page.actors.len(),
            ..Default::default()
        };
        for &id in &page.actors {
            match self.restore_native_workflow(id, recovery.config.max_attempts) {
                Ok(true) => report.restored += 1,
                Ok(false) => report.deferred += 1,
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(actor_id = id, %error, "native workflow recovery failed");
                }
            }
        }
        report.sweep_complete = page.actors.len() < recovery.config.page_size
            || page.actors.last().copied() == page.watermark;
        if report.sweep_complete {
            recovery.after = None;
            recovery.watermark = None;
            recovery.cooldown_since = Some(self.now());
        } else {
            recovery.after = page.actors.last().copied();
            recovery.watermark = page.watermark;
            recovery.cooldown_since = None;
        }
        self.native_workflow_recovery = Some(recovery);
        Ok(Some(report))
    }

    fn restore_native_workflow(&mut self, actor_id: u64, max_attempts: u32) -> io::Result<bool> {
        if self.actors.contains_key(&actor_id) {
            return Ok(false);
        }
        if self.recovery_modules.contains_key(&actor_id)
            || self.migrated_actors.contains_key(&actor_id)
            || self.actor_grain_id.contains_key(&actor_id)
            || actor_id % u64::from(self.shard_count) != u64::from(self.shard_idx)
        {
            return Err(invalid(
                "native workflow recovery ownership or backend mismatch",
            ));
        }
        let Some(binding) = self
            .persistence
            .load_native_workflow_recovery_binding(actor_id)?
        else {
            return Ok(false);
        };
        let Some(recipe) = self
            .native_workflow_recovery_recipes
            .get(&(binding.name.clone(), binding.revision.clone()))
            .cloned()
        else {
            return Ok(false);
        };
        if !binding
            .behaviors
            .iter()
            .eq(recipe.iter().map(|(name, _)| name))
        {
            return Err(invalid("native workflow recovery layout mismatch"));
        }
        let Some(open) = self.persistence.load_open_workflow_command(actor_id)? else {
            return Ok(false);
        };
        let snapshot = self
            .persistence
            .load_snapshot(actor_id)
            .ok_or_else(|| invalid("native recovery snapshot missing"))?;
        let sequence = open.activation.command_sequence;
        if !ordinary_snapshot(&snapshot)
            || snapshot.actor_id != actor_id
            || binding.activation_epoch != snapshot.activation_epoch
            || open.tail.activation_epoch != snapshot.activation_epoch
            || open.activation.actor_id != actor_id
            || open.tail.sequence != sequence
            || snapshot.sequence.checked_add(1) != Some(sequence)
            || !self.persistence.read_events(actor_id).is_empty()
        {
            return Err(invalid(
                "native recovery requires an ordinary same-epoch open command",
            ));
        }
        let events = self.persistence.read_workflow_events(actor_id);
        if !matches!(events.first(), Some(WorkflowEvent::WorkflowStarted { name, sequence, .. }) if name == &binding.name && *sequence <= snapshot.sequence)
            || events.iter().skip(1).any(|event| {
                !matches!(
                    event,
                    WorkflowEvent::StepCompleted { .. } | WorkflowEvent::StepFailed { .. }
                ) || event.sequence() > snapshot.sequence
            })
        {
            return Err(invalid(
                "native recovery excludes timers, signals and intermediate history",
            ));
        }
        let journal = self.persistence.read_journal(actor_id);
        let mut pending = journal
            .iter()
            .filter(|entry| entry.sequence > snapshot.sequence);
        let entry = pending
            .next()
            .ok_or_else(|| invalid("native recovery command missing"))?;
        if pending.next().is_some()
            || entry.sequence != sequence
            || entry.behavior_id == 0
            || usize::from(entry.behavior_id) > recipe.len()
        {
            return Err(invalid("native recovery command or behavior mismatch"));
        }
        // Charge the reservation durably *before* reconstructing or running
        // the command. A crash after this write uses a slot even if the
        // handler was never reached. Never execute on an ambiguous write.
        let Some(attempt) = self
            .persistence
            .reserve_native_workflow_recovery_attempt(open, max_attempts)?
        else {
            return Ok(false);
        };
        if attempt.is_exhausted() {
            tracing::warn!(actor_id, command_sequence = sequence, attempts = attempt.attempts_reserved,
                "native workflow recovery reserved final automatic attempt; further replay disabled if still open");
        }
        // Verify persisted values before making executable native handlers visible.
        // The existing recovery path independently checks the admission and tail.
        let Some(_) = self.recover_actor(actor_id) else {
            self.record_native_recovery_failure(
                attempt,
                NativeWorkflowRecoveryFailure::ReconstructionRejected,
            );
            return Err(invalid("native workflow recovery rejected by runtime"));
        };
        let actor = self.actors.get_mut(&actor_id).expect("recovered actor");
        let representable =
            actor.backend == ActorBackend::Native
                && matches!(actor.role(), Ok(ActorRole::Workflow))
                && actor.behavior_table.len() == 1
                && actor.behavior_table[0].name == "__timer_fired"
                && actor.current_workflow_activation == Some(open.activation)
                && actor.mailbox.admitted_workflow_behavior() == Some(entry.behavior_id)
                && actor
                    .mailbox
                    .admitted_workflow_message()
                    .is_some_and(|message| {
                        message.payload.len() == entry.payload.len()
                            && entry.payload.iter().zip(message.payload.as_slice()).all(
                                |(expected, value)| faithfully_persisted(actor, value, expected),
                            )
                    })
                && snapshot.state.iter().all(|(key, expected)| {
                    actor
                        .get_state_field(key)
                        .is_some_and(|value| faithfully_persisted(actor, &value, expected))
                });
        if !representable {
            self.remove_actor_reaping(actor_id);
            self.record_native_recovery_failure(
                attempt,
                NativeWorkflowRecoveryFailure::ReconstructionRejected,
            );
            return Err(invalid(
                "native recovery could not faithfully reconstruct command or state",
            ));
        }
        actor.name = binding.name;
        for (name, handler) in recipe {
            actor.register_behavior(name, handler);
        }
        self.native_workflow_recovery_bound.insert(actor_id);
        // Avoid reusing an ID already restored by this runtime. Hosts still
        // own allocation of IDs for nonresident durable actors.
        super::ACTOR_ID_COUNTER.fetch_max(actor_id + 1, std::sync::atomic::Ordering::Relaxed);
        self.enqueue_actor(actor_id);
        Ok(true)
    }

    pub(crate) fn tick_native_workflow_recovery(&mut self) -> bool {
        match self.poll_native_workflow_recovery() {
            Ok(Some(report)) => !report.sweep_complete,
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(%error, "native workflow recovery scan failed");
                false
            }
        }
    }

    /// Stop immediate terminal-failure replay for locally bound actors. The
    /// open admission is authoritative; only a later due sweep may retry it.
    pub(crate) fn defer_native_workflow_terminal_failure(&mut self, actor_id: u64) -> bool {
        self.defer_native_workflow_failure(
            actor_id,
            NativeWorkflowRecoveryFailure::TerminalCommitFailed,
        )
    }

    pub(crate) fn defer_native_workflow_failure(
        &mut self,
        actor_id: u64,
        failure: NativeWorkflowRecoveryFailure,
    ) -> bool {
        if !self.native_workflow_recovery_bound.contains(&actor_id)
            || self.native_workflow_recovery.is_none()
        {
            return false;
        }
        match self.native_workflow_retry_status(actor_id) {
            Ok(Some(status)) => self.record_native_recovery_failure(status, failure),
            Ok(None) => {} // No reserved replay, or an ambiguous commit actually closed.
            Err(error) => {
                tracing::warn!(actor_id, %error, "native workflow failure diagnostic status unavailable")
            }
        }
        let now = self.now();
        self.native_workflow_recovery
            .as_mut()
            .unwrap()
            .cooldown_since = Some(now);
        self.remove_actor_reaping(actor_id);
        true
    }
}
