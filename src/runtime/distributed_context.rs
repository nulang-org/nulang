//! Distributed actor system context extracted from the Runtime god-object.
//!
//! Groups the fields that support location-transparent message passing,
//! cluster membership, gossip, remote spawn, and the first Nulang Fabric
//! messaging primitives.

use std::collections::{BTreeMap, HashMap};

use crate::runtime::cluster::{ClusterState, NodeId};
use crate::runtime::network::NetworkTransport;
use crate::runtime::{AddressResolver, Runtime};
use crate::vm::Value;

/// One ephemeral Fabric subscription.
///
/// `group == None` is a fan-out subscriber: every matching publication is
/// delivered to it. `group == Some(name)` is a competing consumer: exactly
/// one matching member of each group receives a publication.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FabricSubscription {
    pattern: String,
    actor_id: u64,
    behavior: String,
    group: Option<String>,
}

/// A concrete actor delivery selected by Fabric routing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FabricTarget {
    actor_id: u64,
    behavior: String,
}

/// Ephemeral topic routing state.
///
/// This intentionally starts shard-local. Cross-node subscription replication
/// and durable stream cursors build on top of the same routing semantics in a
/// later stage instead of making the core subject matcher depend on storage or
/// consensus.
#[derive(Default)]
struct FabricRegistry {
    subscriptions: Vec<FabricSubscription>,
    group_cursors: HashMap<String, usize>,
}

impl FabricRegistry {
    fn subscribe(
        &mut self,
        pattern: &str,
        actor_id: u64,
        behavior: &str,
        group: Option<&str>,
    ) -> Result<bool, String> {
        validate_pattern(pattern)?;
        if behavior.is_empty() {
            return Err("Fabric behavior name cannot be empty".to_string());
        }
        if actor_id == 0 {
            return Err("Fabric subscriber actor id cannot be 0".to_string());
        }
        if matches!(group, Some("")) {
            return Err("Fabric consumer group cannot be empty".to_string());
        }

        let candidate = FabricSubscription {
            pattern: pattern.to_string(),
            actor_id,
            behavior: behavior.to_string(),
            group: group.map(str::to_string),
        };
        if self.subscriptions.contains(&candidate) {
            return Ok(false);
        }
        self.subscriptions.push(candidate);
        Ok(true)
    }

    fn unsubscribe_actor(&mut self, actor_id: u64) -> usize {
        let before = self.subscriptions.len();
        self.subscriptions.retain(|sub| sub.actor_id != actor_id);
        before - self.subscriptions.len()
    }

    fn len(&self) -> usize {
        self.subscriptions.len()
    }

    fn route(&mut self, topic: &str) -> Result<Vec<FabricTarget>, String> {
        validate_topic(topic)?;

        let mut targets = Vec::new();
        // BTreeMap keeps group iteration deterministic, which matters for DST
        // and makes routing tests reproducible across hash-map seeds.
        let mut grouped: BTreeMap<String, Vec<FabricTarget>> = BTreeMap::new();

        for sub in &self.subscriptions {
            if !pattern_matches(&sub.pattern, topic) {
                continue;
            }
            let target = FabricTarget {
                actor_id: sub.actor_id,
                behavior: sub.behavior.clone(),
            };
            if let Some(group) = &sub.group {
                grouped.entry(group.clone()).or_default().push(target);
            } else {
                targets.push(target);
            }
        }

        for (group, members) in grouped {
            if members.is_empty() {
                continue;
            }
            let cursor = self.group_cursors.entry(group).or_insert(0);
            let index = *cursor % members.len();
            targets.push(members[index].clone());
            *cursor = cursor.wrapping_add(1);
        }

        Ok(targets)
    }
}

/// Validate a concrete published topic.
///
/// Topics are dot-separated and cannot contain wildcards. Empty segments are
/// rejected so `orders..created` cannot silently alias another subject.
fn validate_topic(topic: &str) -> Result<(), String> {
    if topic.is_empty() {
        return Err("Fabric topic cannot be empty".to_string());
    }
    for token in topic.split('.') {
        if token.is_empty() {
            return Err(format!("invalid Fabric topic `{topic}`: empty token"));
        }
        if token == "*" || token == ">" || token.contains('*') || token.contains('>') {
            return Err(format!(
                "invalid Fabric topic `{topic}`: wildcards are only valid in subscriptions"
            ));
        }
    }
    Ok(())
}

/// Validate a NATS-style subscription pattern.
///
/// `*` matches exactly one token. `>` matches one-or-more remaining tokens
/// and must be the final token. Wildcard characters must occupy an entire
/// token, preventing ambiguous spellings such as `orders.foo*`.
fn validate_pattern(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("Fabric subscription pattern cannot be empty".to_string());
    }
    let tokens: Vec<&str> = pattern.split('.').collect();
    for (index, token) in tokens.iter().enumerate() {
        if token.is_empty() {
            return Err(format!(
                "invalid Fabric subscription pattern `{pattern}`: empty token"
            ));
        }
        if token.contains('*') && *token != "*" {
            return Err(format!(
                "invalid Fabric subscription pattern `{pattern}`: `*` must occupy a full token"
            ));
        }
        if token.contains('>') && *token != ">" {
            return Err(format!(
                "invalid Fabric subscription pattern `{pattern}`: `>` must occupy a full token"
            ));
        }
        if *token == ">" && index + 1 != tokens.len() {
            return Err(format!(
                "invalid Fabric subscription pattern `{pattern}`: `>` must be the final token"
            ));
        }
    }
    Ok(())
}

fn pattern_matches(pattern: &str, topic: &str) -> bool {
    let pattern_tokens: Vec<&str> = pattern.split('.').collect();
    let topic_tokens: Vec<&str> = topic.split('.').collect();
    let mut p = 0;
    let mut t = 0;

    while p < pattern_tokens.len() {
        match pattern_tokens[p] {
            ">" => return t < topic_tokens.len(),
            "*" => {
                if t >= topic_tokens.len() {
                    return false;
                }
                p += 1;
                t += 1;
            }
            literal => {
                if t >= topic_tokens.len() || literal != topic_tokens[t] {
                    return false;
                }
                p += 1;
                t += 1;
            }
        }
    }

    t == topic_tokens.len()
}

/// Distributed-subsystem state owned by [`Runtime`].
#[derive(Default)]
pub struct DistributedContext {
    pub transport: Option<Box<dyn NetworkTransport>>,
    pub cluster: Option<ClusterState>,
    pub resolver: Option<AddressResolver>,
    pub node_id: Option<NodeId>,
    pub enabled: bool,
    fabric: FabricRegistry,
}

impl DistributedContext {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Runtime {
    /// Subscribe a local actor to an ephemeral Fabric subject pattern.
    ///
    /// Publications fan out to every matching non-group subscription. The
    /// pattern supports NATS-style `*` (one token) and terminal `>`
    /// (one-or-more remaining tokens) wildcards.
    ///
    /// Returns `Ok(true)` when a new subscription was inserted and
    /// `Ok(false)` when the identical subscription already existed.
    pub fn fabric_subscribe(
        &mut self,
        pattern: &str,
        actor_id: u64,
        behavior: &str,
    ) -> Result<bool, String> {
        if !self.actors.contains_key(&actor_id) {
            return Err(format!("Fabric subscriber actor {actor_id} does not exist"));
        }
        if self.behavior_id_for(actor_id, behavior).is_none() {
            return Err(format!(
                "Fabric subscriber actor {actor_id} has no behavior `{behavior}`"
            ));
        }
        self.distributed
            .fabric
            .subscribe(pattern, actor_id, behavior, None)
    }

    /// Subscribe a local actor as a competing consumer in `group`.
    ///
    /// For each publication, Fabric selects exactly one matching subscriber
    /// from every consumer group using deterministic round-robin routing.
    pub fn fabric_subscribe_group(
        &mut self,
        pattern: &str,
        group: &str,
        actor_id: u64,
        behavior: &str,
    ) -> Result<bool, String> {
        if !self.actors.contains_key(&actor_id) {
            return Err(format!("Fabric subscriber actor {actor_id} does not exist"));
        }
        if self.behavior_id_for(actor_id, behavior).is_none() {
            return Err(format!(
                "Fabric subscriber actor {actor_id} has no behavior `{behavior}`"
            ));
        }
        self.distributed
            .fabric
            .subscribe(pattern, actor_id, behavior, Some(group))
    }

    /// Remove every Fabric subscription owned by `actor_id`.
    pub fn fabric_unsubscribe_actor(&mut self, actor_id: u64) -> usize {
        self.distributed.fabric.unsubscribe_actor(actor_id)
    }

    /// Number of ephemeral subscriptions registered on this runtime shard.
    ///
    /// Exposed primarily for runtime metrics, tests, and leak detection.
    pub fn fabric_subscription_count(&self) -> usize {
        self.distributed.fabric.len()
    }

    /// Publish an ephemeral Fabric message to a concrete topic.
    ///
    /// The returned count is the number of actor deliveries selected by
    /// routing. This first slice is shard-local and intentionally has no
    /// persistence semantics; durable streams and cluster-wide subscription
    /// replication layer on top of the same subject router.
    pub fn fabric_publish(&mut self, topic: &str, args: &[Value]) -> Result<usize, String> {
        let targets = self.distributed.fabric.route(topic)?;
        let mut delivered = 0;
        for target in targets {
            // Resolve by name again at delivery time. This deliberately avoids
            // `send_message`'s legacy unknown-name -> behavior-0 fallback: a
            // stale/hot-reloaded subscription must never invoke the wrong
            // behavior silently.
            let behavior_id = self.behavior_id_for(target.actor_id, &target.behavior);
            if let Some(behavior_id) = behavior_id {
                self.send_message_by_id(target.actor_id, behavior_id, args);
                delivered += 1;
            } else {
                // The actor exited or its behavior table changed after
                // subscription. Remove all of its stale routing entries.
                self.distributed.fabric.unsubscribe_actor(target.actor_id);
            }
        }
        Ok(delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop(_actor: &mut crate::runtime::Actor, _args: &[Value]) {}

    #[test]
    fn fabric_subject_wildcards_match_expected_tokens() {
        assert!(pattern_matches("orders.created", "orders.created"));
        assert!(pattern_matches("orders.*", "orders.created"));
        assert!(!pattern_matches("orders.*", "orders.created.us"));
        assert!(pattern_matches("orders.>", "orders.created"));
        assert!(pattern_matches("orders.>", "orders.created.us"));
        assert!(!pattern_matches("orders.>", "orders"));
        assert!(pattern_matches(">", "orders"));
    }

    #[test]
    fn fabric_rejects_invalid_patterns_and_topics() {
        assert!(validate_pattern("orders.*.created").is_ok());
        assert!(validate_pattern("orders.>").is_ok());
        assert!(validate_pattern("orders.>.created").is_err());
        assert!(validate_pattern("orders.foo*").is_err());
        assert!(validate_pattern("orders..created").is_err());
        assert!(validate_topic("orders.created").is_ok());
        assert!(validate_topic("orders.*").is_err());
        assert!(validate_topic("orders..created").is_err());
    }

    #[test]
    fn fabric_fans_out_and_round_robins_consumer_groups() {
        let mut fabric = FabricRegistry::default();
        assert!(fabric.subscribe("orders.*", 1, "fanout", None).unwrap());
        assert!(fabric
            .subscribe("orders.created", 2, "work", Some("billing"))
            .unwrap());
        assert!(fabric
            .subscribe("orders.created", 3, "work", Some("billing"))
            .unwrap());

        let first = fabric.route("orders.created").unwrap();
        assert_eq!(
            first,
            vec![
                FabricTarget {
                    actor_id: 1,
                    behavior: "fanout".to_string(),
                },
                FabricTarget {
                    actor_id: 2,
                    behavior: "work".to_string(),
                },
            ]
        );

        let second = fabric.route("orders.created").unwrap();
        assert_eq!(second[0].actor_id, 1);
        assert_eq!(second[1].actor_id, 3);

        let third = fabric.route("orders.created").unwrap();
        assert_eq!(third[1].actor_id, 2);
    }

    #[test]
    fn fabric_deduplicates_identical_subscriptions_and_removes_actor_entries() {
        let mut fabric = FabricRegistry::default();
        assert!(fabric.subscribe("events.>", 7, "handle", None).unwrap());
        assert!(!fabric.subscribe("events.>", 7, "handle", None).unwrap());
        assert!(fabric
            .subscribe("events.>", 7, "handle", Some("workers"))
            .unwrap());
        assert_eq!(fabric.unsubscribe_actor(7), 2);
        assert!(fabric.route("events.created").unwrap().is_empty());
    }

    #[test]
    fn fabric_rejects_unknown_behavior_names() {
        let mut rt = Runtime::new();
        let actor_id = rt.spawn_actor(Box::new(|| Vec::new()));
        assert!(rt
            .fabric_subscribe("events.created", actor_id, "missing")
            .is_err());
    }

    #[test]
    fn fabric_actor_exit_removes_subscriptions() {
        let mut rt = Runtime::new();
        let actor_id = rt.spawn_actor(Box::new(|| Vec::new()));
        rt.actors
            .get_mut(&actor_id)
            .unwrap()
            .register_behavior("handle", noop);

        assert!(rt
            .fabric_subscribe("events.>", actor_id, "handle")
            .unwrap());
        assert_eq!(rt.fabric_subscription_count(), 1);

        rt.exit_actor(actor_id, crate::types::ExitReason::Normal);
        assert_eq!(rt.fabric_subscription_count(), 0);
    }
}
