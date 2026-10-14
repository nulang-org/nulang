//! Read-only, payload-free actor inspection snapshots.
//!
//! This module intentionally contains no live actor references or mutable
//! inspection hooks. Runtime-owned state is only read from its owning shard.

use super::{Actor, Runtime};
use std::collections::BinaryHeap;

/// Maximum number of actors returned by one read-only inspection page.
///
/// Bounding page size avoids accidentally materializing huge snapshots on
/// runtime shards hosting hundreds of thousands of actors.
pub const MAX_ACTOR_INFO_PAGE_SIZE: usize = 100;

/// Shard-local, point-in-time actor metadata for operational diagnostics.
///
/// Contains only non-payload runtime metadata. Actor state field values,
/// mailbox messages, authority tokens, pending effect arguments, and flight
/// recordings are deliberately excluded. Inspection never calls user code.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActorInfo {
    pub id: u64,
    pub name: String,
    /// Debug name of `ActorState`; not a stable on-wire discriminant.
    pub state: String,
    /// Scheduler ownership state (`Idle`, `Queued`, `Running`).
    pub run_state: String,
    pub priority: String,
    pub mailbox_depth: usize,
    /// Completed message-handler reductions, not VM instruction count.
    pub processed_messages: u32,
    pub max_messages_per_turn: u32,
    pub parent: Option<u64>,
    pub child_count: usize,
    pub link_count: usize,
    pub monitor_count: usize,
    pub behavior_count: usize,
    pub persistent: bool,
    pub workflow: bool,
    pub hibernated: bool,
    /// Whether a VM continuation is currently suspended.
    pub vm_suspended: bool,
    /// Current fencing epoch for durable actors only.
    pub activation_epoch: Option<u64>,
}

impl ActorInfo {
    fn from_actor(actor: &Actor) -> Self {
        Self {
            id: actor.id,
            name: actor.name.clone(),
            state: format!("{:?}", actor.state),
            run_state: format!("{:?}", actor.run_state),
            priority: format!("{:?}", actor.priority),
            mailbox_depth: actor.mailbox.len(),
            processed_messages: actor.reduction_count,
            max_messages_per_turn: actor.max_reductions,
            parent: actor.parent,
            child_count: actor.children.len(),
            link_count: actor.links.len(),
            monitor_count: actor.monitors.len(),
            behavior_count: actor.behavior_table.len(),
            persistent: actor.persistent,
            workflow: actor.is_workflow,
            hibernated: actor.is_hibernated(),
            vm_suspended: actor.suspended_execution.is_some(),
            activation_epoch: actor.persistent.then_some(actor.activation_epoch),
        }
    }
}

impl Runtime {
    /// Return a detached, payload-free snapshot of one living actor.
    ///
    /// This is a shard-local API; callers must dispatch requests to the
    /// actor's owning runtime shard. `None` means the id is not live here.
    /// Do not call this concurrently with a mutable scheduler turn.
    pub fn actor_info(&self, actor_id: u64) -> Option<ActorInfo> {
        self.actors.get(&actor_id).map(ActorInfo::from_actor)
    }

    /// Return a deterministic, exclusive-cursor page of actor metadata.
    ///
    /// Pages contain ids strictly greater than `after`, in ascending order.
    /// A bounded max-heap keeps memory O(limit), not O(actor_count). Each
    /// page still scans all local actor ids: this is an operational API, not
    /// an actor-message hot path. Snapshots are point-in-time, not a
    /// transactional view across pages if actors spawn or exit concurrently.
    pub fn actor_info_page(&self, after: Option<u64>, limit: usize) -> Vec<ActorInfo> {
        let limit = limit.min(MAX_ACTOR_INFO_PAGE_SIZE);
        if limit == 0 {
            return Vec::new();
        }

        let mut smallest_ids = BinaryHeap::with_capacity(limit);
        for &id in self.actors.keys() {
            if after.is_some_and(|cursor| id <= cursor) {
                continue;
            }
            if smallest_ids.len() < limit {
                smallest_ids.push(id);
            } else if let Some(&largest) = smallest_ids.peek() {
                if id < largest {
                    smallest_ids.pop();
                    smallest_ids.push(id);
                }
            }
        }

        smallest_ids
            .into_sorted_vec()
            .into_iter()
            .filter_map(|id| self.actor_info(id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;

    #[test]
    fn actor_info_is_a_detached_payload_free_snapshot() {
        let mut runtime = Runtime::new();
        let id = runtime.spawn_actor(Box::new(|| vec![("api_token".into(), Value::int(9876))]));
        let actor = runtime.actors.get_mut(&id).unwrap();
        actor.name = "worker".into();
        actor.state = ActorState::Waiting;
        actor.priority = ActorPriority::High;
        actor.parent = Some(456);
        actor.links.push(17);
        actor.monitors.push(23);
        actor.persistent = true;
        actor.reduction_count = 12;
        actor.mailbox.push(Message {
            behavior_id: 0,
            payload: MessagePayload::from_slice(&[Value::int(42)]),
            sender: 0,
            priority: MessagePriority::Normal,
            trace_id: None,
        }).unwrap();

        let info = runtime.actor_info(id).expect("living actor");
        assert_eq!(info.id, id);
        assert_eq!(info.name, "worker");
        assert_eq!(info.state, "Waiting");
        assert_eq!(info.priority, "High");
        assert_eq!(info.mailbox_depth, 1);
        assert_eq!(info.processed_messages, 12);
        assert_eq!(info.parent, Some(456));
        assert_eq!(info.link_count, 1);
        assert_eq!(info.monitor_count, 1);
        assert!(info.persistent);

        // Snapshots must not borrow or expose mutable actor state or secrets.
        runtime.actors.get_mut(&id).unwrap().name = "renamed".into();
        assert_eq!(info.name, "worker");
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("state_data").is_none());
        assert!(json.get("api_token").is_none());
        assert!(json.get("capabilities").is_none());
        assert!(json.get("authority_manifest").is_none());
        assert!(json.get("flight_recorder").is_none());
    }

    #[test]
    fn actor_info_returns_none_after_actor_is_gone() {
        let mut runtime = Runtime::new();
        let id = runtime.spawn_actor(Box::new(|| vec![]));
        assert!(runtime.actor_info(id).is_some());
        runtime.actors.remove(&id);
        assert!(runtime.actor_info(id).is_none());
        assert!(runtime.actor_info(u64::MAX).is_none());
    }

    #[test]
    fn actor_info_pages_are_sorted_exclusive_and_bounded() {
        let mut runtime = Runtime::new();
        let ids: Vec<u64> = (0..3)
            .map(|_| runtime.spawn_actor(Box::new(|| vec![])))
            .collect();

        assert!(runtime.actor_info_page(None, 0).is_empty());
        let first = runtime.actor_info_page(None, 2);
        assert_eq!(first.iter().map(|a| a.id).collect::<Vec<_>>(), ids[..2]);
        let second = runtime.actor_info_page(Some(first[1].id), 2);
        assert_eq!(second.iter().map(|a| a.id).collect::<Vec<_>>(), ids[2..]);
        assert!(runtime.actor_info_page(Some(ids[2]), 2).is_empty());

        for _ in 0..(MAX_ACTOR_INFO_PAGE_SIZE + 1) {
            runtime.spawn_actor(Box::new(|| vec![]));
        }
        assert_eq!(
            runtime.actor_info_page(None, usize::MAX).len(),
            MAX_ACTOR_INFO_PAGE_SIZE
        );
    }
}
