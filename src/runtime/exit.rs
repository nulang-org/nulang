//! Actor exit protocol: reap, notify monitors/links, propagate faults through
//! the supervision tree, and retire heaps.
//!
//! All functions in this module take `&mut Runtime` to access the runtime's
use crate::runtime::network::Packet;
use crate::runtime::supervision::RemoteLink;
use crate::runtime::NodeId;
use crate::runtime::{
    ActorState, ExitReason, Message, MessagePriority, Runtime, Supervisor, SupervisorAction,
};
use crate::vm::Value;
use std::sync::Arc;

/// Exit an actor with the given reason, then run the full exit protocol:
/// reap the actor (notify monitors, propagate links, release ORCA holds,
/// retire the heap) and route the exit through its supervisor if one exists.
pub(crate) fn handle_actor_exit(rt: &mut Runtime, actor_id: u64, reason: ExitReason) {
    let parent = match rt.actors.get(&actor_id) {
        Some(a) => a.parent,
        None => return,
    };

    reap_living_actor(rt, actor_id, reason.clone());

    if let Some(supervisor_id) = parent {
        let mut supervisor = match rt.supervisors.remove(&supervisor_id) {
            Some(s) => s,
            None => return,
        };
        let action = supervisor.handle_exit(actor_id, reason.clone(), rt);
        match action {
            SupervisorAction::Restarted(_new_id) => {
                rt.supervisors.insert(supervisor_id, supervisor);
            }
            SupervisorAction::Shutdown => {
                let sup_parent = supervisor.parent;
                shutdown_supervisor(rt, supervisor_id, &supervisor);
                if let Some(parent_id) = sup_parent {
                    let escalate_reason =
                        ExitReason::Error("child supervisor shutdown".to_string());
                    handle_supervisor_parent_exit(rt, parent_id, supervisor_id, escalate_reason);
                }
            }
            SupervisorAction::Ignore => {
                rt.supervisors.insert(supervisor_id, supervisor);
            }
            SupervisorAction::Escalate => {
                rt.supervisors.insert(supervisor_id, supervisor);
                if let Some(parent_id) = parent {
                    let escalate_reason = reason.clone();
                    handle_supervisor_parent_exit(rt, parent_id, actor_id, escalate_reason);
                }
            }
        }
    }
}

/// Mark an actor terminated, then exit it through the full protocol.
pub(crate) fn exit_actor(rt: &mut Runtime, actor_id: u64, reason: ExitReason) {
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.state = ActorState::Terminated;
    }
    handle_actor_exit(rt, actor_id, reason);
}

/// Shortcut for kill exits.
pub(crate) fn kill_actor(rt: &mut Runtime, actor_id: u64) {
    exit_actor(rt, actor_id, ExitReason::Kill);
}

// ---------------------------------------------------------------------------
// Reap: notify, clean up, retire
// ---------------------------------------------------------------------------

/// Run the exit protocol for an actor being removed: mark it terminated,
/// release receiver-side ORCA holds, unregister names, leave process groups,
/// send DOWN to monitors, propagate abnormal exits to linked actors, then
/// reap (retire the heap while foreign references are outstanding).
pub(crate) fn reap_living_actor(rt: &mut Runtime, actor_id: u64, reason: ExitReason) {
    let (monitors, links) = {
        let actor = match rt.actors.get(&actor_id) {
            Some(a) => a,
            None => return,
        };
        (actor.monitors.clone(), actor.links.clone())
    };
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.state = ActorState::Terminated;
    }

    // Queued pointer-bearing messages are discarded with the actor. Their
    // send-side mailbox transfer holds can now be published safely.
    rt.release_pending_mailbox_refs(actor_id);
    rt.release_held_foreign_refs(actor_id);

    rt.registry.unregister_by_actor(actor_id);
    rt.process_groups.leave_all(actor_id);

    for watcher_id in monitors {
        send_down_message(rt, watcher_id, actor_id, &reason);
    }

    // Notify remote watchers (cross-node links and monitors).
    let local_node = rt.distributed.node_id.unwrap_or(NodeId::LOCAL);
    let target = RemoteLink {
        node_id: local_node,
        actor_id,
    };
    if let Some(watchers) = rt.remote_links.get_watchers(target) {
        let remote_nodes: Vec<(NodeId, String)> = watchers
            .iter()
            .filter(|w| w.node_id != local_node)
            .map(|w| (w.node_id, reason.tag().to_string()))
            .collect();
        for (node_id, reason_str) in remote_nodes {
            if let (Some(ref mut transport), Some(ref cluster)) =
                (&mut rt.distributed.transport, &rt.distributed.cluster)
            {
                if let Some(info) = cluster.get_node(node_id) {
                    transport.send(
                        NodeId(node_id.0),
                        info.address,
                        Packet::Down {
                            target,
                            reason: reason_str.clone(),
                        },
                    );
                }
            }
        }
        rt.remote_links.clear_target(target);
    }
    if let Some(watchers) = rt.remote_monitors.get_watchers(target) {
        let remote_nodes: Vec<(NodeId, String)> = watchers
            .iter()
            .filter(|w| w.node_id != local_node)
            .map(|w| (w.node_id, reason.tag().to_string()))
            .collect();
        for (node_id, reason_str) in remote_nodes {
            if let (Some(ref mut transport), Some(ref cluster)) =
                (&mut rt.distributed.transport, &rt.distributed.cluster)
            {
                if let Some(info) = cluster.get_node(node_id) {
                    transport.send(
                        NodeId(node_id.0),
                        info.address,
                        Packet::Down {
                            target,
                            reason: reason_str,
                        },
                    );
                }
            }
        }
        rt.remote_monitors.clear_target(target);
    }

    let is_abnormal = !matches!(reason, ExitReason::Normal);
    for linked_id in links {
        if linked_id == actor_id {
            continue;
        }
        let linked_alive = rt
            .actors
            .get(&linked_id)
            .map(|a| a.state != ActorState::Terminated)
            .unwrap_or(false);
        if !linked_alive {
            continue;
        }

        if is_abnormal {
            if matches!(reason, ExitReason::Kill) {
                let kill_reason = ExitReason::Killed;
                if let Some(actor) = rt.actors.get_mut(&linked_id) {
                    actor.state = ActorState::Terminated;
                }
                handle_actor_exit(rt, linked_id, kill_reason);
                continue;
            }
            let traps = rt
                .actors
                .get(&linked_id)
                .map(|a| a.trap_exits)
                .unwrap_or(false);
            if traps {
                let exit_msg = Message {
                    behavior_id: 0,
                    payload: Arc::new(vec![
                        Value::int(actor_id as i64),
                        Value::int(linked_id as i64),
                    ]),
                    sender: actor_id,
                    priority: MessagePriority::System,
                    trace_id: None,
                };
                if let Some(actor) = rt.actors.get_mut(&linked_id) {
                    let _ = actor.mailbox.push_local(exit_msg);
                }
                rt.enqueue_actor(linked_id);
            } else {
                let linked_reason = ExitReason::Error(format!(
                    "linked actor {} exited with {:?}",
                    actor_id, reason
                ));
                if let Some(actor) = rt.actors.get_mut(&linked_id) {
                    actor.state = ActorState::Terminated;
                }
                handle_actor_exit(rt, linked_id, linked_reason);
            }
        }
    }

    // Notify remote watchers (cross-node links and monitors).
    let local_node = rt.distributed.node_id.unwrap_or(NodeId::LOCAL);
    let target = RemoteLink {
        node_id: local_node,
        actor_id,
    };
    if let Some(watchers) = rt.remote_links.get_watchers(target) {
        let remote_nodes: Vec<(NodeId, String)> = watchers
            .iter()
            .filter(|w| w.node_id != local_node)
            .map(|w| (w.node_id, reason.tag().to_string()))
            .collect();
        for (node_id, reason_str) in remote_nodes {
            if let (Some(ref mut transport), Some(ref cluster)) =
                (&mut rt.distributed.transport, &rt.distributed.cluster)
            {
                if let Some(info) = cluster.get_node(node_id) {
                    transport.send(
                        NodeId(node_id.0),
                        info.address,
                        Packet::Down {
                            target,
                            reason: reason_str.clone(),
                        },
                    );
                }
            }
        }
        rt.remote_links.clear_target(target);
    }
    if let Some(watchers) = rt.remote_monitors.get_watchers(target) {
        let remote_nodes: Vec<(NodeId, String)> = watchers
            .iter()
            .filter(|w| w.node_id != local_node)
            .map(|w| (w.node_id, reason.tag().to_string()))
            .collect();
        for (node_id, reason_str) in remote_nodes {
            if let (Some(ref mut transport), Some(ref cluster)) =
                (&mut rt.distributed.transport, &rt.distributed.cluster)
            {
                if let Some(info) = cluster.get_node(node_id) {
                    transport.send(
                        NodeId(node_id.0),
                        info.address,
                        Packet::Down {
                            target,
                            reason: reason_str,
                        },
                    );
                }
            }
        }
        rt.remote_monitors.clear_target(target);
    }
    rt.remove_actor_reaping(actor_id);
}

// ---------------------------------------------------------------------------
// Supervisor escalation
// ---------------------------------------------------------------------------
pub(crate) fn handle_supervisor_parent_exit(
    rt: &mut Runtime,
    parent_id: u64,
    child_supervisor_id: u64,
    reason: ExitReason,
) {
    let mut supervisor = match rt.supervisors.remove(&parent_id) {
        Some(s) => s,
        None => return,
    };
    let action = supervisor.handle_exit(child_supervisor_id, reason.clone(), rt);
    match action {
        SupervisorAction::Restarted(_) | SupervisorAction::Ignore => {
            rt.supervisors.insert(parent_id, supervisor);
        }
        SupervisorAction::Shutdown => {
            let sup_parent = supervisor.parent;
            shutdown_supervisor(rt, parent_id, &supervisor);
            if let Some(grandparent_id) = sup_parent {
                let escalate_reason = ExitReason::Error("child supervisor shutdown".to_string());
                handle_supervisor_parent_exit(rt, grandparent_id, parent_id, escalate_reason);
            }
        }
        SupervisorAction::Escalate => {
            let grandparent_id = supervisor.parent;
            rt.supervisors.insert(parent_id, supervisor);
            if let Some(grandparent_id) = grandparent_id {
                handle_supervisor_parent_exit(rt, grandparent_id, child_supervisor_id, reason);
            }
        }
    }
}

pub(crate) fn shutdown_supervisor(rt: &mut Runtime, supervisor_id: u64, supervisor: &Supervisor) {
    let child_ids: Vec<u64> = supervisor.children.iter().map(|(_, id)| *id).collect();
    let reason = ExitReason::Error("supervisor shutdown".to_string());
    for child_id in child_ids {
        rt.exit_actor(child_id, reason.clone());
    }
    rt.supervisors.remove(&supervisor_id);
    rt.registry.unregister_by_actor(supervisor_id);
    if let Some(actor) = rt.actors.get_mut(&supervisor_id) {
        actor.state = ActorState::Terminated;
    }
    rt.remove_actor_reaping(supervisor_id);
}
pub(crate) fn send_down_message(
    rt: &mut Runtime,
    watcher_id: u64,
    target_id: u64,
    reason: &ExitReason,
) {
    let reason_str = reason.tag();
    let down_msg = Message {
        behavior_id: 0,
        payload: Arc::new(vec![
            Value::int(target_id as i64),
            Value::int(watcher_id as i64),
            Value::int(match reason {
                ExitReason::Normal => 0,
                ExitReason::Error(_) => 1,
                ExitReason::Kill => 2,
                ExitReason::Killed => 3,
                ExitReason::Shutdown(_) => 4,
                ExitReason::NoConnection => 6,
                ExitReason::Custom(_) => 5,
            }),
        ]),
        sender: target_id,
        priority: MessagePriority::System,
        trace_id: None,
    };
    if let Some(watcher) = rt.actors.get_mut(&watcher_id) {
        let _ = watcher.mailbox.push_local(down_msg);
        let _ = reason_str;
    }
    rt.enqueue_actor(watcher_id);
}
