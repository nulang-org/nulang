//! Read-only, payload-free actor inspection snapshots.
//!
//! This module intentionally contains no live actor references or mutable
//! inspection hooks. Runtime-owned state is only read from its owning shard.

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
