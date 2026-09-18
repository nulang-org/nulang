//! Distributed actor system context extracted from the Runtime god-object.
//!
//! Groups the fields that support location-transparent message passing,
//! cluster membership, gossip, remote spawn, and the first Nulang Fabric
//! messaging primitives.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use crate::runtime::cluster::{ClusterState, NodeId};
use crate::runtime::network::NetworkTransport;
use crate::runtime::{
    ActorAddress, AddressResolver, FileFabricStreamStore, MessageAdmission, Runtime,
};
use crate::vm::Value;

/// Cluster advertisement for one ephemeral Fabric subscription.
///
/// This is deliberately transport-agnostic. The NUL0/gossip layer can carry
/// these records without making Fabric depend on a second payload protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricAdvertisement {
    pub node_id: NodeId,
    pub pattern: String,
    pub actor_id: u64,
    pub behavior: String,
    pub group: Option<String>,
}

/// A complete, versioned subscription snapshot owned by one cluster node.
///
/// `generation` is monotonically increasing for the lifetime of the node.
/// Receivers ignore snapshots whose generation is not newer than the latest
/// snapshot already applied for that node. This makes duplicate/reordered
/// gossip idempotent and prevents an older subscription set from resurrecting
/// routes that a newer unsubscribe already removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricAdvertisementSnapshot {
    pub node_id: NodeId,
    pub generation: u64,
    pub subscriptions: Vec<FabricAdvertisement>,
}

/// Admission accounting for one ephemeral Fabric publication.
///
/// Remote entries are counted as `forwarded_remote` because the local node can
/// only confirm that it attempted the existing distributed actor send; final
/// mailbox admission is owned by the destination node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FabricPublishReport {
    pub selected: usize,
    pub admitted: usize,
    pub backpressured: usize,
    pub rejected: usize,
    pub forwarded_remote: usize,
}

/// One ephemeral Fabric subscription.
///
/// `node_id == None` denotes a subscription owned by this runtime process.
/// Those entries retain the numeric behavior id needed for fast local and
/// cross-shard delivery. `node_id == Some(node)` denotes a remote actor and
/// intentionally stores no numeric behavior id because behavior-table indices
/// are node-local; remote delivery resolves by behavior name.
///
/// `group == None` is a fan-out subscriber: every matching publication is
/// delivered to it. `group == Some(name)` is a competing consumer: exactly
/// one matching member of each group receives a publication.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FabricSubscription {
    node_id: Option<NodeId>,
    pattern: String,
    actor_id: u64,
    behavior: String,
    behavior_id: Option<u16>,
    group: Option<String>,
}

impl FabricSubscription {
    fn validate_fields(
        pattern: &str,
        actor_id: u64,
        behavior: &str,
        group: Option<&str>,
    ) -> Result<(), String> {
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
        Ok(())
    }

    fn local(
        pattern: &str,
        actor_id: u64,
        behavior: &str,
        behavior_id: u16,
        group: Option<&str>,
    ) -> Result<Self, String> {
        Self::validate_fields(pattern, actor_id, behavior, group)?;
        Ok(Self {
            node_id: None,
            pattern: pattern.to_string(),
            actor_id,
            behavior: behavior.to_string(),
            behavior_id: Some(behavior_id),
            group: group.map(str::to_string),
        })
    }

    fn remote(advertisement: FabricAdvertisement) -> Result<Self, String> {
        Self::validate_fields(
            &advertisement.pattern,
            advertisement.actor_id,
            &advertisement.behavior,
            advertisement.group.as_deref(),
        )?;
        Ok(Self {
            node_id: Some(advertisement.node_id),
            pattern: advertisement.pattern,
            actor_id: advertisement.actor_id,
            behavior: advertisement.behavior,
            behavior_id: None,
            group: advertisement.group,
        })
    }

    fn same_identity(&self, other: &Self) -> bool {
        self.node_id == other.node_id
            && self.pattern == other.pattern
            && self.actor_id == other.actor_id
            && self.behavior == other.behavior
            && self.group == other.group
    }
}

/// A concrete actor delivery selected by Fabric routing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FabricTarget {
    Local {
        actor_id: u64,
        behavior_id: u16,
    },
    Remote {
        node_id: NodeId,
        actor_id: u64,
        behavior: String,
    },
}

/// Deterministic placement ordering for one queue-group candidate.
///
/// Lower scores are preferred. Locality is deliberately the primary key so
/// Fabric avoids a network/shard hop when a same-shard consumer can accept
/// work; mailbox depth breaks ties between same-shard workers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct FabricPlacementScore {
    locality: u8,
    mailbox_depth: usize,
}


/// Cross-shard Fabric control traffic.
///
/// Payload publications continue to use the Runtime's existing CrossShardMsg
/// actor-delivery path. Only small routing metadata is replicated here.
#[derive(Debug, Clone)]
enum FabricControl {
    Subscribe(FabricSubscription),
    UnsubscribeLocalActor(u64),
    ReplaceRemoteNode {
        node_id: NodeId,
        generation: u64,
        subscriptions: Vec<FabricSubscription>,
    },
    RemoveRemoteNode(NodeId),
}

/// Ephemeral topic routing state.
///
/// Every Fabric-enabled runtime shard keeps the same subscription metadata.
/// A publisher therefore selects targets once on its own shard and then uses
/// ordinary actor delivery for local/cross-shard payload transport and the
/// existing distributed actor transport for remote targets.
#[derive(Default)]
struct FabricRegistry {
    subscriptions: Vec<FabricSubscription>,
    // Queue-group cursors are scoped by concrete topic as well as group name;
    // unrelated subjects using the same queue-group label must not perturb
    // one another's deterministic round-robin order.
    group_cursors: HashMap<(String, String), usize>,
    // Highest complete snapshot applied for each remote node. Removing a node
    // clears this entry so a restarted node with the same stable NodeId can
    // start a fresh generation sequence after failure cleanup.
    remote_generations: HashMap<NodeId, u64>,
}

impl FabricRegistry {
    fn insert(&mut self, candidate: FabricSubscription) -> bool {
        if let Some(existing) = self
            .subscriptions
            .iter_mut()
            .find(|existing| existing.same_identity(&candidate))
        {
            // Local hot reloads may keep the same subscription identity while
            // the numeric behavior-table slot changes. Refresh it rather than
            // silently retaining the stale id.
            if existing.behavior_id != candidate.behavior_id {
                *existing = candidate;
                return true;
            }
            return false;
        }
        self.subscriptions.push(candidate);
        true
    }

    fn unsubscribe_local_actor(&mut self, actor_id: u64) -> usize {
        let before = self.subscriptions.len();
        self.subscriptions
            .retain(|sub| sub.node_id.is_some() || sub.actor_id != actor_id);
        before - self.subscriptions.len()
    }

    fn remove_remote_subscriptions(&mut self, node_id: NodeId) -> usize {
        let before = self.subscriptions.len();
        self.subscriptions
            .retain(|sub| sub.node_id != Some(node_id));
        before - self.subscriptions.len()
    }

    fn remove_remote_node(&mut self, node_id: NodeId) -> (usize, bool) {
        let removed = self.remove_remote_subscriptions(node_id);
        let generation_removed = self.remote_generations.remove(&node_id).is_some();
        (removed, generation_removed)
    }

    /// Apply a complete remote snapshot if and only if its generation is
    /// newer than the latest snapshot already known for the node.
    fn replace_remote_node(
        &mut self,
        node_id: NodeId,
        generation: u64,
        subscriptions: Vec<FabricSubscription>,
    ) -> usize {
        if self
            .remote_generations
            .get(&node_id)
            .is_some_and(|current| generation <= *current)
        {
            return 0;
        }

        let removed = self.remove_remote_subscriptions(node_id);
        let mut inserted = 0;
        for subscription in subscriptions {
            debug_assert_eq!(subscription.node_id, Some(node_id));
            if self.insert(subscription) {
                inserted += 1;
            }
        }
        self.remote_generations.insert(node_id, generation);
        removed + inserted
    }

    fn local_subscription_count(&self) -> usize {
        self.subscriptions
            .iter()
            .filter(|sub| sub.node_id.is_none())
            .count()
    }

    fn local_snapshot(
        &self,
        node_id: NodeId,
        generation: u64,
        limit: usize,
    ) -> Result<FabricAdvertisementSnapshot, String> {
        let count = self.local_subscription_count();
        if count > limit {
            return Err(format!(
                "Fabric snapshot has {count} local subscriptions, exceeding limit {limit}; refusing partial advertisement"
            ));
        }

        let subscriptions = self
            .subscriptions
            .iter()
            .filter(|sub| sub.node_id.is_none())
            .map(|sub| FabricAdvertisement {
                node_id,
                pattern: sub.pattern.clone(),
                actor_id: sub.actor_id,
                behavior: sub.behavior.clone(),
                group: sub.group.clone(),
            })
            .collect();

        Ok(FabricAdvertisementSnapshot {
            node_id,
            generation,
            subscriptions,
        })
    }

    fn len(&self) -> usize {
        self.subscriptions.len()
    }

    fn remote_len(&self) -> usize {
        self.subscriptions
            .iter()
            .filter(|sub| sub.node_id.is_some())
            .count()
    }

    fn route_scored<F>(
        &mut self,
        topic: &str,
        mut placement: F,
    ) -> Result<Vec<FabricTarget>, String>
    where
        F: FnMut(&FabricTarget) -> Option<FabricPlacementScore>,
    {
        validate_topic(topic)?;

        let mut targets = Vec::new();
        // BTreeMap keeps group iteration deterministic, which matters for DST
        // and makes routing tests reproducible across hash-map seeds.
        let mut grouped: BTreeMap<String, Vec<(FabricTarget, FabricPlacementScore)>> =
            BTreeMap::new();

        for sub in &self.subscriptions {
            if !pattern_matches(&sub.pattern, topic) {
                continue;
            }
            let target = match (sub.node_id, sub.behavior_id) {
                (None, Some(behavior_id)) => FabricTarget::Local {
                    actor_id: sub.actor_id,
                    behavior_id,
                },
                (Some(node_id), _) => FabricTarget::Remote {
                    node_id,
                    actor_id: sub.actor_id,
                    behavior: sub.behavior.clone(),
                },
                // A local entry without a behavior id violates the registry's
                // construction invariant. Skip it rather than dispatching to
                // an arbitrary behavior.
                (None, None) => continue,
            };
            if let Some(group) = &sub.group {
                if let Some(score) = placement(&target) {
                    grouped
                        .entry(group.clone())
                        .or_default()
                        .push((target, score));
                }
            } else {
                // Fan-out semantics are intentionally placement-agnostic:
                // every matching subscriber receives the publication.
                targets.push(target);
            }
        }

        for (group, members) in grouped {
            let Some(best_score) = members.iter().map(|(_, score)| *score).min() else {
                continue;
            };
            let best_count = members
                .iter()
                .filter(|(_, score)| *score == best_score)
                .count();
            if best_count == 0 {
                continue;
            }

            let key = (topic.to_string(), group);
            let cursor = self.group_cursors.entry(key).or_insert(0);
            let index = *cursor % best_count;
            let selected = members
                .iter()
                .filter(|(_, score)| *score == best_score)
                .nth(index)
                .expect("best-count and candidate iterator must agree")
                .0
                .clone();
            targets.push(selected);
            *cursor = cursor.wrapping_add(1);
        }

        Ok(targets)
    }

    fn route(&mut self, topic: &str) -> Result<Vec<FabricTarget>, String> {
        self.route_scored(topic, |_| Some(FabricPlacementScore::default()))
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
    // Shared by every shard in one runtime process. A single monotonic source
    // avoids divergent generations when subscriptions are registered from
    // different shards.
    fabric_generation: Option<Arc<AtomicU64>>,
    /// Optional durable local stream store. Stream replication is layered on
    /// top later; this first slice owns the append log and replay cursors.
    pub(crate) fabric_streams: Option<FileFabricStreamStore>,
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
        let generation = Arc::new(AtomicU64::new(0));

        for (runtime, (_, rx)) in shards.iter_mut().zip(channels.into_iter()) {
            runtime.distributed.fabric_control_tx = Some(senders.clone());
            runtime.distributed.fabric_control_rx = Some(rx);
            runtime.distributed.fabric_generation = Some(generation.clone());
        }
    }

    fn fabric_generation_counter(&mut self) -> Arc<AtomicU64> {
        self.distributed
            .fabric_generation
            .get_or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone()
    }

    fn fabric_current_generation(&mut self) -> u64 {
        self.fabric_generation_counter().load(Ordering::Acquire)
    }

    fn fabric_bump_generation(&mut self) -> u64 {
        self.fabric_generation_counter()
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
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
                Ok(FabricControl::UnsubscribeLocalActor(actor_id)) => {
                    self.distributed.fabric.unsubscribe_local_actor(actor_id);
                    applied += 1;
                }
                Ok(FabricControl::ReplaceRemoteNode {
                    node_id,
                    generation,
                    subscriptions,
                }) => {
                    self.distributed
                        .fabric
                        .replace_remote_node(node_id, generation, subscriptions);
                    applied += 1;
                }
                Ok(FabricControl::RemoveRemoteNode(node_id)) => {
                    let _ = self.distributed.fabric.remove_remote_node(node_id);
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
            FabricSubscription::local(pattern, actor_id, behavior, behavior_id, None)?;
        let inserted = self.distributed.fabric.insert(subscription.clone());
        if inserted {
            self.fabric_bump_generation();
            self.fabric_broadcast_control(FabricControl::Subscribe(subscription));
        }
        Ok(inserted)
    }

    /// Subscribe a local actor as a competing consumer in `group`.
    ///
    /// For each publication, Fabric selects exactly one matching subscriber
    /// from every consumer group using deterministic per-topic round-robin
    /// routing.
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
            FabricSubscription::local(pattern, actor_id, behavior, behavior_id, Some(group))?;
        let inserted = self.distributed.fabric.insert(subscription.clone());
        if inserted {
            self.fabric_bump_generation();
            self.fabric_broadcast_control(FabricControl::Subscribe(subscription));
        }
        Ok(inserted)
    }

    /// Remove every local Fabric subscription owned by `actor_id` and
    /// replicate the removal to the other Fabric-enabled shards.
    ///
    /// Remote entries with the same numeric actor id are intentionally kept:
    /// actor ids are not globally unique across independent cluster nodes.
    pub fn fabric_unsubscribe_actor(&mut self, actor_id: u64) -> usize {
        self.fabric_sync();
        let removed = self.distributed.fabric.unsubscribe_local_actor(actor_id);
        if removed > 0 {
            self.fabric_bump_generation();
            self.fabric_broadcast_control(FabricControl::UnsubscribeLocalActor(actor_id));
        }
        removed
    }

    /// Return a bounded, complete snapshot of this node's local subscriptions
    /// suitable for cluster advertisement. Remote subscriptions learned from
    /// other nodes are never re-advertised by this method.
    ///
    /// If the local subscription count exceeds `limit`, this returns an error
    /// instead of silently truncating the snapshot. A receiver must never use
    /// an incomplete snapshot with replace semantics because doing so would
    /// incorrectly delete the omitted live routes.
    ///
    /// An undistributed runtime has no cluster node identity and therefore
    /// returns an error rather than manufacturing a routing identity.
    pub fn fabric_advertisements(
        &mut self,
        limit: usize,
    ) -> Result<FabricAdvertisementSnapshot, String> {
        self.fabric_sync();
        let node_id = self.distributed.node_id.ok_or_else(|| {
            "Fabric advertisements require distribution to be enabled".to_string()
        })?;
        let generation = self.fabric_current_generation();
        self.distributed
            .fabric
            .local_snapshot(node_id, generation, limit)
    }

    /// Apply a complete remote subscription snapshot for one cluster node.
    ///
    /// Older or duplicate generations are ignored. Every advertisement must
    /// claim the same node as the snapshot owner; mismatches are rejected so
    /// a peer cannot smuggle another node's routing identity into a direct
    /// snapshot.
    pub fn fabric_replace_remote_advertisements(
        &mut self,
        snapshot: FabricAdvertisementSnapshot,
    ) -> Result<usize, String> {
        self.fabric_sync();
        if !self.distributed.enabled || self.distributed.node_id.is_none() {
            return Err("Fabric remote advertisements require distribution to be enabled".into());
        }
        if self.distributed.node_id == Some(snapshot.node_id) {
            return Ok(0);
        }

        let node_id = snapshot.node_id;
        let generation = snapshot.generation;
        let mut subscriptions = Vec::with_capacity(snapshot.subscriptions.len());
        for advertisement in snapshot.subscriptions {
            if advertisement.node_id != node_id {
                return Err(format!(
                    "Fabric advertisement node mismatch: snapshot owner {:?}, entry claims {:?}",
                    node_id, advertisement.node_id
                ));
            }
            subscriptions.push(FabricSubscription::remote(advertisement)?);
        }

        let changed =
            self.distributed
                .fabric
                .replace_remote_node(node_id, generation, subscriptions.clone());
        // Broadcast the authoritative snapshot even when this shard already
        // had the same logical generation: another shard may have joined the
        // control plane later and still need to converge.
        self.fabric_broadcast_control(FabricControl::ReplaceRemoteNode {
            node_id,
            generation,
            subscriptions,
        });
        Ok(changed)
    }

    /// Remove all subscriptions learned from `node_id`.
    ///
    /// Cluster failure/removal handling should call this as soon as a node is
    /// no longer routable so Fabric cannot select dead remote consumers. The
    /// remembered snapshot generation is also removed, allowing a restarted
    /// node with the same NodeId to begin a fresh sequence.
    pub fn fabric_remove_remote_node(&mut self, node_id: NodeId) -> usize {
        self.fabric_sync();
        let (removed, generation_removed) = self.distributed.fabric.remove_remote_node(node_id);
        if removed > 0 || generation_removed {
            self.fabric_broadcast_control(FabricControl::RemoveRemoteNode(node_id));
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

    /// Number of subscriptions currently learned from remote cluster nodes.
    pub fn fabric_remote_subscription_count(&self) -> usize {
        self.distributed.fabric.remote_len()
    }

    /// Publish an ephemeral Fabric message with admission accounting.
    ///
    /// `admitted` counts successful same-process mailbox/channel admission.
    /// `backpressured` reports bounded mailbox or cross-shard-channel
    /// saturation. `rejected` covers stale local targets or payloads that
    /// cannot cross a shard boundary. Remote node sends are counted separately
    /// as `forwarded_remote` because this node has no destination-mailbox ACK.
    pub fn fabric_publish_report(
        &mut self,
        topic: &str,
        args: &[Value],
    ) -> Result<FabricPublishReport, String> {
        self.fabric_sync();

        let shard_idx = self.shard_idx;
        let shard_count = self.shard_count.max(1);
        let local_mailboxes: HashMap<u64, (usize, usize)> = self
            .actors
            .iter()
            .map(|(&actor_id, actor)| {
                (
                    actor_id,
                    (actor.mailbox.len(), actor.mailbox.capacity()),
                )
            })
            .collect();
        let cluster_known = self.distributed.cluster.is_some();
        let healthy_remote: HashSet<NodeId> = self
            .distributed
            .cluster
            .as_ref()
            .map(|cluster| {
                cluster
                    .healthy_members()
                    .into_iter()
                    .map(|node| node.node_id)
                    .collect()
            })
            .unwrap_or_default();

        let targets = self.distributed.fabric.route_scored(topic, |target| match target {
            FabricTarget::Local { actor_id, .. } => {
                let owner_shard = (*actor_id % shard_count as u64) as u16;
                if owner_shard == shard_idx {
                    let (depth, capacity) = *local_mailboxes.get(actor_id)?;
                    if capacity > 0 && depth >= capacity {
                        return None;
                    }
                    Some(FabricPlacementScore {
                        locality: 0,
                        mailbox_depth: depth,
                    })
                } else {
                    Some(FabricPlacementScore {
                        locality: 1,
                        mailbox_depth: 0,
                    })
                }
            }
            FabricTarget::Remote { node_id, .. } => {
                if cluster_known && !healthy_remote.contains(node_id) {
                    return None;
                }
                Some(FabricPlacementScore {
                    locality: 2,
                    mailbox_depth: 0,
                })
            }
        })?;
        if !self.distributed.enabled
            && targets
                .iter()
                .any(|target| matches!(target, FabricTarget::Remote { .. }))
        {
            return Err("Fabric remote publication requires distribution to be enabled".into());
        }

        let mut report = FabricPublishReport {
            selected: targets.len(),
            ..FabricPublishReport::default()
        };
        for target in targets {
            match target {
                FabricTarget::Local {
                    actor_id,
                    behavior_id,
                } => match self.fabric_admit_local(actor_id, behavior_id, args) {
                    MessageAdmission::Accepted => report.admitted += 1,
                    MessageAdmission::Backpressured => report.backpressured += 1,
                    MessageAdmission::Rejected => report.rejected += 1,
                },
                FabricTarget::Remote {
                    node_id,
                    actor_id,
                    behavior,
                } => {
                    self.send_distributed(
                        ActorAddress::remote(node_id, actor_id),
                        &behavior,
                        args,
                    );
                    report.forwarded_remote += 1;
                }
            }
        }
        debug_assert_eq!(
            report.selected,
            report.admitted
                + report.backpressured
                + report.rejected
                + report.forwarded_remote
        );
        Ok(report)
    }

    /// Publish an ephemeral Fabric message to a concrete topic.
    ///
    /// Preserves the original API: the returned count is the number of routes
    /// selected. Call `fabric_publish_report` when admission/backpressure
    /// details are needed.
    pub fn fabric_publish(&mut self, topic: &str, args: &[Value]) -> Result<usize, String> {
        Ok(self.fabric_publish_report(topic, args)?.selected)
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
        assert!(
            fabric.insert(FabricSubscription::local("orders.*", 1, "fanout", 10, None).unwrap())
        );
        assert!(fabric.insert(
            FabricSubscription::local("orders.created", 2, "work", 20, Some("billing")).unwrap()
        ));
        assert!(fabric.insert(
            FabricSubscription::local("orders.created", 3, "work", 30, Some("billing")).unwrap()
        ));

        let first = fabric.route("orders.created").unwrap();
        assert_eq!(
            first,
            vec![
                FabricTarget::Local {
                    actor_id: 1,
                    behavior_id: 10,
                },
                FabricTarget::Local {
                    actor_id: 2,
                    behavior_id: 20,
                },
            ]
        );

        let second = fabric.route("orders.created").unwrap();
        assert!(matches!(second[0], FabricTarget::Local { actor_id: 1, .. }));
        assert!(matches!(second[1], FabricTarget::Local { actor_id: 3, .. }));

        let third = fabric.route("orders.created").unwrap();
        assert!(matches!(third[1], FabricTarget::Local { actor_id: 2, .. }));
    }

    #[test]
    fn fabric_deduplicates_identical_subscriptions_and_removes_local_actor_entries() {
        let mut fabric = FabricRegistry::default();
        let fanout = FabricSubscription::local("events.>", 7, "handle", 1, None).unwrap();
        assert!(fabric.insert(fanout.clone()));
        assert!(!fabric.insert(fanout));
        assert!(fabric.insert(
            FabricSubscription::local("events.>", 7, "handle", 1, Some("workers")).unwrap()
        ));

        let remote = FabricSubscription::remote(FabricAdvertisement {
            node_id: NodeId(99),
            pattern: "events.>".into(),
            actor_id: 7,
            behavior: "handle".into(),
            group: None,
        })
        .unwrap();
        assert!(fabric.insert(remote));

        assert_eq!(fabric.unsubscribe_local_actor(7), 2);
        assert_eq!(fabric.remote_len(), 1);
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

    #[test]
    fn fabric_scored_routing_prefers_lower_score_and_round_robins_ties() {
        let mut fabric = FabricRegistry::default();
        assert!(fabric.insert(
            FabricSubscription::local("jobs.*", 2, "work", 20, Some("workers")).unwrap()
        ));
        assert!(fabric.insert(
            FabricSubscription::local("jobs.*", 4, "work", 40, Some("workers")).unwrap()
        ));
        assert!(fabric.insert(
            FabricSubscription::local("jobs.*", 6, "work", 60, Some("workers")).unwrap()
        ));

        let first = fabric
            .route_scored("jobs.run", |target| match target {
                FabricTarget::Local { actor_id: 2, .. } => Some(FabricPlacementScore {
                    locality: 0,
                    mailbox_depth: 5,
                }),
                FabricTarget::Local { .. } => Some(FabricPlacementScore {
                    locality: 0,
                    mailbox_depth: 1,
                }),
                FabricTarget::Remote { .. } => None,
            })
            .unwrap();
        assert!(matches!(
            first[0],
            FabricTarget::Local { actor_id: 4, .. }
        ));

        let second = fabric
            .route_scored("jobs.run", |target| match target {
                FabricTarget::Local { actor_id: 2, .. } => Some(FabricPlacementScore {
                    locality: 0,
                    mailbox_depth: 5,
                }),
                FabricTarget::Local { .. } => Some(FabricPlacementScore {
                    locality: 0,
                    mailbox_depth: 1,
                }),
                FabricTarget::Remote { .. } => None,
            })
            .unwrap();
        assert!(matches!(
            second[0],
            FabricTarget::Local { actor_id: 6, .. }
        ));
    }

    #[test]
    fn fabric_placement_prefers_same_shard_then_falls_back_when_full() {
        let mut shards = Runtime::new_fabric_sharded(2);

        let mut same_shard = Actor::new(2, "same-shard-worker", 1);
        same_shard.state = ActorState::Running;
        same_shard.register_behavior("work", noop);
        shards[0].actors.insert(2, same_shard);

        let mut cross_shard = Actor::new(3, "cross-shard-worker", 4);
        cross_shard.state = ActorState::Running;
        cross_shard.register_behavior("work", noop);
        shards[1].actors.insert(3, cross_shard);

        shards[0]
            .fabric_subscribe_group("jobs.*", "workers", 2, "work")
            .unwrap();
        shards[1].fabric_sync();
        shards[1]
            .fabric_subscribe_group("jobs.*", "workers", 3, "work")
            .unwrap();
        shards[0].fabric_sync();

        let first = shards[0]
            .fabric_publish_report("jobs.run", &[Value::int(1)])
            .unwrap();
        assert_eq!(first.admitted, 1);
        assert_eq!(shards[0].actors.get(&2).unwrap().mailbox.len(), 1);

        let second = shards[0]
            .fabric_publish_report("jobs.run", &[Value::int(2)])
            .unwrap();
        assert_eq!(second.admitted, 1);
        assert_eq!(second.backpressured, 0);

        shards[1].drain_cross_shard_messages();
        assert_eq!(shards[1].actors.get(&3).unwrap().mailbox.len(), 1);
    }

    #[test]
    fn fabric_publish_report_exposes_bounded_mailbox_backpressure() {
        let mut rt = Runtime::new();
        let actor_id = 9000_u64;
        let mut actor = Actor::new(actor_id, "bounded-fabric-target", 1);
        actor.state = ActorState::Running;
        actor.register_behavior("handle", noop);
        rt.actors.insert(actor_id, actor);
        rt.fabric_subscribe("events.*", actor_id, "handle").unwrap();

        let first = rt
            .fabric_publish_report("events.created", &[Value::int(1)])
            .unwrap();
        assert_eq!(first.selected, 1);
        assert_eq!(first.admitted, 1);
        assert_eq!(first.backpressured, 0);

        let second = rt
            .fabric_publish_report("events.created", &[Value::int(2)])
            .unwrap();
        assert_eq!(second.selected, 1);
        assert_eq!(second.admitted, 0);
        assert_eq!(second.backpressured, 1);
        assert_eq!(second.rejected, 0);
        assert_eq!(second.forwarded_remote, 0);
        assert_eq!(rt.actors.get(&actor_id).unwrap().mailbox.len(), 1);
    }

    #[test]
    fn fabric_publish_report_exposes_cross_shard_channel_backpressure() {
        let mut shards = Runtime::new_fabric_sharded(2);
        let actor_id = 2_u64;
        let mut actor = Actor::new(actor_id, "cross-shard-fabric-target", 0);
        actor.state = ActorState::Running;
        actor.register_behavior("handle", noop);
        shards[0].actors.insert(actor_id, actor);
        shards[0]
            .fabric_subscribe("jobs.*", actor_id, "handle")
            .unwrap();
        shards[1].fabric_sync();

        for _ in 0..1024 {
            let report = shards[1]
                .fabric_publish_report("jobs.run", &[Value::int(1)])
                .unwrap();
            assert_eq!(report.admitted, 1);
            assert_eq!(report.backpressured, 0);
        }
        let saturated = shards[1]
            .fabric_publish_report("jobs.run", &[Value::int(2)])
            .unwrap();
        assert_eq!(saturated.selected, 1);
        assert_eq!(saturated.admitted, 0);
        assert_eq!(saturated.backpressured, 1);
    }

    #[test]
    fn fabric_exports_complete_local_snapshot_and_replaces_remote_snapshot() {
        let mut source = Runtime::new();
        source.distributed.enabled = true;
        source.distributed.node_id = Some(NodeId(10));
        let actor_id = source.spawn_actor(Box::new(|| Vec::new()));
        source
            .actors
            .get_mut(&actor_id)
            .unwrap()
            .register_behavior("handle", noop);
        source
            .fabric_subscribe_group("jobs.*", "workers", actor_id, "handle")
            .unwrap();

        let snapshot = source.fabric_advertisements(16).unwrap();
        assert_eq!(snapshot.node_id, NodeId(10));
        assert_eq!(snapshot.generation, 1);
        assert_eq!(snapshot.subscriptions.len(), 1);
        assert_eq!(snapshot.subscriptions[0].pattern, "jobs.*");
        assert_eq!(snapshot.subscriptions[0].group.as_deref(), Some("workers"));

        let mut target = Runtime::new();
        target.distributed.enabled = true;
        target.distributed.node_id = Some(NodeId(20));
        assert_eq!(
            target
                .fabric_replace_remote_advertisements(snapshot)
                .unwrap(),
            1
        );
        assert_eq!(target.fabric_remote_subscription_count(), 1);
        assert_eq!(
            target.distributed.fabric.route("jobs.render").unwrap(),
            vec![FabricTarget::Remote {
                node_id: NodeId(10),
                actor_id,
                behavior: "handle".into(),
            }]
        );

        target
            .fabric_replace_remote_advertisements(FabricAdvertisementSnapshot {
                node_id: NodeId(10),
                generation: 2,
                subscriptions: Vec::new(),
            })
            .unwrap();
        assert_eq!(target.fabric_remote_subscription_count(), 0);
    }

    #[test]
    fn fabric_rejects_partial_local_snapshots() {
        let mut source = Runtime::new();
        source.distributed.enabled = true;
        source.distributed.node_id = Some(NodeId(10));
        let actor_id = source.spawn_actor(Box::new(|| Vec::new()));
        source
            .actors
            .get_mut(&actor_id)
            .unwrap()
            .register_behavior("handle", noop);
        source
            .fabric_subscribe("events.one", actor_id, "handle")
            .unwrap();
        source
            .fabric_subscribe("events.two", actor_id, "handle")
            .unwrap();

        let err = source.fabric_advertisements(1).unwrap_err();
        assert!(err.contains("refusing partial advertisement"));
        assert_eq!(source.fabric_advertisements(2).unwrap().generation, 2);
    }

    #[test]
    fn fabric_ignores_stale_remote_snapshot_generation() {
        let mut target = Runtime::new();
        target.distributed.enabled = true;
        target.distributed.node_id = Some(NodeId(20));

        let current = FabricAdvertisementSnapshot {
            node_id: NodeId(10),
            generation: 2,
            subscriptions: vec![FabricAdvertisement {
                node_id: NodeId(10),
                pattern: "events.new".into(),
                actor_id: 7,
                behavior: "handle".into(),
                group: None,
            }],
        };
        assert_eq!(
            target
                .fabric_replace_remote_advertisements(current)
                .unwrap(),
            1
        );

        let stale = FabricAdvertisementSnapshot {
            node_id: NodeId(10),
            generation: 1,
            subscriptions: vec![FabricAdvertisement {
                node_id: NodeId(10),
                pattern: "events.old".into(),
                actor_id: 7,
                behavior: "handle".into(),
                group: None,
            }],
        };
        assert_eq!(
            target.fabric_replace_remote_advertisements(stale).unwrap(),
            0
        );
        assert_eq!(target.fabric_remote_subscription_count(), 1);
        assert_eq!(
            target.distributed.fabric.route("events.new").unwrap().len(),
            1
        );
        assert!(target
            .distributed
            .fabric
            .route("events.old")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn fabric_remote_node_cleanup_does_not_remove_colliding_local_actor_id() {
        let mut rt = Runtime::new();
        rt.distributed.enabled = true;
        rt.distributed.node_id = Some(NodeId(20));

        let actor_id = rt.spawn_actor(Box::new(|| Vec::new()));
        rt.actors
            .get_mut(&actor_id)
            .unwrap()
            .register_behavior("handle", noop);
        rt.fabric_subscribe("events.*", actor_id, "handle").unwrap();

        let snapshot = FabricAdvertisementSnapshot {
            node_id: NodeId(10),
            generation: 1,
            subscriptions: vec![FabricAdvertisement {
                node_id: NodeId(10),
                pattern: "events.*".into(),
                actor_id,
                behavior: "handle".into(),
                group: None,
            }],
        };
        rt.fabric_replace_remote_advertisements(snapshot).unwrap();
        assert_eq!(rt.fabric_subscription_count(), 2);

        assert_eq!(rt.fabric_remove_remote_node(NodeId(10)), 1);
        assert_eq!(rt.fabric_subscription_count(), 1);
        assert_eq!(rt.fabric_remote_subscription_count(), 0);
        assert!(matches!(
            rt.distributed.fabric.route("events.created").unwrap()[0],
            FabricTarget::Local { actor_id: id, .. } if id == actor_id
        ));
    }
}
