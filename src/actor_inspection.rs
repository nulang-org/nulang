//! Read-only actor runtime introspection.
//!
//! Production actor systems need an answer to "what is this actor doing?"
//! without exposing mutable scheduler/runtime internals. This module projects
//! the existing runtime state into stable, serializable inspection snapshots.
//! It performs no mutation and deliberately summarizes sensitive capability
//! manifests by count rather than returning raw authority tokens.

use crate::runtime::{Actor, ActorBackend, ActorPriority, ActorState, Runtime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorLifecycleView {
    Created,
    Running,
    Waiting,
    Suspended,
    Terminated,
}

impl From<ActorState> for ActorLifecycleView {
    fn from(value: ActorState) -> Self {
        match value {
            ActorState::Created => Self::Created,
            ActorState::Running => Self::Running,
            ActorState::Waiting => Self::Waiting,
            ActorState::Suspended => Self::Suspended,
            ActorState::Terminated => Self::Terminated,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKindView {
    Agent,
    Workflow,
    PersistentActor,
    TransientActor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorPriorityView {
    High,
    Normal,
    Low,
}

impl From<ActorPriority> for ActorPriorityView {
    fn from(value: ActorPriority) -> Self {
        match value {
            ActorPriority::High => Self::High,
            ActorPriority::Normal => Self::Normal,
            ActorPriority::Low => Self::Low,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActorBackendView {
    Native,
    WasmComponent { component_path: String },
}

impl From<&ActorBackend> for ActorBackendView {
    fn from(value: &ActorBackend) -> Self {
        match value {
            ActorBackend::Native => Self::Native,
            ActorBackend::WasmComponent { component_path } => Self::WasmComponent {
                component_path: component_path.clone(),
            },
        }
    }
}

/// Read-only snapshot of one actor activation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActorInspection {
    pub actor_id: u64,
    pub name: String,
    pub lifecycle: ActorLifecycleView,
    pub kind: ActorKindView,
    pub backend: ActorBackendView,
    pub priority: ActorPriorityView,

    /// Current mailbox depth including system and application messages.
    pub mailbox_depth: usize,
    /// Configured mailbox capacity. Zero means unbounded.
    pub mailbox_capacity: usize,
    /// `depth / capacity` for bounded mailboxes; `None` for unbounded ones.
    pub mailbox_utilization: Option<f64>,

    pub behavior_count: usize,
    pub state_field_count: usize,
    pub dirty_field_count: usize,
    pub held_object_count: usize,
    pub query_handler_count: usize,

    /// Lifetime messages/reductions handled by the actor.
    pub reduction_count: u32,
    pub max_reductions_per_turn: u32,

    /// Last durable actor sequence/checkpoint position.
    pub durable_sequence: u64,
    pub event_log_len: usize,
    pub event_sourced_field_count: usize,

    pub persistent: bool,
    pub suspended_execution: bool,
    pub waiting_signal: Option<String>,
    pub hibernated: bool,
    pub idle_ms: u64,
    pub pinned: bool,

    pub parent: Option<u64>,
    pub children: Vec<u64>,
    pub monitors: Vec<u64>,
    pub links: Vec<u64>,
    pub trap_exits: bool,

    /// Raw capability tokens can contain resource names/endpoints. The default
    /// inspection surface exposes only the number of installed grants.
    pub capability_count: usize,
    pub flight_recorder_len: usize,
}

impl ActorInspection {
    pub fn from_actor(actor: &Actor) -> Self {
        let mailbox_depth = actor.mailbox.len();
        let mailbox_capacity = actor.mailbox.capacity();
        let mailbox_utilization = if mailbox_capacity == 0 {
            None
        } else {
            Some(mailbox_depth as f64 / mailbox_capacity as f64)
        };

        let kind = if actor.is_agent {
            ActorKindView::Agent
        } else if actor.is_workflow {
            ActorKindView::Workflow
        } else if actor.persistent {
            ActorKindView::PersistentActor
        } else {
            ActorKindView::TransientActor
        };

        let mut children = actor.children.clone();
        let mut monitors = actor.monitors.clone();
        let mut links = actor.links.clone();
        children.sort_unstable();
        monitors.sort_unstable();
        links.sort_unstable();

        Self {
            actor_id: actor.id,
            name: actor.name.clone(),
            lifecycle: actor.state.into(),
            kind,
            backend: (&actor.backend).into(),
            priority: actor.priority.into(),
            mailbox_depth,
            mailbox_capacity,
            mailbox_utilization,
            behavior_count: actor.behavior_table.len(),
            state_field_count: actor.state_data.len(),
            dirty_field_count: actor.dirty_fields.len(),
            held_object_count: actor.held_objects.len(),
            query_handler_count: actor.query_handlers.len(),
            reduction_count: actor.reduction_count,
            max_reductions_per_turn: actor.max_reductions,
            durable_sequence: actor.sequence,
            event_log_len: actor.event_log.len(),
            event_sourced_field_count: actor.event_sourced_sequences.len(),
            persistent: actor.persistent,
            suspended_execution: actor.suspended_execution.is_some(),
            waiting_signal: actor.waiting_signal.clone(),
            hibernated: actor.hibernation_state.is_some(),
            idle_ms: actor.idle_ms,
            pinned: actor.pinned,
            parent: actor.parent,
            children,
            monitors,
            links,
            trap_exits: actor.trap_exits,
            capability_count: actor.capabilities.len(),
            flight_recorder_len: actor.flight_recorder.len(),
        }
    }
}

/// Aggregate read model for one runtime shard/process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActorFleetInspection {
    pub actor_count: usize,
    pub persistent_count: usize,
    pub hibernated_count: usize,
    pub suspended_count: usize,
    pub total_mailbox_depth: usize,
    pub actors: Vec<ActorInspection>,
}

/// Inspect one activation by its current runtime actor id.
pub fn inspect_actor(runtime: &Runtime, actor_id: u64) -> Option<ActorInspection> {
    runtime
        .actors
        .get(&actor_id)
        .map(ActorInspection::from_actor)
}

/// Inspect every activation in stable actor-id order.
pub fn inspect_actors(runtime: &Runtime) -> ActorFleetInspection {
    let mut actors = runtime
        .actors
        .values()
        .map(ActorInspection::from_actor)
        .collect::<Vec<_>>();
    actors.sort_by_key(|actor| actor.actor_id);

    ActorFleetInspection {
        actor_count: actors.len(),
        persistent_count: actors.iter().filter(|actor| actor.persistent).count(),
        hibernated_count: actors.iter().filter(|actor| actor.hibernated).count(),
        suspended_count: actors
            .iter()
            .filter(|actor| actor.suspended_execution)
            .count(),
        total_mailbox_depth: actors.iter().map(|actor| actor.mailbox_depth).sum(),
        actors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Runtime;
    use crate::vm::Value;

    #[test]
    fn missing_actor_returns_none() {
        let runtime = Runtime::new();
        assert_eq!(inspect_actor(&runtime, 999), None);
    }

    #[test]
    fn inspection_projects_existing_actor_state_without_mutation() {
        let mut runtime = Runtime::new();
        let id = runtime.spawn_actor(Box::new(|| {
            vec![
                ("count".into(), Value::int(7)),
                ("ready".into(), Value::bool(true)),
            ]
        }));

        {
            let actor = runtime.actors.get_mut(&id).unwrap();
            actor.persistent = true;
            actor.sequence = 41;
            actor.reduction_count = 99;
            actor.children = vec![9, 3];
            actor.links = vec![8, 2];
            actor.monitors = vec![7, 1];
            actor
                .capabilities
                .insert("Net::TcpOut(api.example:443)".into());
            actor.idle_ms = 1234;
            actor.pinned = true;
        }

        let view = inspect_actor(&runtime, id).unwrap();
        assert_eq!(view.actor_id, id);
        assert_eq!(view.kind, ActorKindView::PersistentActor);
        assert_eq!(view.state_field_count, 2);
        assert_eq!(view.durable_sequence, 41);
        assert_eq!(view.reduction_count, 99);
        assert_eq!(view.children, vec![3, 9]);
        assert_eq!(view.links, vec![2, 8]);
        assert_eq!(view.monitors, vec![1, 7]);
        assert_eq!(view.capability_count, 1);
        assert_eq!(view.idle_ms, 1234);
        assert!(view.pinned);
        assert_eq!(runtime.actors.get(&id).unwrap().sequence, 41);
    }

    #[test]
    fn fleet_view_is_sorted_and_aggregated() {
        let mut runtime = Runtime::new();
        let first = runtime.spawn_actor(Box::new(Vec::new));
        let second = runtime.spawn_actor(Box::new(Vec::new));
        runtime.actors.get_mut(&second).unwrap().persistent = true;

        let fleet = inspect_actors(&runtime);
        assert_eq!(fleet.actor_count, 2);
        assert_eq!(fleet.persistent_count, 1);
        assert_eq!(fleet.actors[0].actor_id, first.min(second));
        assert_eq!(fleet.actors[1].actor_id, first.max(second));
        assert_eq!(fleet.total_mailbox_depth, 0);
    }

    #[test]
    fn mailbox_utilization_is_none_for_unbounded_mailbox() {
        let mut runtime = Runtime::new();
        let id = runtime.spawn_actor(Box::new(Vec::new));
        let view = inspect_actor(&runtime, id).unwrap();
        assert_eq!(view.mailbox_capacity, 0);
        assert_eq!(view.mailbox_utilization, None);
    }
}
