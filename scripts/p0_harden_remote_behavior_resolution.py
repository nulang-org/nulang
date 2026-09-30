from pathlib import Path

path = Path("src/runtime/distributed.rs")
text = path.read_text()

start = """                    // Resolve the behavior name against the target actor's
                    // behavior table. Unknown names must never alias behavior 0.
"""
end = """                    // Intern string payloads into the TARGET actor's module
"""

if text.count(start) != 1:
    raise SystemExit(f"remote resolution start marker count changed: {text.count(start)}")
if text.count(end) < 1:
    raise SystemExit("remote resolution end marker missing")

prefix, rest = text.split(start, 1)
old_block, suffix = rest.split(end, 1)

replacement = r'''                    // Resolve the behavior name against the target actor's
                    // behavior table. A message never gets a numeric behavior id
                    // until the requested name actually resolves. If the sender
                    // supplied a content hash, an unresolved name may trigger
                    // fetch-on-demand, but no sentinel id is manufactured.
                    let resolved_behavior_id =
                        runtime.behavior_id_for(target_actor, &behavior_name);
                    if let Some(behavior_id) = resolved_behavior_id {
                        msg.behavior_id = behavior_id;
                    } else if let Some(sender_hash) = content_hash {
                        if let Some(cached) = runtime.behavior_cache.get(&sender_hash).cloned() {
                            hot_reload_behavior(runtime, target_actor, &cached, &behavior_name);
                            let Some(behavior_id) =
                                runtime.behavior_id_for(target_actor, &behavior_name)
                            else {
                                notify_delivery_failed(
                                    runtime,
                                    msg.sender,
                                    "unknown behavior after cached hot reload",
                                );
                                ack_packet(
                                    transport,
                                    cluster,
                                    incoming.from_node,
                                    incoming.seq,
                                );
                                continue;
                            };
                            msg.behavior_id = behavior_id;
                        } else {
                            warn!(
                                "nulang-net: behavior '{}' is not installed for actor {}; requesting content-addressed bytecode from sender",
                                behavior_name, target_actor
                            );
                            let request = Packet::FetchBehaviorRequest {
                                content_hash: sender_hash,
                            };
                            let from = incoming.from_node;
                            let request_addr = cluster
                                .get_node(from)
                                .map(|info| info.address)
                                .or_else(|| transport.connection_addr(from));
                            if let Some(addr) = request_addr {
                                transport.send(from, addr, request);
                            }
                            runtime
                                .pending_fetched_messages
                                .entry(sender_hash)
                                .or_default()
                                .push((
                                    target_actor,
                                    behavior_name.clone(),
                                    msg.clone(),
                                    string_table.clone(),
                                    object_table.clone(),
                                ));
                            ack_packet(
                                transport,
                                cluster,
                                incoming.from_node,
                                incoming.seq,
                            );
                            continue;
                        }
                    } else {
                        warn!(
                            "nulang-net: rejecting message to actor {}: unknown behavior '{}'",
                            target_actor, behavior_name
                        );
                        notify_delivery_failed(runtime, msg.sender, "unknown behavior");
                        ack_packet(transport, cluster, incoming.from_node, incoming.seq);
                        continue;
                    }

                    // If the sender attached a content hash, verify it against
                    // the behavior that was resolved by name. A cached reload is
                    // accepted only after both name resolution and hash
                    // verification succeed; otherwise fetch or fail closed.
                    if let Some(sender_hash) = content_hash {
                        if !verify_behavior_hash(
                            runtime,
                            target_actor,
                            msg.behavior_id,
                            &sender_hash,
                        ) {
                            if let Some(cached) = runtime.behavior_cache.get(&sender_hash).cloned() {
                                hot_reload_behavior(runtime, target_actor, &cached, &behavior_name);
                                let Some(behavior_id) =
                                    runtime.behavior_id_for(target_actor, &behavior_name)
                                else {
                                    notify_delivery_failed(
                                        runtime,
                                        msg.sender,
                                        "unknown behavior after cached hot reload",
                                    );
                                    ack_packet(
                                        transport,
                                        cluster,
                                        incoming.from_node,
                                        incoming.seq,
                                    );
                                    continue;
                                };
                                msg.behavior_id = behavior_id;
                                if !verify_behavior_hash(
                                    runtime,
                                    target_actor,
                                    msg.behavior_id,
                                    &sender_hash,
                                ) {
                                    notify_delivery_failed(
                                        runtime,
                                        msg.sender,
                                        "behavior content hash mismatched after cached hot reload",
                                    );
                                    ack_packet(
                                        transport,
                                        cluster,
                                        incoming.from_node,
                                        incoming.seq,
                                    );
                                    continue;
                                }
                            } else {
                                warn!(
                                    "nulang-net: behavior '{}' content hash mismatch for actor {}; requesting bytecode from sender",
                                    behavior_name, target_actor
                                );
                                let request = Packet::FetchBehaviorRequest {
                                    content_hash: sender_hash,
                                };
                                let from = incoming.from_node;
                                let request_addr = cluster
                                    .get_node(from)
                                    .map(|info| info.address)
                                    .or_else(|| transport.connection_addr(from));
                                if let Some(addr) = request_addr {
                                    transport.send(from, addr, request);
                                }
                                runtime
                                    .pending_fetched_messages
                                    .entry(sender_hash)
                                    .or_default()
                                    .push((
                                        target_actor,
                                        behavior_name.clone(),
                                        msg.clone(),
                                        string_table.clone(),
                                        object_table.clone(),
                                    ));
                                ack_packet(
                                    transport,
                                    cluster,
                                    incoming.from_node,
                                    incoming.seq,
                                );
                                continue;
                            }
                        }
                    }
'''

path.write_text(prefix + replacement + end + suffix)
