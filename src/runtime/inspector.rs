//! Read-only, deliberately data-minimal live actor inspection.
//!
//! This is an embedders' API, not an unauthenticated HTTP/DAP endpoint.
//! Runtime owners must authorize any remote exposure of these snapshots.

use super::{ActorPriority, ActorRunState, ActorState, Runtime};

/// Detached operational metadata for a single actor.
///
/// Excludes user state, message arguments, flight-recorder traces, effect
/// arguments, and authority grants. All exposed fields are plain values,
/// so an inspector cannot borrow or mutate an actor's internal storage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActorInspection {
    pub actor_id: u64,
    pub name: String,
    pub lifecycle: &'static str,
    pub scheduling: &'static str,
    pub priority: &'static str,
    pub mailbox_depth: usize,
    pub parent: Option<u64>,
    /// Actor IDs are sorted for deterministic rendering.
    pub children: Vec<u64>,
    pub link_count: usize,
    pub monitor_count: usize,
    pub persistent: bool,
    pub workflow: bool,
    pub persisted_sequence: u64,
    pub activation_epoch: u64,
}

impl Runtime {
    /// Capture metadata for a resident actor without reading its state values,
    /// consuming messages, or invoking its behavior.
    ///
    /// Returns `None` for absent actors (including exited actors removed
    /// from the runtime). Call on the owning runtime shard; this does not
    /// aggregate actors from remote nodes.
    pub fn inspect_actor(&self, actor_id: u64) -> Option<ActorInspection> {
        let actor = self.actors.get(&actor_id)?;
        let mut children = actor.children.clone();
        children.sort_unstable();
        Some(ActorInspection {
            actor_id,
            name: actor.name.clone(),
            lifecycle: match actor.state {
                ActorState::Created => "created",
                ActorState::Running => "running",
                ActorState::Waiting => "waiting",
                ActorState::Suspended => "suspended",
                ActorState::Terminated => "terminated",
            },
            scheduling: match actor.run_state {
                ActorRunState::Idle => "idle",
                ActorRunState::Queued => "queued",
                ActorRunState::Running => "running",
            },
            priority: match actor.priority {
                ActorPriority::High => "high",
                ActorPriority::Normal => "normal",
                ActorPriority::Low => "low",
            },
            mailbox_depth: actor.mailbox.len(),
            parent: actor.parent,
            children,
            link_count: actor.links.len(),
            monitor_count: actor.monitors.len(),
            persistent: actor.persistent,
            workflow: actor.is_workflow,
            persisted_sequence: actor.sequence,
            activation_epoch: actor.activation_epoch,
        })
    }

    /// Deterministic metadata snapshots for all actors resident on this shard.
    ///
    /// The returned list is ordered by actor ID and owns its data; mutating
    /// the runtime afterward cannot change previously captured snapshots.
    pub fn inspect_actors(&self) -> Vec<ActorInspection> {
        let mut ids: Vec<_> = self.actors.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| self.inspect_actor(id))
            .collect()
    }
}
