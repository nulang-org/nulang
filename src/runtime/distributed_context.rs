//! Distributed actor system context extracted from the Runtime god-object.
//!
//! Groups the fields that support location-transparent message passing,
//! cluster membership, gossip, remote spawn, and the first Nulang Fabric
//! messaging primitives.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc;

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
    behavior_id: u16,
    group: Option<String>,
}

impl FabricSubscription {
    fn validated(
        pattern: &str,
        actor_id: u64,
        behavior: &str,
        behavior_id: u16,
        group: Option<&str>,
    ) -> Result<Self, String> {
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
        Ok(Self {
            pattern: pattern.to_string(),
            actor_id,
            behavior: behavior.to_string(),
            behavior_id,
            group: group.map(str::to_string),
        })
    }
}

/// A concrete actor delivery selected by Fabric routing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FabricTarget {
    actor_id: u64,
    behavior_id: u16,
}

/// Cross-shard Fabric control traffic.
///
/// Payload publications continue to use the Runtime's existing CrossShardMsg
/// actor-delivery path. Only small routing metadata is replicated here.
#[derive(Debug, Clone)]
enum FabricControl {
    Subscribe(FabricSubscription),
    UnsubscribeActor(u64),
}

/// Ephemeral topic routing state.
///
/// Every Fabric-enabled runtime shard keeps the same subscription metadata.
/// A publisher therefore selects targets once on its own shard and then uses
/// ordinary actor delivery for local/cross-shard payload transport.
#[derive(Default)]
struct FabricRegistry {
    subscriptions: Vec<FabricSubscription>,
    group_cursors: HashMap<String, usize>,
}

impl FabricRegistry {
    fn insert(&mut self, candidate: FabricSubscription) -> bool {
        if self.subscriptions.contains(&candidate) {
            return false;
        }
        self.subscriptions.push(candidate);
        true
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
                behavior_id: sub.behavior_id,
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
    fabric_control_tx: Option<Vec<mpsc::Sender<FabricControl>>>,
    fabric_control_rx: Option<mpsc::Receiver<FabricControl>>,
}

impl DistributedContext {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Runtime {
    /// Create normal Runtime shards and wire Fabric's metadata control plane.
    ///
    /// Fabric publishes still use the existing Runtime cross-shard payload
    /// transport. The extra channel only replicates subscription lifecycle
    /// metadata so every publisher has a complete routing view.
    pub fn new_fabric_sharded(num_shards: usize) -> Vec<Runtime> {
        let mut shards = Runtime::new_sharded(num_shards);
        Runtime::wire_fabric_shards(&mut shards);
        shards
    }

    /// Wire Fabric metadata replication across an existing ordered shard set.
    ///
    /// Call this before registering subscriptions. `Runtime::new_fabric_sharded`
    /// is the preferred constructor when Fabric is needed from startup.
    pub fn wire_fabric_shards(shards: &mut [Runtime]) {
        if shards.is_empty() {
            return;
        }
        let channels: Vec<_> = (0..shards.len()).map(|_| mpsc::channel()).collect();
        let senders: Vec<_> = channels.iter().map(|(tx, _)| tx.clone()).collect();

        for (runtime, (_, rx)) in shards.iter_mut().zip(channels.into_iter()) {
            runtime.distributed.fabric_control_tx = Some(senders.clone());
            runtime.distributed.fabric_control_rx = Some(rx);
        }
    }

    fn fabric_broadcast_control(&self, control: FabricControl) {
        let Some(senders) = &self.distributed.fabric_control_tx else {
            return;
        };
        for (index, sender) in senders.iter().enumerate() {
            if index == self.shard_idx as usize {
                continue;
            }
            if sender.send(control.clone()).is_err() {
                tracing::warn!(
                    shard = index,
                    "nulang-fabric: shard metadata receiver disconnected"
                );
            }
        }
    }

    /// Apply pending cross-shard Fabric subscription metadata.
    ///
    /// Fabric API entry points call this automatically. Embedders may call it
    /// explicitly before reading metrics or inspecting routing state.
    pub fn fabric_sync(&mut self) -> usize {
        let Some(receiver) = self.distributed.fabric_control_rx.take() else {
            return 0;
        };
        let mut applied = 0;
        loop {
            match receiver.try_recv() {
                Ok(FabricControl::Subscribe(subscription)) => {
                    self.distributed.fabric.insert(subscription);
                    applied += 1;
                }
                Ok(FabricControl::UnsubscribeActor(actor_id)) => {
                    self.distributed.fabric.unsubscribe_actor(actor_id);
                    applied += 1;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        self.distributed.fabric_control_rx = Some(receiver);
        applied
    }

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
        self.fabric_sync();
        if !self.actors.contains_key(&actor_id) {
            return Err(format!("Fabric subscriber actor {actor_id} does not exist"));
        }
        let behavior_id = self.behavior_id_for(actor_id, behavior).ok_or_else(|| {
            format!("Fabric subscriber actor {actor_id} has no behavior `{behavior}`")
        })?;
        let subscription =
            FabricSubscription::validated(pattern, actor_id, behavior, behavior_id, None)?;
        let inserted = self.distributed.fabric.insert(subscription.clone());
        if inserted {
            self.fabric_broadcast_control(FabricControl::Subscribe(subscription));
        }
        Ok(inserted)
    }

    /// Subscribe a local actor as a competing consumer in `group`.
    ///
    /// For each publication, Fabric selects exactly one matching subscriber
    /// from every consumer group using deterministic per-publisher-shard
    /// round-robin routing.
    pub fn fabric_subscribe_group(
        &mut self,
        pattern: &str,
        group: &str,
        actor_id: u64,
        behavior: &str,
    ) -> Result<bool, String> {
        self.fabric_sync();
        if !self.actors.contains_key(&actor_id) {
            return Err(format!("Fabric subscriber actor {actor_id} does not exist"));
        }
        let behavior_id = self.behavior_id_for(actor_id, behavior).ok_or_else(|| {
            format!("Fabric subscriber actor {actor_id} has no behavior `{behavior}`")
        })?;
        let subscription =
            FabricSubscription::validated(pattern, actor_id, behavior, behavior_id, Some(group))?;
        let inserted = self.distributed.fabric.insert(subscription.clone());
        if inserted {
            self.fabric_broadcast_control(FabricControl::Subscribe(subscription));
        }
        Ok(inserted)
    }

    /// Remove every Fabric subscription owned by `actor_id` and replicate the
    /// removal to the other Fabric-enabled shards.
    pub fn fabric_unsubscribe_actor(&mut self, actor_id: u64) -> usize {
        self.fabric_sync();
        let removed = self.distributed.fabric.unsubscribe_actor(actor_id);
        if removed > 0 {
            self.fabric_broadcast_control(FabricControl::UnsubscribeActor(actor_id));
        }
        removed
    }

    /// Number of ephemeral subscriptions currently known by this runtime shard.
    ///
    /// Call `fabric_sync()` first when an immediately current cross-shard gauge
    /// is required.
    pub fn fabric_subscription_count(&self) -> usize {
        self.distributed.fabric.len()
    }

    /// Publish an ephemeral Fabric message to a concrete topic.
    ///
    /// The returned count is the number of actor deliveries selected by
    /// routing. In a Fabric-sharded runtime, subscription metadata is synced
    /// first and ordinary `send_message_by_id` handles local vs cross-shard
    /// payload delivery.
    pub fn fabric_publish(&mut self, topic: &str, args: &[Value]) -> Result<usize, String> {
        self.fabric_sync();
        let targets = self.distributed.fabric.route(topic)?;
        let delivered = targets.len();
        for target in targets {
            self.send_message_by_id(target.actor_id, target.behavior_id, args);
        }
        Ok(delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{Actor, ActorState};

    fn noop(_actor: &mut Actor, _args: &[Value]) {}

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
        assert!(fabric.insert(
            FabricSubscription::validated("orders.*", 1, "fanout", 10, None).unwrap()
        ));
        assert!(fabric.insert(
            FabricSubscription::validated("orders.created", 2, "work", 20, Some("billing"))
                .unwrap()
        ));
        assert!(fabric.insert(
            FabricSubscription::validated("orders.created", 3, "work", 30, Some("billing"))
                .unwrap()
        ));

        let first = fabric.route("orders.created").unwrap();
        assert_eq!(
            first,
            vec![
                FabricTarget {
                    actor_id: 1,
                    behavior_id: 10,
                },
                FabricTarget {
                    actor_id: 2,
                    behavior_id: 20,
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
        let fanout = FabricSubscription::validated("events.>", 7, "handle", 1, None).unwrap();
        assert!(fabric.insert(fanout.clone()));
        assert!(!fabric.insert(fanout));
        assert!(fabric.insert(
            FabricSubscription::validated("events.>", 7, "handle", 1, Some("workers")).unwrap()
        ));
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

        assert!(rt.fabric_subscribe("events.>", actor_id, "handle").unwrap());
        assert_eq!(rt.fabric_subscription_count(), 1);

        rt.exit_actor(actor_id, crate::types::ExitReason::Normal);
        assert_eq!(rt.fabric_subscription_count(), 0);
    }

    #[test]
    fn fabric_subscription_replication_enables_cross_shard_publish() {
        let mut shards = Runtime::new_fabric_sharded(2);
        let actor_id = 2_u64;
        let mut actor = Actor::new(actor_id, "fabric-target", 0);
        actor.state = ActorState::Running;
        actor.register_behavior("handle", noop);
        shards[0].actors.insert(actor_id, actor);

        assert!(shards[0]
            .fabric_subscribe("events.*", actor_id, "handle")
            .unwrap());
        assert_eq!(shards[1].fabric_sync(), 1);
        assert_eq!(shards[1].fabric_subscription_count(), 1);
        assert_eq!(
            shards[1]
                .fabric_publish("events.created", &[Value::int(42)])
                .unwrap(),
            1
        );

        shards[0].drain_cross_shard_messages();
        assert_eq!(shards[0].actors.get(&actor_id).unwrap().mailbox.len(), 1);
    }
}
