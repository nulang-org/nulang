//! Distributed Actor Address Resolution for Nulang.
//!
//! This module provides **location-transparent actor addressing**, allowing
//! actors to send messages to other actors regardless of whether they reside
//! on the same node or a remote node in the cluster.
//!
//! # Architecture
//!
//! ```text
//!  Actor (local)          ActorAddress::Local{actor_id}
//!       |                            |
//!       v                            v
//!  send_distributed() ----> AddressResolver::resolve()
//!                                 |
//!                     +-----------+-----------+
//!                     |                       |
//!                Local actor          Remote actor
//!                     |                       |
//!               Runtime::           NetworkTransport::
//!               send_message()        send(packet)
//!                                         |
//!                                         v
//!                                    Packet::ActorMessage
//! ```
//!
//! The [`ActorAddress`] enum is the core abstraction: it can refer to either a
//! local actor (same node) or a remote actor (different node). The runtime
//! uses this to route messages to the correct destination without the sender
//! knowing the physical location of the target actor.
//!
//! # Key Types
//!
//! - [`ActorAddress`] — location-transparent actor reference.
//! - [`AddressResolver`] — resolves addresses to local lookups or network routes.
//! - [`RemoteActorCache`] — LRU cache of recently-contacted remote actors.
//! - [`DistributedRuntime`] — trait extending [`Runtime`] with distributed ops.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Imports from sibling modules in the runtime
// ---------------------------------------------------------------------------

use super::mailbox::{Message, MessagePriority};
use super::network::{NetworkTransport, Packet};
use super::{ClusterState, NodeId, NodeStatus};
use crate::runtime::Runtime;
use crate::types::ExitReason;
use crate::vm::Value;
use tracing::warn;

// Message wrapper for distributed communication
#[derive(Debug, Clone, PartialEq)]
pub struct DistributedMessage(pub Message);

impl From<Message> for DistributedMessage {
    fn from(msg: Message) -> Self {
        DistributedMessage(msg)
    }
}

impl From<DistributedMessage> for Message {
    fn from(dm: DistributedMessage) -> Self {
        dm.0
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default maximum number of entries in the remote actor cache.
const DEFAULT_CACHE_SIZE: usize = 10_000;
/// Default TTL for cache entries in seconds. Stale entries are evicted on access.
const CACHE_TTL_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// ActorAddress
// ---------------------------------------------------------------------------

/// A location-transparent address for an actor.
///
/// An `ActorAddress` can refer to either a local actor (same node) or a
/// remote actor (different node). The runtime uses this to route messages
/// to the correct destination without the sender knowing the location.
///
/// # Example
///
/// ```ignore
/// use nulang::runtime::distributed::ActorAddress;
/// use nulang::runtime::cluster::NodeId;
///
/// let local = ActorAddress::local(42);
/// assert!(local.is_local());
///
/// let remote = ActorAddress::remote(NodeId(7), 42);
/// assert!(remote.is_remote());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActorAddress {
    /// Actor on this node.
    Local { actor_id: u64 },
    /// Actor on a remote node.
    Remote { node_id: NodeId, actor_id: u64 },
}

impl ActorAddress {
    /// Create a local address.
    pub fn local(actor_id: u64) -> Self {
        ActorAddress::Local { actor_id }
    }

    /// Create a remote address.
    pub fn remote(node_id: NodeId, actor_id: u64) -> Self {
        ActorAddress::Remote { node_id, actor_id }
    }

    /// Get the actor ID regardless of location.
    pub fn actor_id(&self) -> u64 {
        match self {
            ActorAddress::Local { actor_id } => *actor_id,
            ActorAddress::Remote { actor_id, .. } => *actor_id,
        }
    }

    /// Get the node ID (returns `NodeId::LOCAL` for local actors).
    pub fn node_id(&self) -> NodeId {
        match self {
            ActorAddress::Local { .. } => NodeId::LOCAL,
            ActorAddress::Remote { node_id, .. } => *node_id,
        }
    }

    /// Check if this is a local address.
    pub fn is_local(&self) -> bool {
        matches!(self, ActorAddress::Local { .. })
    }

    /// Check if this is a remote address.
    pub fn is_remote(&self) -> bool {
        matches!(self, ActorAddress::Remote { .. })
    }
}

// ---------------------------------------------------------------------------
// RemoteActorInfo
// ---------------------------------------------------------------------------

/// Cached information about a remote actor.
#[derive(Debug, Clone)]
pub struct RemoteActorInfo {
    /// Node the actor lives on.
    pub node_id: NodeId,
    /// Actor ID on the remote node.
    pub actor_id: u64,
    /// When this cache entry was last accessed.
    pub last_accessed: Instant,
    /// How many messages have been sent to this actor (approximate).
    pub message_count: u64,
}

// ---------------------------------------------------------------------------
// RemoteActorCache
// ---------------------------------------------------------------------------

/// Cache of remote actors that this node knows about.
///
/// When we send a message to a remote actor, we cache its address so
/// that subsequent sends don't need to resolve it again. The cache
/// also tracks which remote actors have sent us messages (so we can
/// reply).
///
/// Uses a simple LRU eviction policy: when the cache exceeds `max_entries`,
pub struct RemoteActorCache {
    /// Map: (remote node, actor id) → cached info.
    entries: HashMap<(NodeId, u64), RemoteActorInfo>,
    /// Maximum number of entries before eviction.
    max_entries: usize,
    /// Access order for LRU eviction — most recent at the back.
    access_order: VecDeque<(NodeId, u64)>,
    /// How long an entry can live before being evicted as stale.
    ttl: Duration,
}

impl RemoteActorCache {
    /// Create a new cache with the given maximum capacity and per-entry TTL.
    pub fn new(max_entries: usize, ttl: Duration) -> Self {
        RemoteActorCache {
            entries: HashMap::with_capacity(max_entries.min(1024)),
            max_entries: max_entries.max(1), // Ensure at least 1
            access_order: VecDeque::new(),
            ttl,
        }
    }

    /// Create a new cache with the default size and TTL.
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_CACHE_SIZE, Duration::from_secs(CACHE_TTL_SECS))
    }

    /// Look up a remote actor in the cache.
    ///
    /// On a hit, the entry is moved to the most-recently-used position.
    pub fn get(&mut self, node_id: NodeId, actor_id: u64) -> Option<&RemoteActorInfo> {
        let key = (node_id, actor_id);
        // Check staleness first (immutable borrow) to avoid double mutable borrow.
        let is_stale = self
            .entries
            .get(&key)
            .map(|info| info.last_accessed.elapsed() > self.ttl)
            .unwrap_or(false);
        if is_stale {
            self.entries.remove(&key);
            self.access_order.retain(|&k| k != key);
            return None;
        }
        if let Some(info) = self.entries.get_mut(&key) {
            // Update LRU position: remove and re-insert at back.
            self.access_order.retain(|&k| k != key);
            self.access_order.push_back(key);
            // Update last_accessed timestamp.
            info.last_accessed = Instant::now();
            Some(info)
        } else {
            None
        }
    }
    /// Add or update a remote actor in the cache.
    ///
    /// If the cache is at capacity, the least-recently-used entry is evicted.
    pub fn put(&mut self, node_id: NodeId, actor_id: u64) {
        let key = (node_id, actor_id);

        // If already present, just update position and timestamp.
        if let Some(info) = self.entries.get_mut(&key) {
            self.access_order.retain(|&k| k != key);
            self.access_order.push_back(key);
            info.last_accessed = Instant::now();
            return;
        }

        // Evict if at capacity.
        if self.entries.len() >= self.max_entries {
            if let Some(evict_key) = self.access_order.pop_front() {
                self.entries.remove(&evict_key);
            }
        }

        // Insert new entry.
        let info = RemoteActorInfo {
            node_id,
            actor_id,
            last_accessed: Instant::now(),
            message_count: 0,
        };
        self.entries.insert(key, info);
        self.access_order.push_back(key);
    }

    /// Remove a remote actor from the cache.
    pub fn remove(&mut self, node_id: NodeId, actor_id: u64) {
        let key = (node_id, actor_id);
        self.entries.remove(&key);
        self.access_order.retain(|&k| k != key);
    }

    /// Remove every cached entry whose actor lives on the given node.
    ///
    /// Called when a node is declared `Failed` so sends to its actors fail
    /// fast with an unresolvable result instead of stale-resolving to a
    /// node that is no longer reachable.
    pub fn remove_node(&mut self, node_id: NodeId) {
        self.entries.retain(|&(n, _), _| n != node_id);
        self.access_order
            .retain(|key| self.entries.contains_key(key));
    }

    /// Get the N most recently accessed remote actors.
    ///
    /// Returns them in MRU order (most recent first).
    pub fn most_active(&self, n: usize) -> Vec<&RemoteActorInfo> {
        self.access_order
            .iter()
            .rev()
            .filter_map(|key| self.entries.get(key))
            .take(n)
            .collect()
    }

    /// Increment the message count for a cached entry.
    ///
    /// No-op if the entry is not in the cache.
    fn increment_message_count(&mut self, node_id: NodeId, actor_id: u64) {
        let key = (node_id, actor_id);
        if let Some(info) = self.entries.get_mut(&key) {
            info.message_count += 1;
        }
    }

    /// Current number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Evict all entries older than the TTL. Call this periodically to
    /// prevent the cache from filling with stale entries for dead actors.
    pub fn evict_stale(&mut self) {
        let cutoff = Instant::now() - self.ttl;
        self.entries.retain(|_, info| info.last_accessed >= cutoff);
        self.access_order
            .retain(|key| self.entries.get(key).is_some());
    }
}

// ---------------------------------------------------------------------------
// ResolveResult
// ---------------------------------------------------------------------------

/// Result of resolving an actor address.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolveResult {
    /// Actor is local — look it up in the local actor table.
    Local { actor_id: u64 },
    /// Actor is remote — send this packet to the given node.
    Remote { node_id: NodeId, actor_id: u64 },
    /// Cannot resolve — node is not in the cluster or actor is unknown.
    Unresolvable { reason: String },
}

// ---------------------------------------------------------------------------
// ResolverStats
// ---------------------------------------------------------------------------

/// Statistics for the address resolver.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct ResolverStats {
    /// Number of addresses resolved to local actors.
    pub local_resolves: u64,
    /// Number of addresses resolved to remote actors.
    pub remote_resolves: u64,
    /// Number of failed resolves (unhealthy node, unknown actor, etc.).
    pub failed_resolves: u64,
    /// Number of cache hits during resolution.
    pub cache_hits: u64,
    /// Number of cache misses during resolution.
    pub cache_misses: u64,
}

// ---------------------------------------------------------------------------
// AddressResolver
// ---------------------------------------------------------------------------

/// Resolves actor addresses to either local actor lookups or network routes.
///
/// This is the core of location transparency: given an [`ActorAddress`],
/// the resolver either finds the local actor or prepares a network packet
/// to send to the remote node.
///
/// The resolver maintains a [`RemoteActorCache`] to avoid repeated cluster
/// lookups for hot remote actors, and tracks statistics for observability.
pub struct AddressResolver {
    local_node: NodeId,
    remote_cache: RemoteActorCache,
    stats: ResolverStats,
}

impl AddressResolver {
    /// Create a new resolver for the given local node.
    pub fn new(local_node: NodeId) -> Self {
        AddressResolver {
            local_node,
            remote_cache: RemoteActorCache::with_defaults(),
            stats: ResolverStats::default(),
        }
    }

    /// Resolve an actor address.
    ///
    /// For local addresses, returns [`ResolveResult::Local`].
    /// For remote addresses, checks the cluster membership to verify
    /// the node is healthy, then returns [`ResolveResult::Remote`].
    pub fn resolve(&mut self, cluster: &ClusterState, address: ActorAddress) -> ResolveResult {
        match address {
            ActorAddress::Local { actor_id } => {
                self.stats.local_resolves += 1;
                ResolveResult::Local { actor_id }
            }
            ActorAddress::Remote { node_id, actor_id } => {
                self.resolve_remote(cluster, node_id, actor_id)
            }
        }
    }

    /// Resolve from raw `node_id` + `actor_id` (used when receiving a message).
    ///
    /// Checks the cluster to verify the remote node is a known, healthy member.
    /// Updates the remote actor cache on success.
    pub fn resolve_remote(
        &mut self,
        cluster: &ClusterState,
        node_id: NodeId,
        actor_id: u64,
    ) -> ResolveResult {
        // Fast path: local node.
        if node_id == self.local_node || node_id == NodeId::LOCAL {
            self.stats.local_resolves += 1;
            return ResolveResult::Local { actor_id };
        }

        // Periodic eviction sweep — every 100th remote resolve, clean stale entries.
        if self.stats.remote_resolves % 100 == 0 {
            self.remote_cache.evict_stale();
        }

        // Check the cache first.
        if self.remote_cache.get(node_id, actor_id).is_some() {
            self.stats.cache_hits += 1;
            self.stats.remote_resolves += 1;
            return ResolveResult::Remote { node_id, actor_id };
        }

        self.stats.cache_misses += 1;

        // Verify the node is in the cluster and healthy.
        match cluster.get_node(node_id) {
            Some(info) => {
                if info.status != NodeStatus::Healthy {
                    self.stats.failed_resolves += 1;
                    return ResolveResult::Unresolvable {
                        reason: format!(
                            "node {:?} is not healthy (status: {:?})",
                            node_id, info.status
                        ),
                    };
                }

                // Healthy node — cache the actor and return remote result.
                self.remote_cache.put(node_id, actor_id);
                self.stats.remote_resolves += 1;
                ResolveResult::Remote { node_id, actor_id }
            }
            None => {
                self.stats.failed_resolves += 1;
                ResolveResult::Unresolvable {
                    reason: format!("node {:?} is not known in the cluster", node_id),
                }
            }
        }
    }

    /// Record that we successfully sent to a remote actor.
    ///
    /// Updates the cache and increments the per-actor message count.
    pub fn record_remote_send(&mut self, node_id: NodeId, actor_id: u64) {
        self.remote_cache.put(node_id, actor_id);
        self.remote_cache.increment_message_count(node_id, actor_id);
    }

    /// Record that we received from a remote actor.
    ///
    /// Updates the cache so we can reply efficiently.
    pub fn record_remote_receive(&mut self, node_id: NodeId, actor_id: u64) {
        self.remote_cache.put(node_id, actor_id);
    }

    /// Build a network packet for sending a message to a remote actor.
    ///
    /// The packet carries the behavior **name** (not a behavior id, which is
    /// a per-actor-table index and meaningless across nodes) plus the
    /// sender's node ID so the remote node can route replies back.
    /// `string_table` holds the UTF-8 content for every string-id value in
    /// `payload` (see [`Packet::ActorMessage::string_table`]); pass an empty
    /// vec when the payload carries no strings.
    /// `content_hash` is an optional BLAKE3 hash of the expected behavior
    /// implementation; `None` means no hash verification is requested.
    pub fn build_packet(
        &self,
        target_actor: u64,
        behavior_name: &str,
        payload: Vec<Value>,
        sender_actor: u64,
        priority: MessagePriority,
        string_table: Vec<String>,
        object_table: Vec<(u64, Vec<u8>)>,
        content_hash: Option<[u8; 32]>,
        trace_id: Option<String>,
    ) -> Packet {
        Packet::ActorMessage {
            target_actor,
            behavior_name: behavior_name.to_string(),
            content_hash,
            payload,
            string_table,
            object_table,
            sender_actor,
            sender_node: NodeId(self.local_node.0),
            priority,
            trace_id,
        }
    }
    /// Parse a received network packet into a message for local delivery.
    ///
    /// Returns `Some((target_actor_id, behavior_name, message, string_table, content_hash))`
    /// if the packet is an actor message that should be delivered locally.
    /// The message's `behavior_id` is left as `0` — the caller must resolve
    /// `behavior_name` against the target actor's behavior table before
    /// enqueueing (see [`process_network_packets`]). `string_table` carries
    /// the content for any string-id values in the payload, which the caller
    /// must intern into the target actor's module pool before delivery.
    /// `content_hash` is an optional BLAKE3 hash for cross-node behavior
    /// identity verification.
    /// Returns `None` for other packet types (e.g., heartbeats, spawn
    /// requests).
    ///
    /// Also updates the remote cache with the sender information.
    pub fn parse_packet(
        &mut self,
        packet: Packet,
    ) -> Option<(
        u64,
        String,
        Message,
        Vec<String>,
        Vec<(u64, Vec<u8>)>,
        Option<[u8; 32]>,
    )> {
        match packet {
            Packet::ActorMessage {
                target_actor,
                behavior_name,
                content_hash,
                payload,
                string_table,
                object_table,
                sender_actor,
                sender_node,
                priority,
                trace_id,
            } => {
                // Record the sender in our cache so we can reply.
                let sender_cluster_node = NodeId(sender_node.0);
                self.record_remote_receive(sender_cluster_node, sender_actor);

                let msg = Message {
                    behavior_id: 0, // resolved from behavior_name at delivery
                    payload: Arc::new(payload),
                    sender: sender_actor,
                    priority,
                    trace_id,
                };
                Some((
                    target_actor,
                    behavior_name,
                    msg,
                    string_table,
                    object_table,
                    content_hash,
                ))
            }
            // Non-actor-message packets are not parsed here.
            _ => None,
        }
    }

    /// Get statistics about the resolver.
    pub fn stats(&self) -> ResolverStats {
        self.stats
    }

    /// Get a reference to the remote actor cache.
    pub fn cache(&self) -> &RemoteActorCache {
        &self.remote_cache
    }

    /// Get a mutable reference to the remote actor cache.
    pub fn cache_mut(&mut self) -> &mut RemoteActorCache {
        &mut self.remote_cache
    }

    /// Drop every cached entry for the given node.
    ///
    /// Called when the node is declared `Failed` so subsequent sends fail
    /// fast instead of stale-resolving to a dead node.
    pub fn invalidate_node(&mut self, node_id: NodeId) {
        self.remote_cache.remove_node(node_id);
    }

    /// Get the local node ID.
    pub fn local_node(&self) -> NodeId {
        self.local_node
    }
}
// ---------------------------------------------------------------------------
// DistributedRuntime trait
// ---------------------------------------------------------------------------

/// Extension methods for [`Runtime`] that add distributed capabilities.
///
/// These methods are called by the integration layer (Stage B1) to
/// wire distributed messaging into the existing [`Runtime`].
///
/// # Example
///
/// ```ignore
/// use nulang_runtime::distributed::{ActorAddress, DistributedRuntimeImpl};
///
/// let mut runtime = Runtime::new();
/// let mut transport = crate::runtime::network::TcpTransport::bind(addr, crate::runtime::network::TlsConfig::PlaintextInsecure).unwrap();
/// let mut cluster = ClusterState::new(local_node, addr);
/// let mut resolver = AddressResolver::new(local_node);
///
/// // Send to a remote actor as if it were local
/// let target = ActorAddress::remote(remote_node, remote_actor_id);
/// DistributedRuntimeImpl::send_distributed(
///     &mut runtime, &mut transport, &cluster, &mut resolver,
///     target, "handle_msg", &[Value::int(42)],
/// );
/// ```
pub trait DistributedRuntime {
    /// Send a message to an actor using a location-transparent address.
    ///
    /// If the actor is local, delegates to the normal [`Runtime::send_message`].
    /// If the actor is remote, serializes the message and sends it
    /// over the network transport.
    fn send_distributed(
        &mut self,
        transport: &mut dyn NetworkTransport,
        cluster: &ClusterState,
        resolver: &mut AddressResolver,
        target: ActorAddress,
        behavior: &str,
        args: &[Value],
    );

    /// Process incoming network packets.
    ///
    /// Reads all packets from the transport and delivers actor messages
    /// to their target actors. Handles heartbeats by forwarding to the
    /// cluster state, merges gossip, and answers spawn requests (which is
    /// why the transport is taken mutably: replies go back over the wire).
    fn process_network_packets(
        &mut self,
        transport: &mut dyn NetworkTransport,
        cluster: &mut ClusterState,
        resolver: &mut AddressResolver,
    );

    /// Spawn an actor on a specific node.
    ///
    /// If `node` is the local node, behaves like normal spawn.
    /// If `node` is remote, sends a spawn request over the network.
    fn spawn_on_node(
        &mut self,
        transport: &mut dyn NetworkTransport,
        node: NodeId,
        behavior_name: &str,
        initial_state: Vec<(String, Value)>,
    ) -> ActorAddress;
}

// ---------------------------------------------------------------------------
// Concrete implementation via wrapper struct
//
// Since Runtime is defined in mod.rs and we can't add trait impls to it
// from here without orphan rules issues, we provide a wrapper struct
// that holds references to all the components.  This is the pattern
// recommended for integration (Stage B1).
// ---------------------------------------------------------------------------

/// Lightweight wrapper that holds a mutable reference to the [`Runtime`].
///
/// Used to implement the [`DistributedRuntime`] trait without consuming
/// the runtime. The other distributed components (transport, cluster,
/// resolver) are passed as parameters to the trait methods, avoiding
/// borrow-checker aliasing issues.
pub struct DistributedRuntimeImpl<'a> {
    pub runtime: &'a mut Runtime,
}

impl<'a> DistributedRuntimeImpl<'a> {
    /// Create a new distributed runtime wrapper.
    pub fn new(runtime: &'a mut Runtime) -> Self {
        DistributedRuntimeImpl { runtime }
    }
}

impl<'a> DistributedRuntime for DistributedRuntimeImpl<'a> {
    fn send_distributed(
        &mut self,
        transport: &mut dyn NetworkTransport,
        cluster: &ClusterState,
        resolver: &mut AddressResolver,
        target: ActorAddress,
        behavior: &str,
        args: &[Value],
    ) {
        send_distributed(
            self.runtime,
            transport,
            cluster,
            resolver,
            target,
            behavior,
            args,
        )
    }

    fn process_network_packets(
        &mut self,
        transport: &mut dyn NetworkTransport,
        cluster: &mut ClusterState,
        resolver: &mut AddressResolver,
    ) {
        process_network_packets(self.runtime, transport, cluster, resolver)
    }

    fn spawn_on_node(
        &mut self,
        _transport: &mut dyn NetworkTransport,
        node: NodeId,
        _behavior_name: &str,
        _initial_state: Vec<(String, Value)>,
    ) -> ActorAddress {
        // The trait method doesn't receive cluster/resolver, so we can
        // only handle local spawns here. For remote spawns, use the
        // `spawn_on_node` free function which takes all components.
        if node == NodeId::LOCAL {
            // Local spawn would need the behavior_name and initial_state.
            // This is a limitation of the trait API — use the free function.
            ActorAddress::local(0)
        } else {
            // Cannot determine if node is local without resolver.
            // Return a placeholder; callers should use the free function.
            ActorAddress::remote(node, 0)
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions (simpler integration alternative)
// ---------------------------------------------------------------------------

/// Send a message to an actor using a location-transparent address.
///
/// This is the simplest way to send a distributed message — no trait
/// objects or wrapper structs needed.
///
/// # Example
///
/// ```ignore
/// use nulang_runtime::distributed::{ActorAddress, send_distributed};
///
/// let target = ActorAddress::remote(remote_node, actor_id);
/// send_distributed(&mut runtime, &mut transport, &cluster, &mut resolver,
///                  target, "handle", &[Value::int(42)]);
/// ```
pub fn send_distributed(
    runtime: &mut Runtime,
    transport: &mut dyn NetworkTransport,
    cluster: &ClusterState,
    resolver: &mut AddressResolver,
    target: ActorAddress,
    behavior: &str,
    args: &[Value],
) {
    match resolver.resolve(cluster, target) {
        ResolveResult::Local { actor_id } => {
            runtime.send_message(actor_id, behavior, args);
        }
        ResolveResult::Remote { node_id, actor_id } => {
            // Remember the bare id → node mapping so a LATER bare actor-ref
            // Value (no node id) can route here too (RFC-0007).
            crate::runtime::distribution::record_remote_ref(runtime, node_id, actor_id);
            // String payloads must cross the wire by CONTENT: a bare string
            // id indexes the sender's module constant pool and means nothing
            // (or the wrong thing) on the receiving node. Resolve each
            // string arg against the sender's pool and carry the contents
            // in the packet's string table.
            let (payload, string_table) = match resolve_wire_strings(runtime, args) {
                Some(resolved) => resolved,
                None => {
                    warn!(
                        "nulang-net: dropping message to actor {} on node {:?}: string payload cannot be resolved to content (no sender module context)",
                        actor_id, node_id
                    );
                    let sender = runtime.current_actor.unwrap_or(0);
                    notify_delivery_failed(runtime, sender, "string payload unresolvable");
                    return;
                }
            };
            // Object-store refs must cross the wire with their byte payloads:
            // object ids are local to each node's store.
            let (payload, object_table) = match resolve_wire_objects(runtime, &payload) {
                Some(resolved) => resolved,
                None => {
                    warn!(
                        "nulang-net: dropping message to actor {} on node {:?}: object ref not found in local store",
                        actor_id, node_id
                    );
                    let sender = runtime.current_actor.unwrap_or(0);
                    notify_delivery_failed(runtime, sender, "object ref unresolvable");
                    return;
                }
            };
            // Remote sends carry the behavior name and an optional content
            // hash; the receiving node resolves the name and MAY verify the
            // hash against its own behavior table on delivery.
            let content_hash = try_lookup_content_hash(runtime, behavior);
            // Carry the current handler's trace span across the wire so the
            // remote side continues the same causal chain. The current span
            // (not a synthetic child) crosses because `traceparent` has no
            // parent field — the receiver creates its own child span.
            let trace_id = runtime.current_trace.as_ref().map(|t| t.to_traceparent());
            let packet = resolver.build_packet(
                actor_id,
                behavior,
                payload,
                runtime.current_actor.unwrap_or(0),
                MessagePriority::Normal,
                string_table,
                object_table,
                content_hash,
                trace_id,
            );

            if let Some(node_info) = cluster.get_node(node_id) {
                let net_node_id = NodeId(node_id.0);
                transport.send(net_node_id, node_info.address, packet);
            } else {
                // The node resolved as remote but is no longer in the
                // membership table (it left between resolve and send). Log
                // the drop rather than losing the message silently.
                warn!(
                    "nulang-net: dropping message to actor {} on node {:?}: node missing from cluster membership",
                    actor_id, node_id
                );
                let sender = runtime.current_actor.unwrap_or(0);
                notify_delivery_failed(runtime, sender, "target node left cluster");
            }
        }
        ResolveResult::Unresolvable { reason } => {
            warn!("nulang-net: dropping message to {:?}: {}", target, reason);
            let sender = runtime.current_actor.unwrap_or(0);
            notify_delivery_failed(runtime, sender, &reason);
        }
    }
}

/// Notify a sender that their message could not be delivered.
///
/// Delivers a system message (behavior 0) to the sender actor with a
/// failure code in the payload: `[failure_code: Int, _reserved: Nil]`.
/// Codes: 0=unresolvable, 1=node left cluster, 2=string payload unresolvable,
/// 3=string intern failed on receiver, 4=target actor not found,
/// 6=object ref unresolvable, 7=object intern failed on receiver, 5=unknown.
/// Non-existent senders (id 0) are silently skipped.
pub(crate) fn notify_delivery_failed(runtime: &mut Runtime, sender_id: u64, reason: &str) {
    if sender_id == 0 {
        return;
    }
    if !runtime.actors.get(&sender_id).is_some() {
        return;
    }
    let code = delivery_failure_code(reason);
    let fail_payload = vec![Value::int(code), Value::nil()];
    runtime.send_message_by_id(sender_id, 0, &fail_payload);
}

/// Map a delivery-failure reason string to an integer code.
fn delivery_failure_code(reason: &str) -> i64 {
    match reason {
        "unresolvable" => 0,
        "target node left cluster" => 1,
        "string payload unresolvable" => 2,
        "string intern failed on receiver" => 3,
        "target actor not found" => 4,
        "object ref unresolvable" => 6,
        "object intern failed on receiver" => 7,
        _ => 5,
    }
}

/// Map the wire reason tag (from [`ExitReason::tag`]) back to an
/// [`ExitReason`]. The wire carries only the tag string; the original
/// message/description payload is not transmitted.
fn exit_reason_from_tag(tag: &str) -> ExitReason {
    match tag {
        "normal" => ExitReason::Normal,
        "kill" => ExitReason::Kill,
        "killed" => ExitReason::Killed,
        "shutdown" => ExitReason::Shutdown(None),
        "noconnection" => ExitReason::NoConnection,
        "error" => ExitReason::Error("remote exit".to_string()),
        _ => ExitReason::Custom(tag.to_string()),
    }
}

/// Verify that the target actor's behavior at the given index has a matching
/// content hash. Returns `true` if verification passes (or if the local
/// behavior entry has no hash — backward compatibility preserves nodes whose
/// modules were compiled before content hashing was added).
fn verify_behavior_hash(
    runtime: &Runtime,
    target_actor: u64,
    behavior_id: u16,
    sender_hash: &[u8; 32],
) -> bool {
    let actor = match runtime.actors.get(&target_actor) {
        Some(a) => a,
        None => return false,
    };
    // Check the per-actor behavior table first (native handler).
    if actor.behavior_table.get(behavior_id as usize).is_some() {
        return true; // Native handlers have no hash — accept.
    }
    let module = match &actor.bytecode_module {
        Some(m) => m,
        None => return false,
    };
    let entry = match module.behaviors.get(behavior_id as usize) {
        Some(e) => e,
        None => return false,
    };
    match &entry.content_hash {
        Some(local_hash) => local_hash == sender_hash,
        None => true, // No local hash — backward compatible, accept.
    }
}

/// Try to look up the content hash for a behavior name in the current
/// actor's bytecode module. Returns `None` if no current actor context,
/// no bytecode module, or the behavior has no content hash.
///
/// This is a best-effort lookup: the sender's module may not define the
/// target behavior (cross-module sends), in which case no hash is
/// transmitted and no receiver-side verification is performed.
fn try_lookup_content_hash(runtime: &Runtime, behavior_name: &str) -> Option<[u8; 32]> {
    let actor_id = runtime.current_actor?;
    let actor = runtime.actors.get(&actor_id)?;
    let module = actor.bytecode_module.as_ref()?;
    let matches = |name: &str| {
        name == behavior_name
            || name
                .strip_suffix(behavior_name)
                .is_some_and(|prefix| prefix.ends_with('.'))
    };
    module
        .behaviors
        .iter()
        .find(|b| matches(&b.name))
        .and_then(|b| b.content_hash)
}

/// Process all incoming network packets and deliver actor messages.
///
/// Heartbeats are forwarded to the cluster state, gossip is merged into
/// the membership table, actor messages are parsed and delivered to the
/// target actor's mailbox, and spawn requests are answered with a
/// [`Packet::SpawnResponse`] (hence the mutable transport).
///
/// Send a transport-level acknowledgement for a successfully processed packet.
fn ack_packet(
    transport: &mut dyn NetworkTransport,
    cluster: &ClusterState,
    from_node: NodeId,
    seq: u64,
) {
    let addr = cluster
        .get_node(from_node)
        .map(|n| n.address)
        .or_else(|| transport.connection_addr(from_node));
    if let Some(addr) = addr {
        transport.send(from_node, addr, Packet::Ack { packet_seq: seq });
    }
}

pub fn process_network_packets(
    runtime: &mut Runtime,
    transport: &mut dyn NetworkTransport,
    cluster: &mut ClusterState,
    resolver: &mut AddressResolver,
) {
    let packets = transport.receive();
    for incoming in packets {
        match incoming.packet {
            Packet::Heartbeat { node_id, .. } => {
                let cluster_node_id = NodeId(node_id.0);
                // The IncomingPacket doesn't carry the sender's address
                // directly, so prefer the address already recorded in the
                // membership table. For a previously-unknown node (e.g. a
                // fresh joiner's first heartbeat to its seed) fall back to
                // the transport's connection table — this is the discovery
                // path by which a seed first learns about a joiner.
                let known_addr = cluster
                    .get_node(cluster_node_id)
                    .map(|info| info.address)
                    .or_else(|| transport.connection_addr(cluster_node_id));
                if let Some(addr) = known_addr {
                    cluster.handle_heartbeat(cluster_node_id, addr);
                }
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::Gossip { members, directory } => {
                cluster.merge_membership_from_sender(members, incoming.from_node);
                if !directory.is_empty() {
                    cluster.merge_directory(directory);
                    // A re-joined node may have been replaced while away:
                    // demote any local actor whose directory epoch is newer.
                    runtime.self_demote_superseded();
                }
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::SpawnRequest {
                request_id,
                behavior_name,
                initial_state,
                bytecode: _,
                content_hash: _,
            } => {
                // MVP: remote spawn only supports behaviors the receiving
                // runtime has explicitly registered via
                // `Runtime::register_spawnable_behavior`. An unknown name
                // replies `success: false` — the tolerated-no-crash
                // counterpart of local send's unknown-behavior fallback.
                let handler = runtime.spawnable_behaviors.get(&behavior_name).copied();
                let (actor_id, success) = match handler {
                    Some(handler) => {
                        let id = runtime.spawn_actor(Box::new(move || initial_state));
                        if let Some(actor) = runtime.actors.get_mut(&id) {
                            actor.register_behavior(behavior_name, handler);
                        }
                        (id, true)
                    }
                    None => (0, false),
                };
                let reply = Packet::SpawnResponse {
                    request_id,
                    actor_id,
                    success,
                };
                let from = incoming.from_node;
                let reply_addr = cluster
                    .get_node(from)
                    .map(|info| info.address)
                    .or_else(|| transport.connection_addr(from));
                if let Some(addr) = reply_addr {
                    transport.send(from, addr, reply);
                }
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::SpawnResponse {
                request_id,
                actor_id,
                success,
            } => {
                // Record the outcome so the requester can learn the real
                // remote actor id (`Runtime::take_spawn_response`). The
                // id carried by spawn_on_node's placeholder address is the
                // request id, not a usable actor id.
                runtime
                    .pending_spawn_responses
                    .insert(request_id, if success { Some(actor_id) } else { None });
                let from = incoming.from_node;
                // The placeholder resolves (success) or dies (failure); in
                // either case it stops being pending.
                runtime.spawn_placeholders.remove(&request_id);
                if success {
                    // The placeholder VALUE the program holds keeps
                    // routing to the real actor id even if the application
                    // consumed the response via take_spawn_response.
                    runtime.spawn_translations.insert(request_id, actor_id);
                    // The placeholder VALUE the program holds (request id)
                    // now translates to the real actor id via
                    // pending_spawn_responses; also map the real id so
                    // direct refs route. The request-id → node entry was
                    // recorded at spawn time.
                    crate::runtime::distribution::record_remote_ref(runtime, from, actor_id);
                }
                // Flush any messages queued against the placeholder.
                if let Some(msgs) = runtime.pending_spawn_messages.remove(&request_id) {
                    if success {
                        for m in msgs {
                            let content_hash = try_lookup_content_hash(runtime, &m.behavior_name);
                            let packet = resolver.build_packet(
                                actor_id,
                                &m.behavior_name,
                                m.payload,
                                m.sender,
                                crate::runtime::mailbox::MessagePriority::Normal,
                                m.string_table,
                                m.object_table,
                                content_hash,
                                m.trace_id,
                            );
                            let reply_addr = cluster
                                .get_node(from)
                                .map(|info| info.address)
                                .or_else(|| transport.connection_addr(from));
                            if let Some(addr) = reply_addr {
                                transport.send(from, addr, packet);
                            } else {
                                warn!(
                                    "nulang-net: dropping queued message for spawned actor {}: node {:?} left cluster",
                                    actor_id, from
                                );
                                notify_delivery_failed(
                                    runtime,
                                    m.sender,
                                    "target node left cluster",
                                );
                            }
                        }
                    } else {
                        for m in msgs {
                            notify_delivery_failed(runtime, m.sender, "spawn request rejected");
                        }
                    }
                }
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::CrdtSync { ops } => {
                if let Some(manager) = &mut runtime.crdt_manager {
                    for op in ops.iter() {
                        manager.apply_op(op.clone());
                    }
                }
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::CrdtDeltaSync { ops } => {
                // Delta ops merge into entries this node already holds;
                // full-state-tagged ops behave exactly like CrdtSync above
                // (including entry creation on first sight).
                if let Some(manager) = &mut runtime.crdt_manager {
                    for op in ops.iter() {
                        manager.apply_delta_op(op.clone());
                    }
                }
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::Ack { packet_seq } => {
                runtime.acked_packets.insert(packet_seq);
            }
            Packet::FetchBehaviorRequest { content_hash } => {
                let mut nbc_bytes: Option<Vec<u8>> = None;
                let mut behavior_name = String::new();
                // Search recovery modules for a matching content hash
                for (_, (module, _, _)) in &runtime.recovery_modules {
                    for entry in &module.behaviors {
                        if entry.content_hash == Some(content_hash) {
                            match module.to_nbc(None) {
                                Ok(bytes) => {
                                    nbc_bytes = Some(bytes);
                                    behavior_name = entry.name.clone();
                                }
                                Err(_) => {}
                            }
                            break;
                        }
                    }
                    if nbc_bytes.is_some() {
                        break;
                    }
                }
                // Also check the behavior cache
                if nbc_bytes.is_none() {
                    if let Some(cached) = runtime.behavior_cache.get(&content_hash) {
                        match cached.to_nbc(None) {
                            Ok(bytes) => {
                                nbc_bytes = Some(bytes);
                                behavior_name = cached
                                    .behaviors
                                    .first()
                                    .map(|b| b.name.clone())
                                    .unwrap_or_default();
                            }
                            Err(_) => {}
                        }
                    }
                }
                let reply = Packet::FetchBehaviorResponse {
                    content_hash,
                    behavior_name,
                    nbc_bytes,
                };
                let from = incoming.from_node;
                let reply_addr = cluster
                    .get_node(from)
                    .map(|info| info.address)
                    .or_else(|| transport.connection_addr(from));
                if let Some(addr) = reply_addr {
                    transport.send(from, addr, reply);
                }
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::FetchBehaviorResponse {
                content_hash,
                behavior_name,
                nbc_bytes,
            } => {
                if let Some(bytes) = nbc_bytes {
                    match crate::bytecode::CodeModule::from_nbc(&bytes) {
                        Ok(artifact) => {
                            runtime.behavior_cache.insert(content_hash, artifact.module);
                            // Retry any messages that were waiting for this bytecode
                            let pending = runtime.pending_fetched_messages.remove(&content_hash);
                            if let Some(messages) = pending {
                                for (
                                    target_actor,
                                    behavior_name,
                                    mut msg,
                                    string_table,
                                    object_table,
                                ) in messages
                                {
                                    // Hot-reload the newly cached module into the target actor
                                    let cached = match runtime.behavior_cache.get(&content_hash) {
                                        Some(c) => c.clone(),
                                        None => {
                                            notify_delivery_failed(
                                                runtime,
                                                msg.sender,
                                                "bytecode fetch succeeded but cache miss on retry",
                                            );
                                            continue;
                                        }
                                    };
                                    hot_reload_behavior(
                                        runtime,
                                        target_actor,
                                        &cached,
                                        &behavior_name,
                                    );
                                    // Resolve behavior_id against the updated module
                                    msg.behavior_id = runtime
                                        .behavior_id_for(target_actor, &behavior_name)
                                        .unwrap_or(0);
                                    // Verify the hash now matches
                                    if !verify_behavior_hash(
                                        runtime,
                                        target_actor,
                                        msg.behavior_id,
                                        &content_hash,
                                    ) {
                                        notify_delivery_failed(
                                            runtime,
                                            msg.sender,
                                            "behavior content hash still mismatched after fetch",
                                        );
                                        continue;
                                    }
                                    // Intern string and object payloads, then deliver
                                    let mut payload_vec = (*msg.payload).clone();
                                    if !intern_wire_strings(
                                        runtime,
                                        target_actor,
                                        &mut payload_vec,
                                        &string_table,
                                    ) {
                                        notify_delivery_failed(
                                            runtime,
                                            msg.sender,
                                            "string intern failed on retry",
                                        );
                                        continue;
                                    }
                                    if !intern_wire_objects(
                                        runtime,
                                        &mut payload_vec,
                                        &object_table,
                                    ) {
                                        notify_delivery_failed(
                                            runtime,
                                            msg.sender,
                                            "object intern failed on retry",
                                        );
                                        continue;
                                    }
                                    msg.payload = Arc::new(payload_vec);
                                    if let Some(actor) = runtime.actors.get_mut(&target_actor) {
                                        let _ = actor.mailbox.push(msg);
                                        runtime.scheduler.enqueue(target_actor);
                                    } else {
                                        notify_delivery_failed(
                                            runtime,
                                            msg.sender,
                                            "target actor not found on retry",
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                "nulang-net: failed to deserialize fetched bytecode for '{}': {}",
                                behavior_name, e
                            );
                            // Drop pending messages for this hash on deserialize failure
                            let pending = runtime.pending_fetched_messages.remove(&content_hash);
                            if let Some(messages) = pending {
                                for (_, _, msg, _, _) in messages {
                                    notify_delivery_failed(
                                        runtime,
                                        msg.sender,
                                        "bytecode fetch failed: deserialization error",
                                    );
                                }
                            }
                        }
                    }
                } else {
                    // Sender didn't have the requested bytecode; drain and notify
                    let pending = runtime.pending_fetched_messages.remove(&content_hash);
                    if let Some(messages) = pending {
                        for (_, _, msg, _, _) in messages {
                            notify_delivery_failed(runtime, msg.sender, "bytecode fetch failed: sender does not have the requested behavior");
                        }
                    }
                }
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::CrdtOp { op } => {
                if let Some(manager) = &mut runtime.crdt_manager {
                    manager.apply_op(op);
                }
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::Link { watcher, target } => {
                // A remote actor (watcher, on the sending node) linked to a
                // local actor (target). Register so the local exit protocol
                // can notify the remote watcher when the target exits.
                runtime.remote_links.register(target, watcher);
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::Monitor { watcher, target } => {
                // Same registration for the asymmetric monitor direction.
                runtime.remote_monitors.register(target, watcher);
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::Down { target, reason } => {
                // A remote actor exited (normal exit or crash). Deliver DOWN
                // to every local actor that had linked/monitored it.
                let reason = exit_reason_from_tag(&reason);
                let local_node = runtime.distributed.node_id.unwrap_or(NodeId::LOCAL);
                let local_link_watchers: Vec<u64> = runtime
                    .remote_links
                    .get_watchers(target)
                    .map(|ws| {
                        ws.iter()
                            .filter(|w| w.node_id == local_node)
                            .map(|w| w.actor_id)
                            .collect()
                    })
                    .unwrap_or_default();
                let local_monitor_watchers: Vec<u64> = runtime
                    .remote_monitors
                    .get_watchers(target)
                    .map(|ws| {
                        ws.iter()
                            .filter(|w| w.node_id == local_node)
                            .map(|w| w.actor_id)
                            .collect()
                    })
                    .unwrap_or_default();
                for watcher_id in local_link_watchers {
                    crate::runtime::exit::send_down_message(
                        runtime,
                        watcher_id,
                        target.actor_id,
                        &reason,
                    );
                }
                for watcher_id in local_monitor_watchers {
                    crate::runtime::exit::send_down_message(
                        runtime,
                        watcher_id,
                        target.actor_id,
                        &reason,
                    );
                }
                runtime.remote_links.clear_target(target);
                runtime.remote_monitors.clear_target(target);
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::MigrateActor {
                actor_id,
                nbc_bytes,
                snapshot_json,
            } => {
                let _ = runtime.receive_migrated_actor(actor_id, nbc_bytes, snapshot_json);
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            Packet::NodeGoodbye { node_id, durable } => {
                // RFC 0014 §1 path 1: the sender declares its durable actors
                // checkpointed and terminated. Fold its manifest into the
                // directory (highest-epoch-wins) and confirm-gone it, then
                // drive re-spawn.
                if !durable.is_empty() {
                    let node = NodeId(node_id.0);
                    cluster.merge_directory(
                        durable
                            .into_iter()
                            .map(|(actor_id, epoch)| {
                                crate::runtime::cluster::DurableDirectoryEntry {
                                    actor_id,
                                    node_id: node,
                                    epoch,
                                }
                            })
                            .collect(),
                    );
                }
                let node = NodeId(node_id.0);
                let newly = cluster.mark_removed(node);
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
                if newly {
                    crate::runtime::distribution::handle_node_removed(runtime, node);
                }
            }
            Packet::ShadowReplicate {
                actor_id,
                nbc_bytes,
                snapshot_json,
                epoch,
            } => {
                // RFC 0014 §3: store the replica, do NOT instantiate it.
                // The shadow re-spawns from it only on confirmed removal.
                runtime.store_shadow_replica(actor_id, nbc_bytes, snapshot_json, epoch);
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
            _ => {
                if let Some((
                    target_actor,
                    behavior_name,
                    mut msg,
                    string_table,
                    object_table,
                    content_hash,
                )) = resolver.parse_packet(incoming.packet)
                {
                    // Record the wire sender (bare id → node) so the
                    // recipient can reply BY VALUE (RFC-0007): a later
                    // `send <sender_ref>` on this node routes over the
                    // wire instead of the local mailbox. parse_packet
                    // already cached the sender in the forward
                    // RemoteActorCache for explicit-address sends.
                    if msg.sender != 0 {
                        crate::runtime::distribution::record_remote_ref(
                            runtime,
                            incoming.from_node,
                            msg.sender,
                        );
                    }
                    // Resolve the behavior name against the target actor's
                    // behavior table — the same rule local sends use
                    // (`Runtime::send_message`). An unknown name falls back
                    // to behavior 0, mirroring `send_message`'s
                    // `unwrap_or(0)`.
                    msg.behavior_id = runtime
                        .behavior_id_for(target_actor, &behavior_name)
                        .unwrap_or(0);
                    // If the sender attached a content hash, verify it
                    // against the local behavior table.
                    if let Some(sender_hash) = content_hash {
                        if !verify_behavior_hash(
                            runtime,
                            target_actor,
                            msg.behavior_id,
                            &sender_hash,
                        ) {
                            // Check if we already have the correct bytecode cached
                            let cached_module = runtime.behavior_cache.get(&sender_hash).cloned();
                            if let Some(cached) = cached_module {
                                // Hot-reload: install the cached module
                                hot_reload_behavior(runtime, target_actor, &cached, &behavior_name);
                                // Retry resolution after hot-reload
                                msg.behavior_id = runtime
                                    .behavior_id_for(target_actor, &behavior_name)
                                    .unwrap_or(0);
                            } else {
                                // Request the bytecode from the sender
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
                                // Enqueue message for retry after fetch completes
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
                                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
                                continue;
                            }
                        }
                    }
                    // Intern string payloads into the TARGET actor's module
                    // pool — on this (scheduler) thread, never in a network
                    // reader thread. The ids on the wire index the packet's
                    // string table; a message whose strings cannot be
                    // interned is dropped rather than delivered with
                    // dangling pool ids.
                    // Clone the Arc payload into a mutable Vec, intern the
                    // strings, then wrap the result back into a fresh Arc.
                    let mut payload_vec = (*msg.payload).clone();
                    if !intern_wire_strings(runtime, target_actor, &mut payload_vec, &string_table)
                    {
                        warn!(
                            "nulang-net: dropping message to actor {}: string payload cannot be interned (target actor missing or has no module pool)",
                            target_actor
                        );
                        notify_delivery_failed(
                            runtime,
                            msg.sender,
                            "string intern failed on receiver",
                        );
                    }
                    if !intern_wire_objects(runtime, &mut payload_vec, &object_table) {
                        warn!(
                            "nulang-net: dropping message to actor {}: object payload cannot be interned (object table mismatch)",
                            target_actor
                        );
                        notify_delivery_failed(
                            runtime,
                            msg.sender,
                            "object intern failed on receiver",
                        );
                    }
                    msg.payload = Arc::new(payload_vec);
                    if let Some(actor) = runtime.actors.get_mut(&target_actor) {
                        let _ = actor.mailbox.push(msg);
                        runtime.scheduler.enqueue(target_actor);
                    } else {
                        notify_delivery_failed(runtime, msg.sender, "target actor not found");
                    }
                }
                ack_packet(transport, cluster, incoming.from_node, incoming.seq);
            }
        }
    }
}

/// Hot-reload a cached `CodeModule` into a target actor's bytecode module.
///
/// Called when a previously-fetched module matches the sender's content hash.
/// The actor's `bytecode_module` and `bytecode_offsets` are replaced so
/// subsequent `behavior_id_for` lookups resolve against the updated module.
fn hot_reload_behavior(
    runtime: &mut Runtime,
    actor_id: u64,
    module: &crate::bytecode::CodeModule,
    _behavior_name: &str,
) {
    let actor = match runtime.actors.get_mut(&actor_id) {
        Some(a) => a,
        None => return,
    };
    // Rebuild bytecode offsets from the cached module (compressed to the
    // actor's own behaviors for workflow actors — local step ids).
    let offsets = crate::runtime::spawn::bytecode_offsets_for(module, actor.is_workflow);
    actor.bytecode_module = Some(module.clone());
    actor.bytecode_offsets = offsets;
    warn!(
        "nulang-net: hot-reloaded bytecode module for actor {} ({} behaviors)",
        actor_id,
        module.behaviors.len()
    );
}
/// Broadcast delta-state CRDT sync ops to all healthy cluster members.
///
/// This is the delta-state counterpart of `Runtime::sync_crdts`: entries
/// that have never been synced ship as full-state ops (the join fallback),
/// all others ship only the changes since the last call, and unchanged
/// entries ship nothing. Receivers apply the packet via
/// [`CrdtManager::apply_delta_op`](crate::runtime::crdt_manager::CrdtManager::apply_delta_op).
/// The full-state `Packet::CrdtSync` path remains available for
/// join/reset and as the repair mechanism after message loss.
pub fn sync_crdts_delta(runtime: &mut Runtime) {
    if !runtime.distributed.enabled {
        return;
    }
    let ops = match &mut runtime.crdt_manager {
        Some(m) => m.generate_delta_sync_ops(),
        None => return,
    };
    if ops.is_empty() {
        return;
    }
    let packet = Packet::CrdtDeltaSync { ops: Arc::new(ops) };
    if let Some(cluster) = &runtime.distributed.cluster {
        for member in cluster.healthy_members() {
            if let Some(transport) = &mut runtime.distributed.transport {
                let net_node_id = NodeId(member.node_id.0);
                transport.send(net_node_id, member.address, packet.clone());
            }
        }
    }
}

/// Synchronize CRDT state via op-based replication.
///
/// Ships individual [`CrdtOp`](crate::runtime::crdt_manager::CrdtOp)s as
/// `Packet::CrdtOp` — the lowest-bandwidth sync path. Each changed entry
/// becomes one packet. The full-state `CrdtSync` and delta `CrdtDeltaSync`
/// remain as the join/repair fallback.
pub fn sync_crdts_op(runtime: &mut Runtime) {
    let ops = match &mut runtime.crdt_manager {
        Some(m) => m.generate_op_syncs(),
        None => return,
    };
    if ops.is_empty() {
        return;
    }
    let Some(cluster) = &runtime.distributed.cluster else {
        return;
    };
    for op in ops {
        let packet = Packet::CrdtOp { op };
        if let Some(transport) = &mut runtime.distributed.transport {
            for member in cluster.healthy_members() {
                let net_node_id = NodeId(member.node_id.0);
                transport.send(net_node_id, member.address, packet.clone());
            }
        }
    }
}

/// Spawn an actor on a specific node.
///
/// Local spawns go through [`Runtime::spawn_actor`]. Remote spawns send
/// a [`Packet::SpawnRequest`] over the network and return a **placeholder**
/// address whose `actor_id` is the request id — the receiving node answers
/// with a [`Packet::SpawnResponse`] carrying the real actor id, which the
/// requester picks up via [`Runtime::take_spawn_response`] after pumping
/// `Runtime::process_network`. Remote spawn only supports behaviors the
/// receiving node registered with `Runtime::register_spawnable_behavior`;
/// anything else is rejected with `success: false`.
pub fn spawn_on_node(
    runtime: &mut Runtime,
    transport: &mut dyn NetworkTransport,
    cluster: &ClusterState,
    resolver: &AddressResolver,
    node: NodeId,
    behavior_name: &str,
    initial_state: Vec<(String, Value)>,
) -> ActorAddress {
    if node == resolver.local_node() || node == NodeId::LOCAL {
        let id = runtime.spawn_actor(Box::new(move || initial_state));
        ActorAddress::local(id)
    } else {
        let request_id = fast_random_u64();
        // The placeholder VALUE the program will hold carries only this
        // request id — record id → node and tag it pending so sends to
        // the ref queue until the SpawnResponse arrives (RFC-0007).
        crate::runtime::distribution::record_remote_ref(runtime, node, request_id);
        runtime.spawn_placeholders.insert(request_id);
        let packet = Packet::SpawnRequest {
            request_id,
            behavior_name: behavior_name.to_string(),
            content_hash: None,
            initial_state,
            bytecode: None,
        };

        if let Some(node_info) = cluster.get_node(node) {
            let net_node_id = NodeId(node.0);
            transport.send(net_node_id, node_info.address, packet);
        } else {
            let sender = runtime.current_actor.unwrap_or(0);
            notify_delivery_failed(runtime, sender, "spawn target node not in cluster");
        }

        ActorAddress::remote(node, request_id)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A fast, non-cryptographic random u64 for request IDs.
///
/// Uses a simple xorshift — good enough for spawn request IDs.
fn fast_random_u64() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0x1234_5678_9ABC_DEF0);
    let mut x = COUNTER.fetch_add(1, Ordering::Relaxed);
    // xorshift64*
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

// ---------------------------------------------------------------------------
// Cross-node string payloads
// ---------------------------------------------------------------------------

/// Resolve string-id values in `args` to their UTF-8 content for a remote
/// send, returning the payload rewritten to index a per-packet string table
/// plus the table itself.
///
/// String ids index the SENDER's module constant pool (the runtime VM module
/// of the actor currently being stepped), so the content — never the id — is
/// what may cross the wire. Returns `None` when a string value has no
/// resolvable content (no current actor, no sender module, or an id outside
/// the pool); the caller must drop the message rather than send a dangling
/// id.
pub(crate) fn resolve_wire_strings(
    runtime: &Runtime,
    args: &[Value],
) -> Option<(Vec<Value>, Vec<String>)> {
    let mut payload = args.to_vec();
    let mut table: Vec<String> = Vec::new();
    for value in payload.iter_mut() {
        let Some(id) = value.as_string_id() else {
            continue;
        };
        let content = resolve_sender_string(runtime, id)?;
        // Reuse a table entry for repeated content within one packet.
        let idx = match table.iter().position(|s| s == &content) {
            Some(i) => i,
            None => {
                table.push(content);
                table.len() - 1
            }
        };
        *value = Value::string(idx as u32);
    }
    Some((payload, table))
}

/// Resolve a sender-pool string id to its content via the current actor's
/// module in the runtime VM.
fn resolve_sender_string(runtime: &Runtime, id: u32) -> Option<String> {
    let sender = runtime.current_actor?;
    let module_idx = runtime.actors.get(&sender)?.bytecode_module_idx?;
    runtime.vm.as_ref()?.constant_string(module_idx, id)
}

/// Intern the string payloads of a received remote message into the target
/// actor's module constant pool, rewriting each string-id value from a
/// packet string table index to its new pool index.
///
/// Must run on the scheduler thread — [`crate::vm::VM::add_runtime_string`]
/// is `&mut self` and the VM is strictly single-threaded. Returns `true`
/// when every string value was interned; `false` when the target actor has
/// no module pool to intern into or a payload id falls outside the table,
/// in which case the caller drops the message rather than deliver a
/// dangling id.
fn intern_wire_strings(
    runtime: &mut Runtime,
    target_actor: u64,
    payload: &mut [Value],
    string_table: &[String],
) -> bool {
    if !payload.iter().any(|v| v.is_string()) {
        return true;
    }
    let Some(module_idx) = ensure_actor_module_idx(runtime, target_actor) else {
        return false;
    };
    let vm = runtime.vm.as_mut().expect("a module index implies a VM");
    for value in payload.iter_mut() {
        let Some(id) = value.as_string_id() else {
            continue;
        };
        let Some(content) = string_table.get(id as usize) else {
            return false;
        };
        *value = intern_pool_string(vm, module_idx, content);
    }
    true
}

/// The module pool index that string payloads for `actor_id` must be
/// interned into: the actor's already-loaded module, or — before its first
/// turn — its bytecode module loaded into the runtime VM, mirroring the
/// lazy load in `Runtime::run_bytecode_at_offset`. `None` for actors
/// without a bytecode module (native handlers have no constant pool).
fn ensure_actor_module_idx(runtime: &mut Runtime, actor_id: u64) -> Option<usize> {
    if let Some(idx) = runtime
        .actors
        .get(&actor_id)
        .and_then(|a| a.bytecode_module_idx)
    {
        return Some(idx);
    }
    let module = runtime
        .actors
        .get(&actor_id)
        .and_then(|a| a.bytecode_module.clone())?;
    if runtime.vm.is_none() {
        runtime.vm = Some(crate::vm::VM::new());
    }
    let vm = runtime.vm.as_mut().expect("VM was just ensured");
    let idx = vm.modules.len();
    vm.load_module(module.clone());
    runtime.register_module_grains(&module);
    if let Some(actor) = runtime.actors.get_mut(&actor_id) {
        actor.bytecode_module_idx = Some(idx);
    }
    Some(idx)
}

/// Intern `content` into module `module_idx`'s constant pool, reusing an
/// existing entry when the text is already present so repeated messages
/// cannot grow the pool without bound.
fn intern_pool_string(vm: &mut crate::vm::VM, module_idx: usize, content: &str) -> Value {
    if let Some(module) = vm.modules.get(module_idx) {
        if let Some(pos) = module
            .constants
            .iter()
            .position(|c| matches!(c, crate::bytecode::Constant::String(s) if s == content))
        {
            return Value::string(pos as u32);
        }
    }
    vm.add_runtime_string(module_idx, content.to_string())
}

/// Resolve object-store ids in `args` to byte payloads.
///
/// Returns the payload with object ids rewritten to packet-local indices, plus
/// the table of byte payloads.  Object ids that do not exist in the local
/// `ObjectStore` are dropped (the packet is rejected).
pub(crate) fn resolve_wire_objects(
    runtime: &Runtime,
    args: &[Value],
) -> Option<(Vec<Value>, Vec<(u64, Vec<u8>)>)> {
    let mut payload = args.to_vec();
    let mut table: Vec<(u64, Vec<u8>)> = Vec::new();
    for value in payload.iter_mut() {
        let Some(id) = value.as_object_id() else {
            continue;
        };
        let entry = runtime.object_store.get(id)?;
        let bytes = entry.as_bytes().to_vec();
        // Reuse a table entry for repeated ids within one packet.
        let idx = match table.iter().position(|(tid, _)| *tid == id) {
            Some(i) => i,
            None => {
                table.push((id, bytes));
                table.len() - 1
            }
        };
        *value = Value::object(idx as u64);
    }
    Some((payload, table))
}

/// Intern object payloads of a received remote message into the local
/// `ObjectStore`, rewriting each object-id value from a packet object-table
/// index to its new local object id.  Returns `true` on success; `false` when
/// a payload id falls outside the table, in which case the caller drops the
/// message.
fn intern_wire_objects(
    runtime: &mut Runtime,
    payload: &mut [Value],
    object_table: &[(u64, Vec<u8>)],
) -> bool {
    if !payload.iter().any(|v| v.is_object()) {
        return true;
    }
    for value in payload.iter_mut() {
        let Some(idx) = value.as_object_id() else {
            continue;
        };
        let Some((_, bytes)) = object_table.get(idx as usize) else {
            return false;
        };
        let local_id = runtime.object_store.put(bytes.clone().into_boxed_slice());
        *value = Value::object(local_id);
    }
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    /// Helper: create a loopback address on a given port.
    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    /// Helper: create a minimal cluster state with the local node and
    /// optionally one peer.
    fn make_cluster(local_port: u16) -> ClusterState {
        let a = addr(local_port);
        let local = NodeId::new(&a);
        ClusterState::new(local, a)
    }

    /// Helper: add a healthy peer to a cluster state.
    fn add_peer(cluster: &mut ClusterState, peer_port: u16) -> NodeId {
        let peer_addr = addr(peer_port);
        let peer_id = NodeId::new(&peer_addr);
        cluster.handle_heartbeat(peer_id, peer_addr);
        peer_id
    }

    // -- 1. Local ActorAddress ----------------------------------------------

    #[test]
    fn test_actor_address_local() {
        let a = ActorAddress::local(42);
        assert!(a.is_local());
        assert!(!a.is_remote());
        assert_eq!(a.actor_id(), 42);
        assert_eq!(a.node_id(), NodeId::LOCAL);
    }

    // -- 2. Remote ActorAddress ---------------------------------------------

    #[test]
    fn test_actor_address_remote() {
        let node = NodeId(7);
        let a = ActorAddress::remote(node, 99);
        assert!(a.is_remote());
        assert!(!a.is_local());
        assert_eq!(a.actor_id(), 99);
        assert_eq!(a.node_id(), node);
    }

    // -- 3. actor_id accessor is consistent ----------------------------------

    #[test]
    fn test_actor_address_actor_id() {
        let local = ActorAddress::local(100);
        let remote = ActorAddress::remote(NodeId(3), 100);
        assert_eq!(local.actor_id(), remote.actor_id());
        assert_eq!(local.actor_id(), 100);
    }

    // -- 4. Cache basic put / get --------------------------------------------

    #[test]
    fn test_remote_cache_put_get() {
        let mut cache = RemoteActorCache::with_defaults();
        assert!(cache.is_empty());

        cache.put(NodeId(1), 10);
        assert_eq!(cache.len(), 1);

        let info = cache.get(NodeId(1), 10);
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.node_id, NodeId(1));
        assert_eq!(info.actor_id, 10);
    }

    // -- 5. LRU eviction when full -------------------------------------------

    #[test]
    fn test_remote_cache_lru_eviction() {
        let mut cache = RemoteActorCache::new(3, Duration::from_secs(3600));

        cache.put(NodeId(1), 10);
        cache.put(NodeId(2), 20);
        cache.put(NodeId(3), 30);
        assert_eq!(cache.len(), 3);

        // Access (1,10) to make it most-recently used.
        let _ = cache.get(NodeId(1), 10);

        // Insert a 4th entry — should evict (2,20) since it's now LRU.
        cache.put(NodeId(4), 40);
        assert_eq!(cache.len(), 3);

        assert!(
            cache.get(NodeId(1), 10).is_some(),
            "(1,10) should still be cached"
        );
        assert!(
            cache.get(NodeId(3), 30).is_some(),
            "(3,30) should still be cached"
        );
        assert!(
            cache.get(NodeId(4), 40).is_some(),
            "(4,40) should be cached"
        );
        assert!(
            cache.get(NodeId(2), 20).is_none(),
            "(2,20) should have been evicted"
        );
    }

    // -- 6. Resolver: local address ------------------------------------------

    #[test]
    fn test_resolver_local_address() {
        let local_addr = addr(9000);
        let local_node = NodeId::new(&local_addr);
        let cluster = make_cluster(9000);
        let mut resolver = AddressResolver::new(local_node);

        let address = ActorAddress::local(42);
        let result = resolver.resolve(&cluster, address);

        assert_eq!(result, ResolveResult::Local { actor_id: 42 });
        assert_eq!(resolver.stats().local_resolves, 1);
    }

    // -- 7. Resolver: remote healthy address ---------------------------------

    #[test]
    fn test_resolver_remote_address() {
        let mut cluster = make_cluster(9000);
        let peer_id = add_peer(&mut cluster, 9001);

        let local_addr = addr(9000);
        let local_node = NodeId::new(&local_addr);
        let mut resolver = AddressResolver::new(local_node);

        let address = ActorAddress::remote(peer_id, 55);
        let result = resolver.resolve(&cluster, address);

        assert_eq!(
            result,
            ResolveResult::Remote {
                node_id: peer_id,
                actor_id: 55
            }
        );
        assert_eq!(resolver.stats().remote_resolves, 1);
        // Should be cached after resolution.
        assert!(resolver.cache_mut().get(peer_id, 55).is_some());
    }

    // -- 8. Resolver: unresolvable (unknown node) --------------------------

    #[test]
    fn test_resolver_unresolvable() {
        let cluster = make_cluster(9000);

        let local_addr = addr(9000);
        let local_node = NodeId::new(&local_addr);
        let mut resolver = AddressResolver::new(local_node);

        // Use a NodeId that does not exist in the cluster.
        let unknown_node = NodeId(9999);
        let address = ActorAddress::remote(unknown_node, 55);
        let result = resolver.resolve(&cluster, address);

        assert!(
            matches!(result, ResolveResult::Unresolvable { .. }),
            "expected Unresolvable for unknown node, got {:?}",
            result
        );
        assert_eq!(resolver.stats().failed_resolves, 1);
    }

    // -- 9. Resolver: record_remote_send updates cache -----------------------

    #[test]
    fn test_resolver_record_send() {
        let local_addr = addr(9000);
        let local_node = NodeId::new(&local_addr);
        let mut resolver = AddressResolver::new(local_node);

        assert!(resolver.cache().is_empty());

        resolver.record_remote_send(NodeId(5), 99);

        assert_eq!(resolver.cache().len(), 1);
        let info = resolver.cache_mut().get(NodeId(5), 99).unwrap();
        assert_eq!(info.node_id, NodeId(5));
        assert_eq!(info.actor_id, 99);
        assert_eq!(info.message_count, 1);
    }

    // -- 9b. Cache: remove_node / resolver invalidate_node -------------------

    #[test]
    fn test_remote_cache_remove_node() {
        let mut cache = RemoteActorCache::with_defaults();
        cache.put(NodeId(1), 10);
        cache.put(NodeId(1), 11);
        cache.put(NodeId(2), 20);

        cache.remove_node(NodeId(1));

        assert_eq!(cache.len(), 1);
        assert!(cache.get(NodeId(1), 10).is_none());
        assert!(cache.get(NodeId(1), 11).is_none());
        assert!(cache.get(NodeId(2), 20).is_some());
    }

    #[test]
    fn test_resolver_invalidate_node() {
        let local_addr = addr(9100);
        let local_node = NodeId::new(&local_addr);
        let mut resolver = AddressResolver::new(local_node);
        resolver.record_remote_send(NodeId(5), 99);
        resolver.record_remote_send(NodeId(5), 98);
        resolver.record_remote_send(NodeId(6), 77);
        assert_eq!(resolver.cache().len(), 3);

        resolver.invalidate_node(NodeId(5));

        assert_eq!(resolver.cache().len(), 1);
        assert!(resolver.cache_mut().get(NodeId(5), 99).is_none());
        assert!(resolver.cache_mut().get(NodeId(5), 98).is_none());
        assert!(resolver.cache_mut().get(NodeId(6), 77).is_some());
    }

    // -- 9c. Remote link/monitor registries ----------------------------------

    #[test]
    fn test_remote_link_registry_clear_node_returns_target_pairs() {
        use crate::runtime::supervision::{RemoteLink, RemoteLinkRegistry};
        let mut reg = RemoteLinkRegistry::new();
        let target = RemoteLink {
            node_id: NodeId(1),
            actor_id: 100,
        };
        let watcher_local = RemoteLink {
            node_id: NodeId(0),
            actor_id: 7,
        };
        let watcher_remote = RemoteLink {
            node_id: NodeId(2),
            actor_id: 8,
        };
        reg.register(target, watcher_local);
        reg.register(target, watcher_remote);

        let cleared = reg.clear_node(NodeId(1));
        assert_eq!(cleared.len(), 2);
        // Both watchers must be paired with the target they were watching.
        assert!(cleared.iter().all(|(t, _)| *t == target));
        assert!(cleared.iter().any(|(_, w)| *w == watcher_local));
        assert!(cleared.iter().any(|(_, w)| *w == watcher_remote));
        // The registry is empty for that node now.
        assert!(reg.get_watchers(target).is_none());
    }

    // -- 10. Build packet ----------------------------------------------------

    #[test]
    fn test_build_packet() {
        let local_addr = addr(9000);
        let local_node = NodeId::new(&local_addr);
        let resolver = AddressResolver::new(local_node);

        let trace = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let packet = resolver.build_packet(
            42, // target_actor
            "handle_msg",
            vec![Value::int(7), Value::string(0)],
            100, // sender_actor
            MessagePriority::Normal,
            vec!["hello".to_string()],
            vec![], // object_table
            None,   // content_hash
            Some(trace.to_string()),
        );
        match packet {
            Packet::ActorMessage {
                target_actor,
                behavior_name,
                content_hash,
                payload,
                string_table,
                sender_actor,
                sender_node,
                priority,
                trace_id,
                ..
            } => {
                assert_eq!(target_actor, 42);
                assert_eq!(behavior_name, "handle_msg");
                assert_eq!(content_hash, None);
                assert_eq!(sender_actor, 100);
                assert_eq!(sender_node.0, local_node.0); // Same underlying u64
                assert_eq!(priority, MessagePriority::Normal);
                assert_eq!(payload.len(), 2);
                assert_eq!(string_table, vec!["hello".to_string()]);
                assert_eq!(trace_id.as_deref(), Some(trace));
            }
            other => panic!("expected ActorMessage packet, got {:?}", other),
        }
    }

    // -- 11. Parse packet ----------------------------------------------------

    #[test]
    fn test_parse_packet() {
        let local_addr = addr(9000);
        let local_node = NodeId::new(&local_addr);
        let mut resolver = AddressResolver::new(local_node);

        let packet = Packet::ActorMessage {
            target_actor: 77,
            behavior_name: "inc".to_string(),
            content_hash: None,
            payload: vec![Value::int(123)],
            string_table: vec![],
            object_table: vec![],
            sender_actor: 88,
            sender_node: NodeId(9),
            priority: MessagePriority::System,
            trace_id: None,
        };
        let result = resolver.parse_packet(packet);
        assert!(result.is_some());

        let (target, behavior_name, msg, string_table, _object_table, content_hash) =
            result.unwrap();
        assert_eq!(target, 77);
        assert_eq!(behavior_name, "inc");
        assert_eq!(content_hash, None);
        // behavior_id is resolved at delivery, not parse time.
        assert_eq!(msg.behavior_id, 0);
        assert_eq!(msg.sender, 88);
        assert_eq!(msg.priority, MessagePriority::System);
        assert_eq!(msg.payload.len(), 1);
        assert!(string_table.is_empty());
        // The sender should now be in the cache.
        assert!(resolver.cache_mut().get(NodeId(9), 88).is_some());
    }

    // -- 12. DistributedRuntime trait compiles -------------------------------

    #[cfg(feature = "tcp")]
    #[test]
    fn test_distributed_trait_exists() {
        // This test just verifies that the trait and wrapper type compile.
        // We create the components (but don't start network threads).
        let mut runtime = Runtime::new();
        let cluster = make_cluster(9000);
        let local_node = NodeId::new(&addr(9000));
        let mut resolver = AddressResolver::new(local_node);

        // Create a transport — bind to port 0 to get an ephemeral port.
        let transport = crate::runtime::network::TcpTransport::bind(
            addr(0),
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        );
        if let Ok(mut transport) = transport {
            let mut dist = DistributedRuntimeImpl::new(&mut runtime);

            // Just verify the trait object can be formed.
            let _trait_obj: &dyn DistributedRuntime = &dist;

            // Verify local send works (delegates to runtime.send_message).
            let local_addr = ActorAddress::local(999); // non-existent actor
            DistributedRuntime::send_distributed(
                &mut dist,
                &mut transport,
                &cluster,
                &mut resolver,
                local_addr,
                "test_behavior",
                &[Value::int(1)],
            );

            // Success if we get here without panicking.
            transport.shutdown();
        }
    }

    // -- 13. Cache most_active ordering --------------------------------------

    #[test]
    fn test_cache_most_active() {
        let mut cache = RemoteActorCache::new(10, Duration::from_secs(3600));

        cache.put(NodeId(1), 10);
        cache.put(NodeId(2), 20);
        cache.put(NodeId(3), 30);

        // Access (1,10) most recently, then (3,30).
        let _ = cache.get(NodeId(2), 20);
        let _ = cache.get(NodeId(1), 10);
        let _ = cache.get(NodeId(3), 30);

        let most_active = cache.most_active(2);
        assert_eq!(most_active.len(), 2);
        // Most recent first.
        assert_eq!(most_active[0].actor_id, 30);
        assert_eq!(most_active[1].actor_id, 10);
    }

    // -- 14. Cache remove ----------------------------------------------------

    #[test]
    fn test_cache_remove() {
        let mut cache = RemoteActorCache::new(10, Duration::from_secs(3600));

        cache.put(NodeId(1), 10);
        cache.put(NodeId(2), 20);
        assert_eq!(cache.len(), 2);

        cache.remove(NodeId(1), 10);
        assert_eq!(cache.len(), 1);
        assert!(cache.get(NodeId(1), 10).is_none());
        assert!(cache.get(NodeId(2), 20).is_some());
    }

    // -- 15. ResolveResult equality ------------------------------------------

    #[test]
    fn test_resolve_result_equality() {
        let a = ResolveResult::Local { actor_id: 1 };
        let b = ResolveResult::Local { actor_id: 1 };
        let c = ResolveResult::Local { actor_id: 2 };
        assert_eq!(a, b);
        assert_ne!(a, c);

        let d = ResolveResult::Remote {
            node_id: NodeId(1),
            actor_id: 5,
        };
        let e = ResolveResult::Remote {
            node_id: NodeId(1),
            actor_id: 5,
        };
        assert_eq!(d, e);
    }

    // -- 16. ActorAddress equality and hash ----------------------------------

    #[test]
    fn test_actor_address_eq_and_hash() {
        let a1 = ActorAddress::remote(NodeId(7), 42);
        let a2 = ActorAddress::remote(NodeId(7), 42);
        let a3 = ActorAddress::remote(NodeId(7), 43);

        assert_eq!(a1, a2);
        assert_ne!(a1, a3);

        // Hash consistency: equal values have equal hashes.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        fn hash_of<T: Hash>(t: &T) -> u64 {
            let mut h = DefaultHasher::new();
            t.hash(&mut h);
            h.finish()
        }
        assert_eq!(hash_of(&a1), hash_of(&a2));
    }

    // -- 17. End-to-end: remote send dispatches the named behavior ---------

    #[cfg(feature = "tcp")]
    #[test]
    fn test_remote_send_dispatches_named_behavior() {
        use std::time::{Duration, Instant};

        // Node B runs the target actor with two behaviors. "inc" is
        // registered second, so its behavior id is 1 — the old stub's
        // placeholder id 0 would dispatch "dec" instead, so this test
        // fails without name-based dispatch.
        let mut runtime_b = Runtime::new();
        let actor_b =
            runtime_b.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
        {
            let actor = runtime_b.actors.get_mut(&actor_b).unwrap();
            actor.register_behavior("dec", |actor, _args| {
                if let Some(n) = actor.get_state_field("count").and_then(|v| v.as_int()) {
                    actor.set_state_field("count", Value::int(n - 1));
                }
            });
            actor.register_behavior("inc", |actor, args| {
                let n = actor
                    .get_state_field("count")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                let by = args.get(0).and_then(|v| v.as_int()).unwrap_or(1);
                actor.set_state_field("count", Value::int(n + by));
            });
        }

        let mut transport_a = crate::runtime::network::TcpTransport::bind(
            addr(0),
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();
        let mut transport_b = crate::runtime::network::TcpTransport::bind(
            addr(0),
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();
        let node_b = transport_b.node_id();
        let addr_b = transport_b.listen_addr();

        // Node A's cluster knows B as a healthy peer.
        let mut cluster_a = ClusterState::new(transport_a.node_id(), transport_a.listen_addr());
        cluster_a.handle_heartbeat(node_b, addr_b);
        let mut resolver_a = AddressResolver::new(transport_a.node_id());
        let mut runtime_a = Runtime::new();

        // Node B's delivery side.
        let mut cluster_b = ClusterState::new(node_b, addr_b);
        let mut resolver_b = AddressResolver::new(node_b);

        let target = ActorAddress::remote(node_b, actor_b);
        let deliver = |runtime_b: &mut Runtime,
                       transport_b: &mut dyn NetworkTransport,
                       cluster_b: &mut ClusterState,
                       resolver_b: &mut AddressResolver| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                process_network_packets(runtime_b, transport_b, cluster_b, resolver_b);
                let pending = runtime_b
                    .actors
                    .get(&actor_b)
                    .map(|a| a.mailbox.len())
                    .unwrap_or(0);
                if pending > 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };

        // Named behavior: must dispatch "inc" (id 1), not "dec" (id 0).
        send_distributed(
            &mut runtime_a,
            &mut transport_a,
            &cluster_a,
            &mut resolver_a,
            target,
            "inc",
            &[Value::int(5)],
        );
        deliver(
            &mut runtime_b,
            &mut transport_b,
            &mut cluster_b,
            &mut resolver_b,
        );
        runtime_b.step_actor(actor_b);
        let count = runtime_b
            .actors
            .get(&actor_b)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int())
            .unwrap();
        assert_eq!(
            count, 5,
            "remote send must dispatch the named behavior \"inc\""
        );

        // Unknown behavior name: falls back to behavior 0, mirroring
        // `Runtime::send_message`'s `unwrap_or(0)` for local sends.
        send_distributed(
            &mut runtime_a,
            &mut transport_a,
            &cluster_a,
            &mut resolver_a,
            target,
            "no_such_behavior",
            &[],
        );
        deliver(
            &mut runtime_b,
            &mut transport_b,
            &mut cluster_b,
            &mut resolver_b,
        );
        runtime_b.step_actor(actor_b);
        let count = runtime_b
            .actors
            .get(&actor_b)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int())
            .unwrap();
        assert_eq!(
            count, 4,
            "unknown behavior name must fall back to behavior 0 (\"dec\")"
        );

        transport_a.shutdown();
        transport_b.shutdown();
    }

    // -- 18. Cross-node string payloads --------------------------------------

    #[test]
    fn test_resolve_wire_strings() {
        use crate::bytecode::{CodeModule, Constant};

        let mut rt = Runtime::new();
        let mut module = CodeModule::new("sender");
        module.add_constant(Constant::String("first".to_string())); // id 0
        module.add_constant(Constant::String("hello".to_string())); // id 1
        rt.vm = Some(crate::vm::VM::new());
        rt.vm.as_mut().unwrap().load_module(module);

        let sender = rt.spawn_actor(Box::new(|| vec![]));
        {
            let actor = rt.actors.get_mut(&sender).unwrap();
            actor.bytecode_module_idx = Some(0);
        }
        rt.current_actor = Some(sender);

        // Strings become per-packet table ids; repeated content shares one
        // entry; scalars pass through unchanged.
        let (payload, table) = resolve_wire_strings(
            &rt,
            &[
                Value::string(1),
                Value::int(5),
                Value::string(1),
                Value::string(0),
            ],
        )
        .expect("strings resolvable in the sender module");
        assert_eq!(
            payload,
            vec![
                Value::string(0),
                Value::int(5),
                Value::string(0),
                Value::string(1),
            ]
        );
        assert_eq!(table, vec!["hello".to_string(), "first".to_string()]);

        // No sender module context → unresolvable → None (drop, don't
        // corrupt).
        rt.current_actor = None;
        assert!(resolve_wire_strings(&rt, &[Value::string(0)]).is_none());

        // A string id outside the sender pool → None.
        rt.current_actor = Some(sender);
        assert!(resolve_wire_strings(&rt, &[Value::string(99)]).is_none());

        // Payloads without strings pass through with an empty table.
        let (payload, table) =
            resolve_wire_strings(&rt, &[Value::int(1), Value::bool(true)]).unwrap();
        assert_eq!(payload, vec![Value::int(1), Value::bool(true)]);
        assert!(table.is_empty());
    }

    #[test]
    fn test_intern_wire_strings() {
        use crate::bytecode::{CodeModule, Constant};

        let mut rt = Runtime::new();
        // The receiver's pool id 0 deliberately holds a DIFFERENT string
        // than the content being interned: resolving a raw sender id
        // against this pool would corrupt the payload.
        let mut module = CodeModule::new("receiver");
        module.add_constant(Constant::String("DECOY-RECEIVER-LOCAL".to_string()));

        let actor_id = rt.spawn_actor(Box::new(|| vec![]));
        {
            let actor = rt.actors.get_mut(&actor_id).unwrap();
            actor.bytecode_module = Some(module);
        }

        // First intern: lazy-loads the actor's module and appends the
        // content after the decoy.
        let table = vec!["hello-cross-node".to_string()];
        let mut payload = vec![Value::string(0), Value::int(9)];
        assert!(intern_wire_strings(&mut rt, actor_id, &mut payload, &table));
        assert_eq!(payload[0], Value::string(1));
        assert_eq!(payload[1], Value::int(9), "scalars must pass through");
        let module_idx = rt
            .actors
            .get(&actor_id)
            .unwrap()
            .bytecode_module_idx
            .unwrap();
        let vm = rt.vm.as_ref().unwrap();
        assert_eq!(
            vm.constant_string(module_idx, payload[0].as_string_id().unwrap()),
            Some("hello-cross-node".to_string())
        );
        let pool_len = vm.modules[module_idx].constants.len();

        // Repeated content dedups against the existing pool entry — the
        // pool must not grow per message.
        let mut payload2 = vec![Value::string(0)];
        assert!(intern_wire_strings(
            &mut rt,
            actor_id,
            &mut payload2,
            &table
        ));
        assert_eq!(payload2[0], Value::string(1));
        assert_eq!(
            rt.vm.as_ref().unwrap().modules[module_idx].constants.len(),
            pool_len
        );

        // Out-of-bounds table id → false (the caller drops the message).
        let mut bad = vec![Value::string(5)];
        assert!(!intern_wire_strings(&mut rt, actor_id, &mut bad, &table));

        // An actor with no module pool at all → false.
        let bare = rt.spawn_actor(Box::new(|| vec![]));
        let mut payload3 = vec![Value::string(0)];
        assert!(!intern_wire_strings(&mut rt, bare, &mut payload3, &table));
    }

    #[cfg(feature = "tcp")]
    /// End-to-end regression: a string payload sent from node A must arrive
    /// on node B with its CONTENT intact — interned into the receiving
    /// actor's module pool — even though the receiver's pool holds a
    /// different string at the sender's pool id. Before cross-node string
    /// interning the send-time guard dropped such packets outright (and
    /// before that, raw ids silently resolved to the wrong text).
    #[test]
    fn test_remote_string_payload_delivered_by_content() {
        use crate::bytecode::{CodeModule, Constant};
        use std::time::{Duration, Instant};

        // --- Node A (sender): pool id 0 = "hello-cross-node". ---
        let mut runtime_a = Runtime::new();
        let mut module_a = CodeModule::new("sender");
        module_a.add_constant(Constant::String("hello-cross-node".to_string()));
        runtime_a.vm = Some(crate::vm::VM::new());
        runtime_a.vm.as_mut().unwrap().load_module(module_a);
        let actor_a = runtime_a.spawn_actor(Box::new(|| vec![]));
        {
            let actor = runtime_a.actors.get_mut(&actor_a).unwrap();
            actor.bytecode_module_idx = Some(0);
        }
        // The send path resolves string content against the CURRENT
        // actor's module pool.
        runtime_a.current_actor = Some(actor_a);

        // --- Node B (receiver): pool id 0 deliberately holds a DIFFERENT
        // string, so delivering the sender's raw id 0 would corrupt the
        // payload into "DECOY-RECEIVER-LOCAL". ---
        let mut runtime_b = Runtime::new();
        let mut module_b = CodeModule::new("receiver");
        module_b.add_constant(Constant::String("DECOY-RECEIVER-LOCAL".to_string()));
        let actor_b =
            runtime_b.spawn_actor(Box::new(|| vec![("received".to_string(), Value::nil())]));
        {
            let actor = runtime_b.actors.get_mut(&actor_b).unwrap();
            actor.bytecode_module = Some(module_b);
            actor.register_behavior("store", |actor, args| {
                let v = args.get(0).copied().unwrap_or(Value::nil());
                actor.set_state_field("received", v);
            });
        }

        let mut transport_a = crate::runtime::network::TcpTransport::bind(
            addr(0),
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();
        let mut transport_b = crate::runtime::network::TcpTransport::bind(
            addr(0),
            crate::runtime::network::TlsConfig::PlaintextInsecure,
        )
        .unwrap();
        let node_b = transport_b.node_id();
        let addr_b = transport_b.listen_addr();

        // Node A's cluster knows B as a healthy peer.
        let mut cluster_a = ClusterState::new(transport_a.node_id(), transport_a.listen_addr());
        cluster_a.handle_heartbeat(node_b, addr_b);
        let mut resolver_a = AddressResolver::new(transport_a.node_id());

        // Node B's delivery side.
        let mut cluster_b = ClusterState::new(node_b, addr_b);
        let mut resolver_b = AddressResolver::new(node_b);

        let target = ActorAddress::remote(node_b, actor_b);
        send_distributed(
            &mut runtime_a,
            &mut transport_a,
            &cluster_a,
            &mut resolver_a,
            target,
            "store",
            &[Value::string(0)],
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            process_network_packets(
                &mut runtime_b,
                &mut transport_b,
                &mut cluster_b,
                &mut resolver_b,
            );
            let pending = runtime_b
                .actors
                .get(&actor_b)
                .map(|a| a.mailbox.len())
                .unwrap_or(0);
            if pending > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            runtime_b
                .actors
                .get(&actor_b)
                .map(|a| a.mailbox.len())
                .unwrap_or(0)
                > 0,
            "string message was not delivered to the remote actor's mailbox"
        );

        runtime_b.step_actor(actor_b);

        let stored = runtime_b
            .actors
            .get(&actor_b)
            .unwrap()
            .get_state_field("received")
            .unwrap();
        let stored_id = stored
            .as_string_id()
            .expect("payload must arrive as a string value");
        let module_idx = runtime_b
            .actors
            .get(&actor_b)
            .unwrap()
            .bytecode_module_idx
            .unwrap();
        let content = runtime_b
            .vm
            .as_ref()
            .unwrap()
            .constant_string(module_idx, stored_id);
        assert_eq!(
            content,
            Some("hello-cross-node".to_string()),
            "string payload must arrive by CONTENT, not by the sender's pool id"
        );

        transport_a.shutdown();
        transport_b.shutdown();
    }

    #[test]
    fn test_ack_stored_on_receive() {
        let mut rt = Runtime::new();
        assert!(!rt.is_acked(42));
        rt.acked_packets.insert(42);
        assert!(rt.is_acked(42));
        let drained = rt.drain_acked();
        assert!(drained.contains(&42));
        assert!(!rt.is_acked(42));
    }

    #[test]
    fn test_delivery_failure_notification_includes_code() {
        // Verify that delivery_failure_code maps known reason strings to
        // the documented integer codes and falls back to 5 for unknowns.
        assert_eq!(delivery_failure_code("unresolvable"), 0);
        assert_eq!(delivery_failure_code("target node left cluster"), 1);
        assert_eq!(delivery_failure_code("string payload unresolvable"), 2);
        assert_eq!(delivery_failure_code("string intern failed on receiver"), 3);
        assert_eq!(delivery_failure_code("target actor not found"), 4);
        assert_eq!(delivery_failure_code("some future reason"), 5);
    }
}
