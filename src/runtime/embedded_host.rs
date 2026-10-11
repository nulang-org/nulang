//! Rust-owned host facade for a persistent, explicitly polled local actor runtime.
//!
//! Native actor behaviors are function pointers today, not captured closures.
//! No C actor ABI or automatic background scheduler is implied.

use std::collections::HashSet;
use std::fmt;

use super::{Actor, MessageAdmission, Runtime};
use crate::vm::Value;

/// Errors returned by the local embedded actor host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostError {
    Stopped,
    ActorLimitReached,
    UnknownActor,
    UnknownBehavior,
    MailboxFull,
    DuplicateBehavior,
    IntegerOutOfRange,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Stopped => "embedded actor host is stopped",
            Self::ActorLimitReached => "embedded actor limit reached",
            Self::UnknownActor => "unknown embedded actor",
            Self::UnknownBehavior => "unknown embedded actor behavior",
            Self::MailboxFull => "embedded actor mailbox is full",
            Self::DuplicateBehavior => "actor behavior is already registered",
            Self::IntegerOutOfRange => "integer does not fit Nulang's 48-bit Int representation",
        };
        f.write_str(message)
    }
}

impl std::error::Error for HostError {}

/// Host-owned, explicitly polled local actor runtime.
///
/// Unlike `Runtime::run_scheduler`, polling does not wait for future timers.
/// This host currently accepts primitive integer state and message arguments;
/// registered native behaviors remain trusted in-process Rust function
/// pointers. It is not an isolation or durability boundary.
pub struct EmbeddedActorHost {
    runtime: Option<Runtime>,
    owned_actors: HashSet<u64>,
    max_actors: usize,
}

impl EmbeddedActorHost {
    /// Start a local actor runtime with a hard limit on host-created actors.
    /// Setting zero disables actor creation while still allowing poll/shutdown.
    pub fn new(max_actors: usize) -> Self {
        Self {
            runtime: Some(Runtime::new()),
            owned_actors: HashSet::new(),
            max_actors,
        }
    }

    fn runtime_mut(&mut self) -> Result<&mut Runtime, HostError> {
        self.runtime.as_mut().ok_or(HostError::Stopped)
    }

    fn checked_int(value: i64) -> Result<Value, HostError> {
        let encoded = Value::int(value);
        if encoded.as_int() != Some(value) {
            return Err(HostError::IntegerOutOfRange);
        }
        Ok(encoded)
    }

    /// Spawn a local native actor with integer-valued state fields.
    ///
    /// A rejected spawn does not consume an actor quota slot.
    pub fn spawn(&mut self, initial_state: Vec<(String, i64)>) -> Result<u64, HostError> {
        self.runtime_mut()?;
        if self.owned_actors.len() >= self.max_actors {
            return Err(HostError::ActorLimitReached);
        }

        let state = initial_state
            .into_iter()
            .map(|(name, value)| Self::checked_int(value).map(|value| (name, value)))
            .collect::<Result<Vec<_>, _>>()?;
        let actor_id = self.runtime_mut()?.spawn_actor(Box::new(move || state));
        self.owned_actors.insert(actor_id);
        Ok(actor_id)
    }

    /// Register a trusted, native function pointer as a named actor behavior.
    ///
    /// No closure capture, serialized module, or foreign function pointer is
    /// supported by this initial embedding API.
    pub fn register_behavior(
        &mut self,
        actor_id: u64,
        name: &str,
        handler: fn(&mut Actor, &[Value]),
    ) -> Result<(), HostError> {
        self.runtime_mut()?;
        if !self.owned_actors.contains(&actor_id) {
            return Err(HostError::UnknownActor);
        }
        let actor = self
            .runtime_mut()?
            .actors
            .get_mut(&actor_id)
            .ok_or(HostError::UnknownActor)?;
        if actor.behavior_table.iter().any(|entry| entry.name == name) {
            return Err(HostError::DuplicateBehavior);
        }
        actor.register_behavior(name, handler);
        Ok(())
    }

    /// Send an integer-only message to an owned local actor.
    ///
    /// Success confirms local mailbox admission, not execution or durable
    /// delivery. A full mailbox returns explicit backpressure without
    /// losing another message silently.
    pub fn send_int(
        &mut self,
        actor_id: u64,
        name: &str,
        args: &[i64],
    ) -> Result<(), HostError> {
        self.runtime_mut()?;
        if !self.owned_actors.contains(&actor_id) {
            return Err(HostError::UnknownActor);
        }
        let actor = self
            .runtime
            .as_ref()
            .and_then(|runtime| runtime.actors.get(&actor_id))
            .ok_or(HostError::UnknownActor)?;
        let behavior_id = actor
            .behavior_table
            .iter()
            .position(|behavior| behavior.name == name)
            .ok_or(HostError::UnknownBehavior)?;
        let behavior_id = u16::try_from(behavior_id).map_err(|_| HostError::UnknownBehavior)?;
        let values = args
            .iter()
            .copied()
            .map(Self::checked_int)
            .collect::<Result<Vec<_>, _>>()?;
        match self
            .runtime_mut()?
            .deliver_local_message(actor_id, behavior_id, &values, None)
        {
            MessageAdmission::Accepted => Ok(()),
            MessageAdmission::Backpressured => Err(HostError::MailboxFull),
            MessageAdmission::Rejected => Err(HostError::UnknownActor),
        }
    }

    /// Read a primitive integer state field, or None if the field is absent
    /// or is not a primitive integer.
    pub fn get_int_field(&self, actor_id: u64, field: &str) -> Result<Option<i64>, HostError> {
        let runtime = self.runtime.as_ref().ok_or(HostError::Stopped)?;
        if !self.owned_actors.contains(&actor_id) {
            return Err(HostError::UnknownActor);
        }
        let actor = runtime.actors.get(&actor_id).ok_or(HostError::UnknownActor)?;
        Ok(actor.get_state_field(field).and_then(|value| value.as_int()))
    }

    /// Run at most `max_turns` ready actor messages on the calling thread.
    /// A zero budget performs no work; future timers require a subsequent poll.
    pub fn poll(&mut self, max_turns: usize) -> Result<usize, HostError> {
        Ok(self.runtime_mut()?.pump_ready(max_turns))
    }

    /// Stop host operations and drop all locally owned actors and their state.
    /// Idempotent; does not persist actor state or await distributed messages.
    pub fn shutdown(&mut self) {
        self.runtime = None;
        self.owned_actors.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Actor;

    fn add_to_counter(actor: &mut Actor, args: &[Value]) {
        let increment = args.first().and_then(Value::as_int).unwrap_or(0);
        let current = actor
            .get_state_field("count")
            .and_then(|value| value.as_int())
            .unwrap_or(0);
        actor.set_state_field("count", Value::int(current + increment));
    }

    #[test]
    fn embedded_host_keeps_state_across_poll_calls_and_shuts_down() {
        let mut host = EmbeddedActorHost::new(2);
        let actor = host.spawn(vec![("count".to_string(), 0)]).unwrap();
        host.register_behavior(actor, "add", add_to_counter).unwrap();
        host.send_int(actor, "add", &[2]).unwrap();
        host.send_int(actor, "add", &[3]).unwrap();

        assert_eq!(host.poll(1).unwrap(), 1);
        assert_eq!(host.get_int_field(actor, "count"), Ok(Some(2)));
        assert_eq!(host.poll(1).unwrap(), 1);
        assert_eq!(host.get_int_field(actor, "count"), Ok(Some(5)));
        assert_eq!(host.poll(1).unwrap(), 0);

        host.shutdown();
        host.shutdown();
        assert_eq!(host.poll(1), Err(HostError::Stopped));
        assert_eq!(host.send_int(actor, "add", &[1]), Err(HostError::Stopped));
    }

    #[test]
    fn embedded_host_fails_closed_on_quota_or_unknown_actor_and_behavior() {
        let mut host = EmbeddedActorHost::new(1);
        let actor = host.spawn(vec![]).unwrap();
        assert_eq!(host.spawn(vec![]), Err(HostError::ActorLimitReached));
        assert_eq!(host.send_int(actor, "missing", &[]), Err(HostError::UnknownBehavior));
        assert_eq!(host.send_int(u64::MAX, "missing", &[]), Err(HostError::UnknownActor));
        assert_eq!(
            host.register_behavior(u64::MAX, "add", add_to_counter),
            Err(HostError::UnknownActor)
        );
    }

    #[test]
    fn embedded_host_reports_mailbox_backpressure_instead_of_silent_success() {
        let mut host = EmbeddedActorHost::new(1);
        let actor_id = host.spawn(vec![("count".to_string(), 0)]).unwrap();
        host.register_behavior(actor_id, "add", add_to_counter).unwrap();
        host.runtime
            .as_mut()
            .unwrap()
            .actors
            .get_mut(&actor_id)
            .unwrap()
            .mailbox
            .reset(1);

        assert_eq!(host.send_int(actor_id, "add", &[2]), Ok(()));
        assert_eq!(
            host.send_int(actor_id, "add", &[100]),
            Err(HostError::MailboxFull),
            "a rejected message must never be reported as admitted"
        );
        assert_eq!(host.poll(1), Ok(1));
        assert_eq!(host.get_int_field(actor_id, "count"), Ok(Some(2)));
        assert_eq!(host.poll(1), Ok(0));
    }

    #[test]
    fn embedded_host_rejects_duplicate_behavior_registration() {
        let mut host = EmbeddedActorHost::new(1);
        let actor = host.spawn(vec![]).unwrap();
        host.register_behavior(actor, "add", add_to_counter).unwrap();
        assert_eq!(
            host.register_behavior(actor, "add", add_to_counter),
            Err(HostError::DuplicateBehavior)
        );
    }

    #[test]
    fn embedded_host_rejects_integer_truncation_before_state_or_message_mutation() {
        let mut host = EmbeddedActorHost::new(2);
        assert_eq!(
            host.spawn(vec![("count".to_string(), i64::MAX)]),
            Err(HostError::IntegerOutOfRange)
        );
        let actor = host.spawn(vec![("count".to_string(), 7)]).unwrap();
        host.register_behavior(actor, "add", add_to_counter).unwrap();
        assert_eq!(
            host.send_int(actor, "add", &[i64::MAX]),
            Err(HostError::IntegerOutOfRange)
        );
        assert_eq!(host.poll(5).unwrap(), 0);
        assert_eq!(host.get_int_field(actor, "count"), Ok(Some(7)));
    }
}
