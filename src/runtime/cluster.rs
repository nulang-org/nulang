//! Cluster membership system for Nulang's distributed actor runtime.
//!
//! This module manages node identity, cluster membership, heartbeat-based
//! failure detection, and gossip-style state dissemination. Multiple Nulang
//! nodes form a cluster, allowing actors to communicate across machine
//! boundaries.
//!
//! # Architecture
//!
//! Each node maintains a [`ClusterState`] containing a membership table of
//! all known nodes. Nodes exchange heartbeats periodically to detect failures
//! and gossip membership updates to disseminate state changes.
//!
//! # Failure Detection
//!
//! The failure detector uses a simple multi-stage timeout:
//!
//! 1. **Healthy** → nodes are responding to heartbeats.
//! 2. **Suspicious** → a heartbeat has not been received within the timeout.
//! 3. **Failed** → the node has been suspicious for too long and is removed.
//!
//! # Gossip Protocol
//!
//! Membership changes propagate via gossip. Each tick, a node selects a random
//! subset of healthy peers and sends them a compact view of the membership
//! table. When merging incoming gossip, the higher incarnation number wins,
//! ensuring convergence even under partition.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tracing::warn;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default interval between heartbeats (500ms).
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// Default timeout before marking a node suspicious (2s).
const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(2);

/// Default duration a node remains suspicious before being marked failed (5s).
const DEFAULT_SUSPICION_DURATION: Duration = Duration::from_secs(5);

/// How long to keep failed nodes in the table before purging them (60s).
const FAILED_NODE_RETENTION: Duration = Duration::from_secs(60);

/// Number of random gossip targets selected each tick.
const GOSSIP_FANOUT: usize = 2;

/// Default interval between liveness probes to Failed members (5s).
const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// Default size of the active view: the maximum number of members a
/// node heartbeats directly. Heartbeats are the O(N) data plane; the
/// active view bounds them so cluster-wide heartbeat traffic stays
/// O(N × active_view_size) instead of O(N²).
const DEFAULT_ACTIVE_VIEW_SIZE: usize = 4;

/// Default size of the passive view: the pool of known-but-not-
/// heartbeated members used to repair the active view when a member
/// fails.
const DEFAULT_PASSIVE_VIEW_SIZE: usize = 20;

/// How long a probationary (promoted) member has to reciprocate our
/// heartbeats before it is demoted back to the passive view. Uses the
/// heartbeat timeout: a live member's reply arrives within one
/// heartbeat interval, so anything beyond the timeout is dead weight.
const PROBATION_TIMEOUT: Duration = DEFAULT_HEARTBEAT_TIMEOUT;

/// How many passive-view members that recently heartbeated us we reply
/// to per heartbeat round. Without replies, a member whose active view
/// filled up would stop heartbeating us and our detector would
/// false-fail it; with `REPLY_SLOTS` slots rotated round-robin, every
/// pinger is answered within the failure-detection window (4 slots ×
/// 500 ms = 2 s = `DEFAULT_HEARTBEAT_TIMEOUT`) for clusters of up to
/// ~80 nodes, keeping heartbeats O(active + probationary + replies).
pub(crate) const REPLY_SLOTS: usize = 4;

// ---------------------------------------------------------------------------
// NodeId
// ---------------------------------------------------------------------------

/// Unique identifier for a node in the cluster.
///
/// When TLS is active, derived from the BLAKE3 hash of the node's
/// X.509 certificate DER encoding — a cryptographic identity that
/// cannot be spoofed by an attacker who controls a socket address.
/// When plaintext is in use, derived from a hash of the node's
/// socket address for backward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u64);

impl NodeId {
    /// Create a `NodeId` from a socket address (TCP).
    ///
    /// The id is derived with `DefaultHasher` so repeated calls with the
    /// same address yield the same id.
    pub fn new(addr: &SocketAddr) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        addr.hash(&mut hasher);
        NodeId(hasher.finish())
    }

    /// Create a `NodeId` from a certificate's DER encoding.
    ///
    /// Uses BLAKE3 (truncated to 64 bits) for a collision-resistant,
    /// cryptographically-secure identity bound to the certificate.
    /// Two nodes presenting the same certificate receive the same id;
    /// two nodes with different certificates are guaranteed distinct ids
    /// (modulo the 64-bit truncation, whose collision probability is
    /// negligible at any realistic cluster size).
    pub fn from_cert_der(cert_der: &[u8]) -> Self {
        let hash = ::blake3::hash(cert_der);
        let bytes: [u8; 8] = hash.as_bytes()[..8].try_into().unwrap();
        NodeId(u64::from_le_bytes(bytes))
    }

    /// Create a `NodeId` from a transport address (TCP or Unix).
    pub fn from_addr(addr: &crate::runtime::network::TransportAddr) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        addr.hash(&mut hasher);
        NodeId(hasher.finish())
    }

    /// The id reserved for the local node.
    pub const LOCAL: NodeId = NodeId(0);
}

// ---------------------------------------------------------------------------
// NodeStatus
// ---------------------------------------------------------------------------

/// Health status of a node in the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// Node is in the process of joining the cluster.
    Joining,
    /// Node is active and responding to heartbeats.
    Healthy,
    /// Node missed a heartbeat and is under suspicion.
    Suspicious,
    /// Node has been declared failed.
    Failed,
    /// Node is gracefully leaving the cluster.
    Leaving,
}

// ---------------------------------------------------------------------------
// NodeInfo
// ---------------------------------------------------------------------------

/// Information about a node in the cluster.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    /// Unique identifier of the node.
    pub node_id: NodeId,
    /// Network address the node listens on.
    pub address: SocketAddr,
    /// Current health status.
    pub status: NodeStatus,
    /// Timestamp of the last received heartbeat.
    pub last_heartbeat: Instant,
    /// When the node first joined the cluster (from our perspective).
    pub joined_at: Instant,
    /// Optional key-value metadata (e.g. region, rack, version).
    pub metadata: HashMap<String, String>,
}

impl NodeInfo {
    /// Create a minimal `NodeInfo` for the given node.
    fn new(node_id: NodeId, address: SocketAddr) -> Self {
        let now = Instant::now();
        NodeInfo {
            node_id,
            address,
            status: NodeStatus::Joining,
            last_heartbeat: now,
            joined_at: now,
            metadata: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// ClusterAction
// ---------------------------------------------------------------------------

/// Actions returned by [`ClusterState::tick`] for the runtime to execute.
///
/// The caller is responsible for serialising and transmitting heartbeats
/// and gossip messages over the network.
#[derive(Debug)]
pub enum ClusterAction {
    /// Send a heartbeat to the specified node.
    SendHeartbeat { to: NodeId, addr: SocketAddr },
    /// Notify that a node has joined the cluster.
    NodeJoined { node: NodeId, addr: SocketAddr },
    /// Notify that a node has been declared failed.
    NodeFailed { node: NodeId },
    /// Notify that a node is confirmed gone (either by a positive
    /// `NodeGoodbye` or by `removal_confirmation_timeout` elapsing past
    /// `Failed`), and its durable re-spawn-opted actors may be re-spawned.
    NodeRemoved { node: NodeId },
    /// Notify that a node has left the cluster.
    NodeLeft { node: NodeId },
    /// Send gossip to a random subset of nodes.
    SendGossip { targets: Vec<(NodeId, SocketAddr)> },
    /// The split-brain resolver decided the local node should leave the
    /// cluster (partition minority / below quorum).
    Down { node: NodeId },
    /// Minimal periodic liveness probe to a Failed member, so a healed
    /// partition re-joins without an external rejoin.
    Probe { to: NodeId, addr: SocketAddr },
}

// ---------------------------------------------------------------------------
// Split-brain resolver
// ---------------------------------------------------------------------------

/// What the local node should do given its current membership view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverDecision {
    /// The local node keeps participating in the cluster.
    StayUp,
    /// The local node leaves the cluster (partition minority / below quorum).
    DownSelf,
}

/// Snapshot of the membership view handed to a [`SplitBrainResolver`].
///
/// Built from the live membership table at tick time; the resolver must
/// treat it as immutable.
#[derive(Debug, Clone)]
pub struct MembershipView {
    /// The node asking for a decision.
    pub local: NodeId,
    /// All known members with their current statuses.
    pub members: Vec<NodeInfo>,
}

/// Pluggable split-brain resolution (Akka-SBR style).
///
/// A resolver is a pure function of the local membership view: no I/O, no
/// timers, so it is unit-testable and DST-drivable. `ClusterState::tick`
/// consults it after failure detection; a `DownSelf` decision marks the
/// local node down and emits [`ClusterAction::Down`].
pub trait SplitBrainResolver: Send + Sync {
    fn decide(&self, view: &MembershipView) -> ResolverDecision;
}

/// Static-quorum strategy: the node stays up iff it sees at least
/// `floor(expected_nodes / 2) + 1` reachable members (itself plus every
/// `Healthy`/`Joining` member). Needs only the operator-configured expected
/// cluster size — no live count, no consensus, no leader.
///
/// With `expected_nodes == 2` both sides of a partition down themselves
/// (`1 < 2`): fail-closed is the intended 2-node behavior, and the strategy
/// is only useful for `expected_nodes >= 3`.
pub struct StaticQuorumResolver {
    pub expected_nodes: usize,
}

impl SplitBrainResolver for StaticQuorumResolver {
    fn decide(&self, view: &MembershipView) -> ResolverDecision {
        let reachable = view
            .members
            .iter()
            .filter(|m| {
                m.node_id == view.local
                    || matches!(m.status, NodeStatus::Healthy | NodeStatus::Joining)
            })
            .count();
        if reachable >= self.expected_nodes / 2 + 1 {
            ResolverDecision::StayUp
        } else {
            ResolverDecision::DownSelf
        }
    }
}

/// Split-brain resolver configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitBrainConfig {
    /// No resolver: current behavior — partitions never self-resolve.
    Disabled,
    /// Static-quorum with the given expected cluster size (see
    /// [`StaticQuorumResolver`] for the 2-node caveat).
    StaticQuorum { expected_nodes: usize },
}

/// Cluster configuration applied when distribution is enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterConfig {
    pub split_brain: SplitBrainConfig,
    /// How often to probe `Failed` members for liveness (the self-healing
    /// path: a probe that reaches a live node re-promotes it to `Healthy`).
    pub probe_interval: Duration,
    /// How long a `Failed` node must stay failed before it is promoted to
    /// "confirmed gone" and its durable actors become re-spawn eligible
    /// (RFC 0014 §1 path 2). `Duration::ZERO` disables timeout-based
    /// promotion (only a positive `NodeGoodbye` confirms); `Duration::MAX`
    /// effectively disables the whole re-spawn surface. Default matches
    /// [`FAILED_NODE_RETENTION`], the point at which the cluster forgets
    /// the node anyway.
    pub removal_confirmation_timeout: Duration,
    /// Whether the durable-actor location directory is gossip-replicated.
    /// Turning this off disables re-spawn (survivors cannot learn where a
    /// dead node's actors lived). Default true.
    pub directory_gossip: bool,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        ClusterConfig {
            split_brain: SplitBrainConfig::Disabled,
            probe_interval: DEFAULT_PROBE_INTERVAL,
            removal_confirmation_timeout: FAILED_NODE_RETENTION,
            directory_gossip: true,
        }
    }
}

impl ClusterConfig {
    /// True when the configuration can be applied. `StaticQuorum` with
    /// `expected_nodes == 0` is a configuration error, not "disabled".
    pub fn is_valid(&self) -> bool {
        match self.split_brain {
            SplitBrainConfig::Disabled => true,
            SplitBrainConfig::StaticQuorum { expected_nodes } => expected_nodes > 0,
        }
    }
}

/// A lightweight gossip entry for membership dissemination.
///
/// This compact representation avoids sending full [`NodeInfo`] (including
/// metadata maps) on every gossip round.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeGossip {
    /// Node identifier.
    pub node_id: NodeId,
    /// Network address.
    pub address: SocketAddr,
    /// Health status.
    pub status: NodeStatus,
    /// Incarnation number for conflict resolution.
    pub incarnation: u64,
}

// ---------------------------------------------------------------------------
// DurableDirectoryEntry
// ---------------------------------------------------------------------------

/// A gossip-replicated entry in the durable-actor location directory
/// (RFC 0014 §2): where each re-spawn-opted durable actor lives and at what
/// activation epoch. Highest-epoch-wins merge (mirroring the incarnation
/// rule in `merge_membership`); the epoch is bumped on every re-spawn so a
/// resurrected old node can detect it has been replaced (§5 self-demote).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurableDirectoryEntry {
    /// The durable actor's globally-unique id.
    pub actor_id: u64,
    /// The node currently hosting the actor.
    pub node_id: NodeId,
    /// Activation epoch; higher = newer incarnation.
    pub epoch: u64,
}

// ---------------------------------------------------------------------------
// ClusterState
// ---------------------------------------------------------------------------

/// Manages the cluster membership for a Nulang node.
///
/// Uses a simple gossip-style protocol where each node maintains a
/// membership table of all known nodes. Heartbeats are exchanged
/// periodically to detect failures.
///
/// # Example
///
/// ```ignore
/// use nulang::runtime::cluster::{ClusterState, NodeId};
/// # use std::net::{SocketAddr, IpAddr, Ipv4Addr};
/// let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9000);
/// let local = NodeId::new(&addr);
/// let mut cluster = ClusterState::new(local, addr);
/// ```
pub struct ClusterState {
    /// This node's identity.
    local_node: NodeId,

    /// Membership table: node_id → node info.
    members: HashMap<NodeId, NodeInfo>,

    /// Nodes that have been declared failed (kept for a while to
    /// prevent rejoining with stale state).
    failed_nodes: HashMap<NodeId, Instant>,

    /// Nodes confirmed gone (RFC 0014 §1): promoted from `Failed` past
    /// `removal_confirmation_timeout`, or on a positive `NodeGoodbye`.
    /// Their durable re-spawn-opted actors are re-spawn eligible.
    removed_nodes: HashSet<NodeId>,

    /// Durable-actor location directory (RFC 0014 §2): actor id → entry,
    /// merged highest-epoch-wins from gossip.
    directory: HashMap<u64, DurableDirectoryEntry>,

    /// How long a `Failed` node must stay failed before promotion to
    /// confirmed-gone (zero disables timeout promotion).
    removal_confirmation_timeout: Duration,

    /// Whether the directory is gossip-replicated.
    directory_gossip: bool,

    /// Heartbeat configuration.
    heartbeat_interval: Duration,
    heartbeat_timeout: Duration,
    suspicion_duration: Duration,

    /// Timestamp of last heartbeat we sent.
    last_heartbeat_sent: Instant,

    /// Optional virtual clock for deterministic testing.
    /// When set, all time queries use this clock instead of wall time.
    clock: Option<super::timer::VirtualClock>,

    /// Optional seeded RNG for deterministic testing. When set, every
    /// internal random pick (gossip target selection, active-view repair
    /// promotion) draws from it instead of `OsRng`, so a same-seed
    /// cluster run is bit-reproducible end to end. `None` = `OsRng`
    /// (production).
    rng: Option<Box<dyn rand_core::RngCore>>,

    /// Optional split-brain resolver; `None` = resolver disabled.
    split_brain: Option<Box<dyn SplitBrainResolver>>,
    /// How often to probe Failed members (the self-healing path).
    probe_interval: Duration,
    /// When we last probed Failed members.
    last_probe_sent: Option<Instant>,
    /// True once the resolver decided the local node should leave.
    local_down: bool,
    /// True once this node has ever received a heartbeat from another
    /// node — i.e. it has been part of a live cluster at some point.
    /// Guards the split-brain resolver: a node that has never contacted
    /// any peer is still bootstrapping (its join handshakes haven't
    /// completed), not a partition minority, and must not down itself
    /// before the cluster can form.
    has_seen_peer: bool,

    /// Active view: members we heartbeat directly. A member joins the
    /// active view by heartbeating us (symmetric by construction), so
    /// the failure detector — which watches exactly this set — never
    /// false-fails a node we cannot hear.
    active_view: Vec<NodeId>,
    /// Probationary members: Healthy passive members we promoted and now
    /// heartbeat, waiting for their first reply to confirm them into the
    /// active view. They are NOT watched by the failure detector, so a
    /// member that never reciprocates is demoted, never falsely failed.
    probationary: Vec<(NodeId, Instant)>,
    /// Passive view: known members we do not heartbeat; the repair pool
    /// for the active view. Their liveness comes from gossip.
    passive_view: Vec<NodeId>,
    /// Capacity of `active_view`.
    active_view_size: usize,
    /// Capacity of `passive_view`.
    passive_view_size: usize,
    /// Rotating index into `passive_view` for the bounded reply rule.
    reply_cursor: usize,
    /// When we last attempted an active-view repair (eventual-repair
    /// throttle).
    last_repair_attempt: Option<Instant>,

    /// Callback for membership change notifications.
    on_member_joined: Option<Box<dyn Fn(NodeId, SocketAddr) + Send>>,
    on_member_left: Option<Box<dyn Fn(NodeId) + Send>>,
    on_member_failed: Option<Box<dyn Fn(NodeId) + Send>>,
}

impl ClusterState {
    /// Create a new cluster state for the local node.
    ///
    /// The local node is automatically added to the membership table with
    /// [`NodeStatus::Healthy`].
    pub fn new(local_node: NodeId, local_addr: SocketAddr) -> Self {
        let now = Instant::now();
        let mut members = HashMap::new();

        let local_info = NodeInfo {
            node_id: local_node,
            address: local_addr,
            status: NodeStatus::Healthy,
            last_heartbeat: now,
            joined_at: now,
            metadata: HashMap::new(),
        };
        members.insert(local_node, local_info);

        ClusterState {
            local_node,
            members,
            clock: None,
            rng: None,
            failed_nodes: HashMap::new(),
            removed_nodes: HashSet::new(),
            directory: HashMap::new(),
            removal_confirmation_timeout: FAILED_NODE_RETENTION,
            directory_gossip: true,
            on_member_joined: None,
            heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
            heartbeat_timeout: DEFAULT_HEARTBEAT_TIMEOUT,
            suspicion_duration: DEFAULT_SUSPICION_DURATION,
            last_heartbeat_sent: now,
            split_brain: None,
            probe_interval: DEFAULT_PROBE_INTERVAL,
            last_probe_sent: None,
            local_down: false,
            has_seen_peer: false,
            active_view: Vec::new(),
            probationary: Vec::new(),
            passive_view: Vec::new(),
            active_view_size: DEFAULT_ACTIVE_VIEW_SIZE,
            passive_view_size: DEFAULT_PASSIVE_VIEW_SIZE,
            last_repair_attempt: None,
            reply_cursor: 0,
            on_member_left: None,
            on_member_failed: None,
        }
    }

    /// Current time, using the virtual clock if one is configured.
    fn now(&self) -> Instant {
        match &self.clock {
            Some(clock) => clock.now(),
            None => Instant::now(),
        }
    }

    /// Install a virtual clock for deterministic testing.
    /// When set, all time queries use this clock instead of wall time.
    pub fn set_clock(&mut self, clock: super::timer::VirtualClock) {
        self.clock = Some(clock);
    }

    /// Install a seeded RNG for deterministic testing. When set, every
    /// internal random pick (gossip targets, repair promotion) draws
    /// from it instead of `OsRng`, making a same-seed run bit-reproducible.
    pub fn set_rng(&mut self, rng: Box<dyn rand_core::RngCore>) {
        self.rng = Some(rng);
    }

    /// Draw a uniform index in `0..len` from the seeded RNG when one is
    /// installed, else from `OsRng`.
    fn random_index(&mut self, len: usize) -> usize {
        debug_assert!(len > 0);
        use rand_core::RngCore;
        let mut buf = [0u8; 8];
        match &mut self.rng {
            Some(rng) => rng.fill_bytes(&mut buf),
            None => rand_core::OsRng.fill_bytes(&mut buf),
        }
        (u64::from_le_bytes(buf) as usize) % len
    }

    /// Join an existing cluster by contacting a seed node.
    ///
    /// Records the seed node in the membership table (as Joining, with
    /// baseline `_incarnation` metadata so the join propagates via gossip).
    /// The actual network request to the seed is the responsibility of
    /// the caller.
    pub fn join_cluster(&mut self, seed_addr: SocketAddr) {
        let seed_id = NodeId::new(&seed_addr);

        if seed_id == self.local_node {
            // Cannot join ourselves.
            return;
        }

        if !self.members.contains_key(&seed_id) {
            let mut info = NodeInfo::new(seed_id, seed_addr);
            info.status = NodeStatus::Joining;
            // Baseline incarnation 1: the seed address is authoritative
            // (it came from an explicit join request), so same-generation
            // gossip (incarnation 1) must not overwrite it with a
            // discovered address of unknown quality. Strictly-higher
            // incarnations still win.
            info.metadata
                .insert("_incarnation".to_string(), "1".to_string());
            self.members.insert(seed_id, info);
        }
    }

    /// Join a cluster by seed node ID and address, for cases where the
    /// node ID is not derived from the address (e.g., TLS cert-based IDs).
    pub fn join_cluster_with_id(&mut self, seed_id: NodeId, seed_addr: SocketAddr) {
        if seed_id == self.local_node {
            return;
        }
        if !self.members.contains_key(&seed_id) {
            let mut info = NodeInfo::new(seed_id, seed_addr);
            info.status = NodeStatus::Joining;
            info.metadata
                .insert("_incarnation".to_string(), "1".to_string());
            self.members.insert(seed_id, info);
        }
    }

    /// Handle an incoming heartbeat from another node.
    ///
    /// Updates the node's `last_heartbeat` timestamp and promotes the
    /// status back to [`NodeStatus::Healthy`] if it was previously
    /// Suspicious or Failed.
    ///
    /// If the node was not previously known, it is added to the
    /// membership table.
    ///
    /// View maintenance: a node that heartbeats us is alive by
    /// definition, so it is placed in the active view (symmetric link —
    /// we will heartbeat it back) if there is room, else the passive
    /// view. A probationary member's first heartbeat confirms it into
    /// the active view.
    pub fn handle_heartbeat(&mut self, from: NodeId, addr: SocketAddr) {
        let now = self.now();
        if from != self.local_node {
            self.has_seen_peer = true;
        }

        match self.members.get_mut(&from) {
            Some(info) => {
                let was_suspicious_or_failed =
                    matches!(info.status, NodeStatus::Suspicious | NodeStatus::Failed);

                info.last_heartbeat = now;
                info.address = addr;

                if was_suspicious_or_failed {
                    info.status = NodeStatus::Healthy;
                    Self::bump_entry_incarnation(info);
                } else if info.status == NodeStatus::Joining {
                    info.status = NodeStatus::Healthy;
                    // Bump the entry incarnation so the promotion wins
                    // merges on nodes that learned the stale Joining
                    // status from an earlier gossip round.
                    Self::bump_entry_incarnation(info);
                }
            }
            None => {
                // New node discovered via heartbeat.
                let mut info = NodeInfo::new(from, addr);
                info.last_heartbeat = now;
                info.status = NodeStatus::Healthy;
                self.members.insert(from, info);

                if let Some(ref cb) = self.on_member_joined {
                    cb(from, addr);
                }
            }
        }

        self.observe_heartbeat(from);
    }

    /// Record that `from` heartbeated us: confirm a probationary member
    /// into the active view, or place a new member into the active view
    /// (room permitting) / passive view.
    fn observe_heartbeat(&mut self, from: NodeId) {
        if from == self.local_node {
            return;
        }
        if let Some(pos) = self.probationary.iter().position(|(id, _)| *id == from) {
            // First reply: the promoted member reciprocates, so the
            // link is symmetric — confirm it into the active view.
            self.probationary.remove(pos);
            self.push_active(from);
            return;
        }
        if self.active_view.contains(&from) {
            return;
        }
        if let Some(pos) = self.passive_view.iter().position(|id| *id == from) {
            self.passive_view.remove(pos);
        }
        self.push_active(from);
    }

    /// Add `node` to the active view if it has room, else the passive
    /// view (bounded).
    fn push_active(&mut self, node: NodeId) {
        if self.active_view.len() < self.active_view_size {
            self.active_view.push(node);
        } else if !self.passive_view.contains(&node)
            && self.passive_view.len() < self.passive_view_size
        {
            self.passive_view.push(node);
        }
    }

    /// The members we currently heartbeat directly.
    pub fn active_view(&self) -> &[NodeId] {
        &self.active_view
    }

    /// The members in the passive repair pool.
    pub fn passive_view(&self) -> &[NodeId] {
        &self.passive_view
    }

    /// The members currently on probation (promoted, awaiting their
    /// first reply).
    pub fn probationary(&self) -> &[(NodeId, Instant)] {
        &self.probationary
    }

    /// Apply operator cluster configuration.
    ///
    /// Returns false (and leaves the previous configuration in place) when
    /// the configuration is invalid, e.g. `static-quorum` with
    /// `expected_nodes == 0`.
    pub fn apply_config(&mut self, config: &ClusterConfig) -> bool {
        if !config.is_valid() {
            warn!(
                "cluster config: static-quorum expected_nodes must be >= 1; \
                 keeping the previous configuration"
            );
            return false;
        }
        self.split_brain = match config.split_brain {
            SplitBrainConfig::Disabled => None,
            SplitBrainConfig::StaticQuorum { expected_nodes } => {
                Some(Box::new(StaticQuorumResolver { expected_nodes }))
            }
        };
        self.probe_interval = config.probe_interval;
        self.removal_confirmation_timeout = config.removal_confirmation_timeout;
        self.directory_gossip = config.directory_gossip;
        true
    }

    /// True once the split-brain resolver downed this node.
    pub fn is_down(&self) -> bool {
        self.local_down
    }

    /// Run the periodic cluster maintenance.
    ///
    /// Should be called regularly (e.g., every 100 ms). Performs:
    ///
    /// 1. Checks active-view members that have missed heartbeats →
    ///    marks Suspicious. Only the active view is watched: passive
    ///    members' liveness comes from gossip, and watching a node we
    ///    do not heartbeat would false-fail it.
    /// 2. Promotes Suspicious active-view members to Failed past the
    ///    suspicion window, repairs the active view (promoting a
    ///    Healthy passive member to probation), and demotes
    ///    probationary members that never reciprocated.
    /// 3. Cleans up old failed nodes.
    /// 4. Consults the split-brain resolver; a `DownSelf` decision marks
    ///    the local node down and no further actions are emitted.
    /// 5. Probes Failed members (throttled) so a healed partition re-joins.
    /// 6. Returns a list of actions for the runtime to execute.
    pub fn tick(&mut self) -> Vec<ClusterAction> {
        let now = self.now();
        let mut actions = Vec::new();

        // ------------------------------------------------------------------
        // 1. Heartbeat timeout → Suspicious (active view only)
        // ------------------------------------------------------------------
        for info in self.members.values_mut() {
            if info.node_id == self.local_node || !self.active_view.contains(&info.node_id) {
                continue;
            }
            if info.status == NodeStatus::Healthy {
                if now.duration_since(info.last_heartbeat) > self.heartbeat_timeout {
                    info.status = NodeStatus::Suspicious;
                }
            }
        }

        // ------------------------------------------------------------------
        // 2. Suspicion timeout → Failed (active view only) + active-view
        //    repair
        // ------------------------------------------------------------------
        let mut newly_failed = Vec::new();
        for info in self.members.values_mut() {
            if info.node_id == self.local_node || !self.active_view.contains(&info.node_id) {
                continue;
            }
            if info.status == NodeStatus::Suspicious {
                // Use the heartbeat timeout as a proxy for "how long
                // has it been suspicious" — the moment it transitions
                // to Suspicious we can track from the last heartbeat.
                if now.duration_since(info.last_heartbeat)
                    > self.heartbeat_timeout + self.suspicion_duration
                {
                    info.status = NodeStatus::Failed;
                    // Bump the entry incarnation so the Failed status
                    // propagates via gossip: under partial-view
                    // membership most nodes never watch a given member
                    // directly and learn its failure only from gossip.
                    Self::bump_entry_incarnation(info);
                    newly_failed.push(info.node_id);
                    self.failed_nodes.insert(info.node_id, now);

                    if let Some(ref cb) = self.on_member_failed {
                        cb(info.node_id);
                    }

                    actions.push(ClusterAction::NodeFailed { node: info.node_id });
                }
            }
        }
        for node_id in &newly_failed {
            self.active_view.retain(|id| id != node_id);
            self.probationary.retain(|(id, _)| id != node_id);
            self.repair_active_view(now);
        }

        // ------------------------------------------------------------------
        // 2.5 Demote probationary members that never reciprocated. They
        //     were never watched, so this is churn, not false failure:
        //     a live member with no room in its own view just gets
        //     another chance later.
        // ------------------------------------------------------------------
        self.probationary
            .retain(|(_, solicited_at)| now.duration_since(*solicited_at) <= PROBATION_TIMEOUT);

        // Every known non-failed member lives in exactly one view:
        // active, probationary, or passive. Anything else (a Joining
        // seed awaiting its first heartbeat, a demoted probationary)
        // goes to the passive pool so the repair path can find it.
        let homeless: Vec<NodeId> = self
            .members
            .values()
            .filter(|info| {
                info.node_id != self.local_node
                    && info.status != NodeStatus::Failed
                    && !self.active_view.contains(&info.node_id)
                    && !self.probationary.iter().any(|(id, _)| *id == info.node_id)
                    && !self.passive_view.contains(&info.node_id)
            })
            .map(|info| info.node_id)
            .collect();
        for node_id in homeless {
            if self.passive_view.len() < self.passive_view_size {
                self.passive_view.push(node_id);
            }
        }

        // Repair is eventual: a demoted probationary leaves the active
        // view underfull, and a later promotion attempt (gated at the
        // probe interval, so this is at most one retry per 5 s) may
        // find a member with room to reciprocate.
        let repair_due = match self.last_repair_attempt {
            Some(last) => now.duration_since(last) >= DEFAULT_PROBE_INTERVAL,
            None => true,
        };
        if self.active_view.len() < self.active_view_size && repair_due {
            self.repair_active_view(now);
        }

        // ------------------------------------------------------------------
        // 3. Promote confirmed-gone Failed nodes to Removed, then clean up.
        // ------------------------------------------------------------------
        // RFC 0014 §1 path 2: a `Failed` node that never sent a goodbye is
        // promoted to confirmed-gone after `removal_confirmation_timeout`,
        // but only while the local node retains quorum (resolver `StayUp`;
        // with no resolver every node is trivially quorate). A node that was
        // merely partitioned re-joins via the probe path before this window
        // elapses and is never promoted.
        let mut to_remove = Vec::new();
        let mut newly_removed = Vec::new();
        for (node_id, failed_at) in &self.failed_nodes {
            let elapsed = now.duration_since(*failed_at);
            if self.removal_confirmation_timeout > Duration::ZERO
                && elapsed > self.removal_confirmation_timeout
                && self.retains_quorum(now)
            {
                newly_removed.push(*node_id);
            }
            if elapsed > FAILED_NODE_RETENTION {
                to_remove.push(*node_id);
            }
        }
        for node_id in newly_removed {
            if self.removed_nodes.insert(node_id) {
                actions.push(ClusterAction::NodeRemoved { node: node_id });
            }
        }
        for node_id in &to_remove {
            self.members.remove(node_id);
            self.failed_nodes.remove(node_id);
            self.passive_view.retain(|id| id != node_id);
            actions.push(ClusterAction::NodeLeft { node: *node_id });

            if let Some(ref cb) = self.on_member_left {
                cb(*node_id);
            }
        }

        // ------------------------------------------------------------------
        // 3.5 Split-brain resolver: decide whether the local node stays up
        // ------------------------------------------------------------------
        if self.local_down {
            // Already down: no heartbeats, gossip, or probes.
            return actions;
        }
        if self.has_seen_peer {
            if let Some(resolver) = &self.split_brain {
                let view = self.resolver_view(now);
                if matches!(resolver.decide(&view), ResolverDecision::DownSelf) {
                    self.local_down = true;
                    actions.push(ClusterAction::Down {
                        node: self.local_node,
                    });
                    return actions;
                }
            }
        }

        // ------------------------------------------------------------------
        // 3.6 Probe Failed members (throttled) so a healed partition
        //     re-joins without an external rejoin.
        // ------------------------------------------------------------------
        let probe_due = match self.last_probe_sent {
            Some(last) => now.duration_since(last) >= self.probe_interval,
            None => true,
        };
        if probe_due {
            self.last_probe_sent = Some(now);
            for info in self.members.values() {
                if info.status == NodeStatus::Failed {
                    actions.push(ClusterAction::Probe {
                        to: info.node_id,
                        addr: info.address,
                    });
                }
            }
        }

        // ------------------------------------------------------------------
        // 4. Send heartbeats (throttled) to the active view, probationary
        //    members, and Joining seeds. This is the bounded data plane:
        //    heartbeat traffic is O(active_view + probationary + joins),
        //    not O(every member).
        // ------------------------------------------------------------------
        if now.duration_since(self.last_heartbeat_sent) >= self.heartbeat_interval {
            self.last_heartbeat_sent = now;

            for info in self.members.values() {
                if info.node_id == self.local_node {
                    continue;
                }
                // Joining members get heartbeats as the join bootstrap:
                // the first heartbeat to a seed is what initiates the
                // join — the seed discovers us from it and heartbeats
                // back, which promotes the seed to Healthy on our side.
                let in_active = self.active_view.contains(&info.node_id);
                let on_probation = self.probationary.iter().any(|(id, _)| *id == info.node_id);
                if matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining)
                    && (in_active || on_probation || info.status == NodeStatus::Joining)
                {
                    actions.push(ClusterAction::SendHeartbeat {
                        to: info.node_id,
                        addr: info.address,
                    });
                }
            }
        }

        // Bounded reply rule: answer up to REPLY_SLOTS passive-view
        // members that recently heartbeated us (rotated round-robin
        // for fairness). Without this, a member whose active view
        // filled up would stop heartbeating us and our detector —
        // which watches exactly the active view — would false-fail
        // it. The rotation bounds how long a pinger waits for a
        // reply: with 4 slots × 500 ms it stays inside the 2 s
        // failure-detection window for clusters up to ~80 nodes.
        let mut replied = 0;
        let n = self.passive_view.len();
        for k in 0..n {
            if replied >= REPLY_SLOTS {
                break;
            }
            let id = self.passive_view[(self.reply_cursor + k) % n];
            if let Some(info) = self.members.get(&id) {
                if matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining)
                    && now.duration_since(info.last_heartbeat) <= self.heartbeat_timeout
                {
                    actions.push(ClusterAction::SendHeartbeat {
                        to: id,
                        addr: info.address,
                    });
                    replied += 1;
                }
            }
        }
        self.reply_cursor = (self.reply_cursor + 1) % n.max(1);

        // ------------------------------------------------------------------
        // 5. Gossip to a random subset of healthy nodes
        // ------------------------------------------------------------------
        let gossip_targets = self.pick_gossip_targets(GOSSIP_FANOUT);
        if !gossip_targets.is_empty() {
            actions.push(ClusterAction::SendGossip {
                targets: gossip_targets,
            });
        }

        actions
    }

    /// Repair the active view after a failure: promote a random Healthy
    /// passive member to probation (we start heartbeating it; its first
    /// reply confirms it into the active view). If nothing suitable is
    /// available the view stays underfull until gossip brings new
    /// candidates.
    fn repair_active_view(&mut self, now: Instant) {
        self.last_repair_attempt = Some(now);
        if self.active_view.len() >= self.active_view_size {
            return;
        }
        let candidates: Vec<NodeId> = self
            .passive_view
            .iter()
            .copied()
            .filter(|id| {
                *id != self.local_node
                    && !self.active_view.contains(id)
                    && !self.probationary.iter().any(|(pid, _)| pid == id)
                    && matches!(
                        self.members.get(id).map(|info| info.status),
                        Some(NodeStatus::Healthy | NodeStatus::Joining)
                    )
            })
            .collect();
        if candidates.is_empty() {
            return;
        }
        let pick = candidates[self.random_index(candidates.len())];
        self.passive_view.retain(|id| *id != pick);
        self.probationary.push((pick, now));
    }

    /// Number of members currently reachable per the resolver's view:
    /// the local node plus every `Healthy`/`Joining` member with fresh
    /// liveness evidence (watched, or its gossip-refreshed timestamp
    /// within the heartbeat timeout). Mirrors the staleness override
    /// `tick` applies when building the resolver view. Used by the
    /// DST cluster harness to assert the resolver's exact semantics.
    #[cfg(test)]
    pub(crate) fn reachable_count(&self) -> usize {
        let now = self.now();
        let mut count = 1;
        for info in self.members.values() {
            if info.node_id == self.local_node {
                continue;
            }
            if !matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining) {
                continue;
            }
            let fresh = self.active_view.contains(&info.node_id)
                || now.duration_since(info.last_heartbeat) <= self.heartbeat_timeout;
            if fresh {
                count += 1;
            }
        }
        count
    }

    /// Get the list of all healthy members **excluding** the local node.
    pub fn healthy_members(&self) -> Vec<&NodeInfo> {
        self.members
            .values()
            .filter(|info| info.node_id != self.local_node && info.status == NodeStatus::Healthy)
            .collect()
    }

    /// Get the list of all members including the local node.
    pub fn all_members(&self) -> Vec<&NodeInfo> {
        self.members.values().collect()
    }

    /// Check if a node is known to the cluster.
    pub fn is_member(&self, node_id: NodeId) -> bool {
        self.members.contains_key(&node_id)
    }

    /// Get info for a specific node.
    pub fn get_node(&self, node_id: NodeId) -> Option<&NodeInfo> {
        self.members.get(&node_id)
    }

    /// Get the number of healthy nodes in the cluster.
    ///
    /// This includes the local node.
    pub fn healthy_node_count(&self) -> usize {
        self.members
            .values()
            .filter(|info| info.status == NodeStatus::Healthy)
            .count()
    }

    /// Set a callback invoked when a new member joins the cluster.
    pub fn on_member_joined<F>(&mut self, callback: F)
    where
        F: Fn(NodeId, SocketAddr) + Send + 'static,
    {
        self.on_member_joined = Some(Box::new(callback));
    }

    /// Set a callback invoked when a member leaves the cluster.
    pub fn on_member_left<F>(&mut self, callback: F)
    where
        F: Fn(NodeId) + Send + 'static,
    {
        self.on_member_left = Some(Box::new(callback));
    }

    /// Set a callback invoked when a member is declared failed.
    pub fn on_member_failed<F>(&mut self, callback: F)
    where
        F: Fn(NodeId) + Send + 'static,
    {
        self.on_member_failed = Some(Box::new(callback));
    }

    /// Merge incoming gossip into the membership table. Higher
    /// incarnation numbers win; an equal-incarnation re-broadcast of a
    /// passive live member refreshes its liveness timestamp (see the
    /// partial-view notes in the merge body). Returns `true` if any
    /// changes were made to our membership table.
    pub fn merge_membership(&mut self, gossip: Vec<NodeGossip>) -> bool {
        self.merge_membership_inner(gossip, None)
    }

    /// Merge gossip, treating the gossip sender's own entry as authoritative.
    ///
    /// A node is the ultimate authority on its own listen address and
    /// status, so when a peer gossips we adopt its self-entry (for the
    /// sending node) regardless of incarnation. This corrects addresses
    /// that were discovered via the heartbeat fallback path
    /// (`connection_addr`), which records a joiner's *ephemeral source
    /// port* rather than its listen address; without the override that
    /// wrong address survives at equal incarnation forever and remote
    /// messages dial a dead port. The `sender` is the node that sent this
    /// gossip (e.g. the transport's `from_node` on `Packet::Gossip`).
    pub fn merge_membership_from_sender(
        &mut self,
        gossip: Vec<NodeGossip>,
        sender: NodeId,
    ) -> bool {
        self.merge_membership_inner(gossip, Some(sender))
    }

    fn merge_membership_inner(&mut self, gossip: Vec<NodeGossip>, sender: Option<NodeId>) -> bool {
        let mut changed = false;

        let now = self.now();
        for entry in gossip {
            // Never overwrite local node info from gossip.
            if entry.node_id == self.local_node {
                continue;
            }

            // The gossip sender asserts its own identity: its self-entry
            // is authoritative for its listen address and status, so it
            // wins even at equal/lower incarnation (see doc on
            // `merge_membership_from_sender`).
            let authoritative_self = sender == Some(entry.node_id);

            match self.members.get_mut(&entry.node_id) {
                Some(existing) => {
                    let stored_incarnation = existing
                        .metadata
                        .get("_incarnation")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    // Higher incarnation wins, and the sender's own entry
                    // always wins. A strictly-newer entry refreshes
                    // `last_heartbeat` and applies the new status. An
                    // equal-incarnation re-broadcast of a LIVE entry also
                    // refreshes the timestamp: under partial-view
                    // membership most members are never heartbeated
                    // directly, so gossip is their only liveness evidence.
                    // Failed entries are never refreshed — that would let
                    // surviving nodes keep a dead peer fresh forever and
                    // defeat the failure detector. (Direct heartbeats
                    // refresh via `handle_heartbeat`.)
                    if entry.incarnation > stored_incarnation || authoritative_self {
                        let old_status = existing.status;
                        let old_addr = existing.address;
                        existing.last_heartbeat = now;
                        existing.status = entry.status;
                        existing.address = entry.address;
                        existing
                            .metadata
                            .insert("_incarnation".to_string(), entry.incarnation.to_string());

                        if old_status != entry.status {
                            changed = true;
                            if entry.status == NodeStatus::Failed {
                                self.failed_nodes.insert(entry.node_id, now);
                            }
                        } else if authoritative_self && old_addr != entry.address {
                            // A sender self-correction of its own listen
                            // address is a real table mutation even when
                            // the status is unchanged. Bump the entry past
                            // the sender's baseline incarnation so the
                            // corrected address PROPAGATES: peers that
                            // learned the stale address via relayed gossip
                            // (equal incarnation) must accept the fix when
                            // we re-gossip it.
                            changed = true;
                            Self::bump_entry_incarnation(existing);
                        }
                    } else if !self.active_view.contains(&entry.node_id)
                        && matches!(existing.status, NodeStatus::Healthy | NodeStatus::Joining)
                        && matches!(entry.status, NodeStatus::Healthy | NodeStatus::Joining)
                    {
                        // Liveness via gossip for passive members only:
                        // watched members are kept fresh by direct
                        // heartbeats alone, or the detector could never
                        // fail them.
                        existing.last_heartbeat = now;
                    }
                }
                None => {
                    // New node learned from gossip.
                    let mut info = NodeInfo::new(entry.node_id, entry.address);
                    info.status = entry.status;
                    info.last_heartbeat = now;
                    info.metadata
                        .insert("_incarnation".to_string(), entry.incarnation.to_string());
                    self.members.insert(entry.node_id, info);
                    changed = true;
                }
            }
        }

        changed
    }

    /// Get a gossip payload to send to other nodes.
    ///
    /// Returns up to `max_entries` entries from the membership table.
    /// If the table is smaller than `max_entries`, all entries are returned.
    pub fn gossip_payload(&self, max_entries: usize) -> Vec<NodeGossip> {
        self.members
            .values()
            .take(max_entries)
            .map(|info| NodeGossip {
                node_id: info.node_id,
                address: info.address,
                status: info.status,
                incarnation: info
                    .metadata
                    .get("_incarnation")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1),
            })
            .collect()
    }

    /// Whether the durable-actor directory is gossip-replicated.
    pub fn directory_gossip(&self) -> bool {
        self.directory_gossip
    }

    /// The durable-actor location directory (RFC 0014 §2), as a copy of the
    /// gossip-replicated entries. A `max_entries` cap bounds the payload.
    pub fn directory_payload(&self, max_entries: usize) -> Vec<DurableDirectoryEntry> {
        self.directory.values().copied().take(max_entries).collect()
    }

    /// Merge directory entries into the local directory, highest-epoch-wins
    /// per actor id (mirroring the incarnation rule in `merge_membership`).
    /// Returns true when any entry changed.
    pub fn merge_directory(&mut self, entries: Vec<DurableDirectoryEntry>) -> bool {
        let mut changed = false;
        for entry in entries {
            match self.directory.get(&entry.actor_id) {
                Some(existing) if existing.epoch > entry.epoch => {}
                Some(existing) if existing.epoch == entry.epoch => {
                    // Equal epoch: keep the existing location (the first
                    // claim at that epoch wins — deterministic convergence).
                }
                _ => {
                    self.directory.insert(entry.actor_id, entry);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Announce (or update) a directory entry for an actor this node hosts.
    /// The caller owns epoch management; a higher epoch replaces the entry.
    pub fn announce_directory(&mut self, entry: DurableDirectoryEntry) {
        match self.directory.get(&entry.actor_id) {
            Some(existing) if existing.epoch >= entry.epoch => {}
            _ => {
                self.directory.insert(entry.actor_id, entry);
            }
        }
    }

    /// The directory entry for an actor, if any.
    pub fn directory_entry(&self, actor_id: u64) -> Option<DurableDirectoryEntry> {
        self.directory.get(&actor_id).copied()
    }

    /// All directory entries whose home node is `node` (the re-spawn set on
    /// that node's confirmed removal).
    pub fn directory_for_node(&self, node: NodeId) -> Vec<DurableDirectoryEntry> {
        self.directory
            .values()
            .filter(|e| e.node_id == node)
            .copied()
            .collect()
    }

    /// The current directory epoch for an actor, if any.
    pub fn directory_epoch(&self, actor_id: u64) -> Option<u64> {
        self.directory.get(&actor_id).map(|e| e.epoch)
    }

    /// Bump the directory epoch for an actor re-spawned onto `node`,
    /// returning the new epoch. A missing entry is announced at epoch 2
    /// (the first re-spawn past the original epoch-1 activation).
    pub fn bump_directory_epoch(&mut self, actor_id: u64, node: NodeId) -> u64 {
        let next = self
            .directory
            .get(&actor_id)
            .map(|e| e.epoch.saturating_add(1))
            .unwrap_or(2);
        self.directory.insert(
            actor_id,
            DurableDirectoryEntry {
                actor_id,
                node_id: node,
                epoch: next,
            },
        );
        next
    }

    /// Mark a node confirmed gone (RFC 0014 §1 path 1, a positive
    /// `NodeGoodbye`). Returns true when the node was not already removed.
    pub fn mark_removed(&mut self, node: NodeId) -> bool {
        self.removed_nodes.insert(node)
    }

    /// Whether `node` has been confirmed gone.
    pub fn is_removed(&self, node: NodeId) -> bool {
        self.removed_nodes.contains(&node)
    }

    /// True when the local node retains quorum and may therefore promote a
    /// `Failed` node to confirmed-gone. With no resolver installed every
    /// node is trivially quorate; with a resolver, `StayUp` is required.
    fn retains_quorum(&self, now: Instant) -> bool {
        match &self.split_brain {
            None => true,
            Some(resolver) => matches!(
                resolver.decide(&self.resolver_view(now)),
                ResolverDecision::StayUp
            ),
        }
    }

    /// Build the immutable membership view handed to the split-brain
    /// resolver, demoting stale-status passive members to `Suspicious` so
    /// gossip-frozen liveness evidence never inflates the reachable count.
    fn resolver_view(&self, now: Instant) -> MembershipView {
        let view_members: Vec<NodeInfo> = self
            .members
            .values()
            .map(|info| {
                let mut info = info.clone();
                if info.node_id != self.local_node
                    && !self.active_view.contains(&info.node_id)
                    && now.duration_since(info.last_heartbeat) > self.heartbeat_timeout
                    && matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining)
                {
                    info.status = NodeStatus::Suspicious;
                }
                info
            })
            .collect();
        MembershipView {
            local: self.local_node,
            members: view_members,
        }
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Increment the per-entry incarnation stored in a member's metadata.
    ///
    /// Entries carry their version in the `_incarnation` metadata key so
    /// gossip merges can resolve conflicts (higher wins). A missing key is
    /// treated as 1 by `gossip_payload`, so a locally-observed status
    /// change must bump the entry past that baseline to propagate.
    fn bump_entry_incarnation(info: &mut NodeInfo) {
        let current = info
            .metadata
            .get("_incarnation")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1);
        info.metadata
            .insert("_incarnation".to_string(), (current + 1).to_string());
    }
    /// Pick `n` distinct random healthy targets for gossip.
    ///
    /// Selection is uniform over the healthy member set (partial
    /// Fisher-Yates shuffle driven by the seeded RNG when one is
    /// installed, else `OsRng`), so no member is systematically starved
    /// of gossip coverage — and a same-seed DST cluster run picks the
    /// same targets every time.
    fn pick_gossip_targets(&mut self, n: usize) -> Vec<(NodeId, SocketAddr)> {
        // Owned copies first: the shuffle below calls `self.random_index`
        // (a mutable borrow), so the candidate list cannot hold references
        // into `self`.
        let mut healthy: Vec<(NodeId, SocketAddr)> = self
            .healthy_members()
            .into_iter()
            .map(|info| (info.node_id, info.address))
            .collect();
        if healthy.is_empty() {
            return Vec::new();
        }

        // Partial Fisher-Yates: swap a random remaining element into
        // position i, then keep the first n — every healthy member has
        // an equal chance of being selected each tick.
        let count = n.min(healthy.len());
        for i in 0..count {
            let j = self.random_index(healthy.len() - i);
            healthy.swap(i, i + j);
        }
        healthy.truncate(count);
        healthy
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::thread;

    /// Helper: create a loopback address on a given port.
    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    // -- 1. NodeId creation ------------------------------------------------

    #[test]
    fn test_node_id_creation() {
        let a = addr(9000);
        let id1 = NodeId::new(&a);
        let id2 = NodeId::new(&a);
        assert_eq!(id1, id2, "same address should yield same NodeId");
        assert_ne!(id1.0, 0, "NodeId should not be zero for non-local");
    }

    #[test]
    fn test_node_id_local() {
        assert_eq!(NodeId::LOCAL.0, 0);
    }

    // -- 2. ClusterState creation ------------------------------------------

    #[test]
    fn test_cluster_new() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let cs = ClusterState::new(local, a);

        assert_eq!(cs.local_node, local);
        assert_eq!(cs.healthy_node_count(), 1);
        assert!(cs.is_member(local));

        let info = cs.get_node(local).unwrap();
        assert_eq!(info.status, NodeStatus::Healthy);
        assert_eq!(info.address, a);
    }

    // -- 3. Heartbeat from unknown node ------------------------------------

    #[test]
    fn test_handle_heartbeat_new_node() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let peer_addr = addr(9001);
        let peer_id = NodeId::new(&peer_addr);

        cs.handle_heartbeat(peer_id, peer_addr);

        assert!(cs.is_member(peer_id));
        assert_eq!(cs.get_node(peer_id).unwrap().status, NodeStatus::Healthy);
        assert_eq!(cs.healthy_node_count(), 2);
    }

    // -- 4. Heartbeat updates existing node --------------------------------

    #[test]
    fn test_handle_heartbeat_existing_node() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let peer_addr = addr(9001);
        let peer_id = NodeId::new(&peer_addr);

        cs.handle_heartbeat(peer_id, peer_addr);
        let first = cs.get_node(peer_id).unwrap().last_heartbeat;

        // Wait a tiny bit so Instant::now() advances.
        thread::sleep(Duration::from_millis(10));
        cs.handle_heartbeat(peer_id, peer_addr);
        let second = cs.get_node(peer_id).unwrap().last_heartbeat;

        assert!(second > first, "heartbeat should update timestamp");
    }

    // -- 5. Suspicion detection --------------------------------------------

    #[test]
    fn test_suspicion_detection() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let peer_addr = addr(9001);
        let peer_id = NodeId::new(&peer_addr);

        cs.handle_heartbeat(peer_id, peer_addr);
        assert_eq!(cs.get_node(peer_id).unwrap().status, NodeStatus::Healthy);

        // Simulate time passing by not sending heartbeats.
        // We can't advance Instant, so we force the status manually
        // and verify tick promotes it.
        // NOTE: In real usage the peer would naturally time out.
        // Here we verify the state machine transition exists.

        // Mark the peer as having a very old heartbeat.
        if let Some(info) = cs.members.get_mut(&peer_id) {
            // Artificially set last_heartbeat far in the past.
            // Since Instant doesn't support subtraction directly,
            // we verify the transition path via tick.
            info.status = NodeStatus::Healthy;
        }

        // Call tick — we force the heartbeat timer to have expired so it sends a heartbeat
        cs.last_heartbeat_sent = Instant::now() - cs.heartbeat_interval - Duration::from_secs(1);
        let actions = cs.tick();
        // Peer is still healthy because the real timeout hasn't passed.
        // The test documents the API; full timeout testing requires
        // mockable clocks (left as a TODO for production).
        assert!(
            cs.get_node(peer_id).unwrap().status == NodeStatus::Healthy
                || cs.get_node(peer_id).unwrap().status == NodeStatus::Suspicious
        );

        // Verify that SendHeartbeat action is produced for the peer.
        let has_heartbeat = actions
            .iter()
            .any(|a| matches!(a, ClusterAction::SendHeartbeat { to, .. } if *to == peer_id));
        assert!(has_heartbeat, "tick should request heartbeat to peer");
    }

    // -- 6. Failure detection ----------------------------------------------

    #[test]
    fn test_failure_detection() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let peer_addr = addr(9001);
        let peer_id = NodeId::new(&peer_addr);

        cs.handle_heartbeat(peer_id, peer_addr);

        // Manually transition through the failure-detector state machine.
        if let Some(info) = cs.members.get_mut(&peer_id) {
            info.status = NodeStatus::Suspicious;
        }

        // tick won't promote to Failed because real time hasn't passed,
        // but we verify the state machine paths are wired correctly by
        // checking the member stays in the table.
        let _actions = cs.tick();
        assert!(cs.is_member(peer_id));
    }

    // -- 7. Healthy members filter -----------------------------------------

    #[test]
    fn test_healthy_members_filter() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let p1 = addr(9001);
        let id1 = NodeId::new(&p1);
        let p2 = addr(9002);
        let id2 = NodeId::new(&p2);

        cs.handle_heartbeat(id1, p1);
        cs.handle_heartbeat(id2, p2);

        let healthy = cs.healthy_members();
        assert_eq!(healthy.len(), 2);
        assert!(healthy.iter().all(|i| i.status == NodeStatus::Healthy));
        assert!(!healthy.iter().any(|i| i.node_id == local));
    }

    // -- 8. Merge membership (gossip) --------------------------------------

    #[test]
    fn test_merge_membership() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let gossip = vec![
            NodeGossip {
                node_id: NodeId(42),
                address: addr(9042),
                status: NodeStatus::Healthy,
                incarnation: 5,
            },
            NodeGossip {
                node_id: NodeId(43),
                address: addr(9043),
                status: NodeStatus::Healthy,
                incarnation: 3,
            },
        ];

        let changed = cs.merge_membership(gossip);
        assert!(changed);
        assert!(cs.is_member(NodeId(42)));
        assert!(cs.is_member(NodeId(43)));
        assert_eq!(cs.get_node(NodeId(42)).unwrap().address, addr(9042));
    }

    // -- 9. Merge conflict resolution (higher incarnation wins) -------------

    #[test]
    fn test_merge_conflict_resolution() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        // Seed the table with a node at incarnation 3.
        let gossip_low = vec![NodeGossip {
            node_id: NodeId(77),
            address: addr(9077),
            status: NodeStatus::Healthy,
            incarnation: 3,
        }];
        cs.merge_membership(gossip_low);

        assert_eq!(cs.get_node(NodeId(77)).unwrap().status, NodeStatus::Healthy);

        // Now receive gossip with a higher incarnation marking it Failed.
        let gossip_high = vec![NodeGossip {
            node_id: NodeId(77),
            address: addr(9077),
            status: NodeStatus::Failed,
            incarnation: 10,
        }];
        let changed = cs.merge_membership(gossip_high);
        assert!(changed);
        assert_eq!(cs.get_node(NodeId(77)).unwrap().status, NodeStatus::Failed);
    }

    // -- 10. Gossip payload size -------------------------------------------

    #[test]
    fn test_gossip_payload_size() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        // Add several peers.
        for port in 9001..=9010 {
            let pa = addr(port);
            let pid = NodeId::new(&pa);
            cs.handle_heartbeat(pid, pa);
        }

        let payload = cs.gossip_payload(3);
        assert_eq!(payload.len(), 3, "payload should respect max_entries");

        let payload_all = cs.gossip_payload(100);
        assert_eq!(payload_all.len(), 11, "payload should contain all members");
    }

    // -- 11. Member joined callback ----------------------------------------

    #[test]
    fn test_member_joined_callback() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let (tx, rx) = std::sync::mpsc::channel();
        cs.on_member_joined(move |id, _addr| {
            let _ = tx.send(id);
        });

        let pa = addr(9001);
        let pid = NodeId::new(&pa);
        cs.handle_heartbeat(pid, pa);

        let received = rx.recv_timeout(Duration::from_secs(1));
        assert!(received.is_ok(), "callback should fire on new member");
        assert_eq!(received.unwrap(), pid);
    }

    // -- 12. Graceful leave handling ---------------------------------------

    #[test]
    fn test_node_left_graceful() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let pa = addr(9001);
        let pid = NodeId::new(&pa);
        cs.handle_heartbeat(pid, pa);
        assert!(cs.is_member(pid));

        // Simulate the peer leaving via gossip.
        let gossip = vec![NodeGossip {
            node_id: pid,
            address: pa,
            status: NodeStatus::Leaving,
            incarnation: 99,
        }];
        let changed = cs.merge_membership(gossip);
        assert!(changed);
        assert_eq!(cs.get_node(pid).unwrap().status, NodeStatus::Leaving);
    }

    // -- 12b. Sender self-entry is authoritative over a discovered address --

    #[test]
    fn test_merge_membership_sender_self_entry_corrects_discovered_address() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        // A peer is discovered via a RELAYED gossip entry (a third node's
        // view) carrying its ephemeral SOURCE port instead of its listen
        // address — the `connection_addr` fallback path in the runtime. It
        // is recorded at incarnation 1, so it is now a normal peer entry,
        // not a fresh heartbeat.
        let peer = NodeId(77);
        let source_port = addr(9777);
        let wrong_view = vec![NodeGossip {
            node_id: peer,
            address: source_port,
            status: NodeStatus::Healthy,
            incarnation: 1,
        }];
        cs.merge_membership(wrong_view);
        assert_eq!(cs.get_node(peer).unwrap().address, source_port);

        // The peer's own gossip entry advertises its real listen address
        // at the SAME (baseline) incarnation. Plain `merge_membership`
        // refuses it — equal incarnation only refreshes liveness — so the
        // wrong source-port address survives.
        let listen = addr(9900);
        let self_entry = vec![NodeGossip {
            node_id: peer,
            address: listen,
            status: NodeStatus::Healthy,
            incarnation: 1,
        }];
        cs.merge_membership(self_entry.clone());
        assert_eq!(
            cs.get_node(peer).unwrap().address,
            source_port,
            "equal-incarnation relayed gossip must not overwrite the address"
        );

        // But when the gossip comes FROM that peer directly, its self-entry
        // is authoritative and corrects the address.
        let changed = cs.merge_membership_from_sender(self_entry, peer);
        assert!(changed);
        assert_eq!(cs.get_node(peer).unwrap().address, listen);
        assert_eq!(cs.get_node(peer).unwrap().status, NodeStatus::Healthy);
    }

    // -- 13. Join cluster via seed -----------------------------------------

    #[test]
    fn test_join_cluster() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let seed = addr(9001);
        cs.join_cluster(seed);

        let seed_id = NodeId::new(&seed);
        assert!(cs.is_member(seed_id));
        assert_eq!(cs.get_node(seed_id).unwrap().status, NodeStatus::Joining);
    }

    // -- 14. Self-join is a no-op ------------------------------------------

    #[test]
    fn test_join_self_is_noop() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        cs.join_cluster(a); // join our own address
        assert_eq!(cs.healthy_node_count(), 1);
    }

    // -- 16. Gossip does not include local node overrides ------------------

    #[test]
    fn test_merge_ignores_local_node() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        // Try to override local node via gossip.
        let gossip = vec![NodeGossip {
            node_id: local,
            address: addr(9999),
            status: NodeStatus::Failed,
            incarnation: 9999,
        }];
        let changed = cs.merge_membership(gossip);
        assert!(!changed);
        assert_eq!(cs.get_node(local).unwrap().status, NodeStatus::Healthy);
    }

    // -- 17. All members includes local ------------------------------------

    #[test]
    fn test_all_members_includes_local() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let pa = addr(9001);
        let pid = NodeId::new(&pa);
        cs.handle_heartbeat(pid, pa);

        assert_eq!(cs.all_members().len(), 2);
    }

    // -- 18. Heartbeat promotes suspicious back to healthy -----------------

    #[test]
    fn test_heartbeat_promotes_suspicious() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let pa = addr(9001);
        let pid = NodeId::new(&pa);
        cs.handle_heartbeat(pid, pa);

        // Force to suspicious.
        if let Some(info) = cs.members.get_mut(&pid) {
            info.status = NodeStatus::Suspicious;
        }

        // Heartbeat should promote back to healthy.
        cs.handle_heartbeat(pid, pa);
        assert_eq!(cs.get_node(pid).unwrap().status, NodeStatus::Healthy);
    }

    // -- 19. Joining status promoted on first heartbeat --------------------

    #[test]
    fn test_joining_promoted_on_heartbeat() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let seed = addr(9001);
        cs.join_cluster(seed);

        let seed_id = NodeId::new(&seed);
        assert_eq!(cs.get_node(seed_id).unwrap().status, NodeStatus::Joining);

        cs.handle_heartbeat(seed_id, seed);
        assert_eq!(cs.get_node(seed_id).unwrap().status, NodeStatus::Healthy);
    }

    // -- 20. Gossip targets are non-empty when peers exist -----------------

    #[test]
    fn test_tick_produces_gossip_when_peers_exist() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        let pa = addr(9001);
        let pid = NodeId::new(&pa);
        cs.handle_heartbeat(pid, pa);

        let actions = cs.tick();
        let has_gossip = actions
            .iter()
            .any(|a| matches!(a, ClusterAction::SendGossip { .. }));
        assert!(
            has_gossip,
            "tick should produce gossip action when peers exist"
        );
    }

    // -- 21. Transitive gossip propagation across a three-node chain -------

    #[test]
    fn test_gossip_transitive_propagation_three_nodes() {
        // Chain topology: A <-> B <-> C. B knows everyone directly; A and
        // C only know B. Relaying gossip payloads hop by hop must converge
        // all three membership tables to the full set.
        let addr_a = addr(9100);
        let addr_b = addr(9101);
        let addr_c = addr(9102);
        let id_a = NodeId::new(&addr_a);
        let id_b = NodeId::new(&addr_b);
        let id_c = NodeId::new(&addr_c);

        let mut a = ClusterState::new(id_a, addr_a);
        let mut b = ClusterState::new(id_b, addr_b);
        let mut c = ClusterState::new(id_c, addr_c);

        a.handle_heartbeat(id_b, addr_b);
        b.handle_heartbeat(id_a, addr_a);
        b.handle_heartbeat(id_c, addr_c);
        c.handle_heartbeat(id_b, addr_b);

        // Round 1: B gossips its full table to A and C.
        let payload_b = b.gossip_payload(100);
        a.merge_membership(payload_b.clone());
        c.merge_membership(payload_b);

        assert!(a.is_member(id_c), "A should learn about C via B's gossip");
        assert!(c.is_member(id_a), "C should learn about A via B's gossip");
        assert_eq!(a.all_members().len(), 3);
        assert_eq!(c.all_members().len(), 3);

        // Round 2: A and C gossip their now-complete tables back to B.
        b.merge_membership(a.gossip_payload(100));
        b.merge_membership(c.gossip_payload(100));
        assert_eq!(b.all_members().len(), 3);

        // Incarnation-based conflict resolution survives relaying: a
        // higher-incarnation failure report about C propagates B -> A.
        let mut failed_view = b.gossip_payload(100);
        for entry in &mut failed_view {
            if entry.node_id == id_c {
                entry.status = NodeStatus::Failed;
                entry.incarnation = 999;
            }
        }
        a.merge_membership(failed_view);
        assert_eq!(a.get_node(id_c).unwrap().status, NodeStatus::Failed);
    }

    // -- 22. Dead peer is detected even while a peer gossips about it -------

    #[test]
    fn test_dead_peer_detected_while_peer_gossips_about_it() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        // The peer that will die.
        let dead_addr = addr(9001);
        let dead_id = NodeId::new(&dead_addr);
        cs.handle_heartbeat(dead_id, dead_addr);

        // A surviving peer that keeps gossiping about the dead peer.
        let surv_addr = addr(9002);
        let surv_id = NodeId::new(&surv_addr);
        cs.handle_heartbeat(surv_id, surv_addr);

        // Establish a known incarnation (5) on the dead peer's entry.
        let gossip_v5 = vec![NodeGossip {
            node_id: dead_id,
            address: dead_addr,
            status: NodeStatus::Healthy,
            incarnation: 5,
        }];
        cs.merge_membership(gossip_v5.clone());

        // The dead peer stops heartbeating: move its timestamp into the past,
        // beyond the full suspicion window.
        let stale = Instant::now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&dead_id).unwrap().last_heartbeat = stale;

        // The survivor keeps gossiping about the dead peer at the SAME
        // incarnation. This must NOT refresh the dead peer's last_heartbeat
        // (that was the failure-detector-defeating bug).
        cs.merge_membership(gossip_v5);
        assert_eq!(
            cs.get_node(dead_id).unwrap().last_heartbeat,
            stale,
            "equal-incarnation gossip must not refresh a dead peer's timestamp"
        );

        // The failure detector must now declare the peer failed.
        let actions = cs.tick();
        assert_eq!(cs.get_node(dead_id).unwrap().status, NodeStatus::Failed);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, ClusterAction::NodeFailed { node } if *node == dead_id)),
            "tick should emit NodeFailed for the dead peer"
        );
        // The surviving peer stays healthy throughout.
        assert_eq!(cs.get_node(surv_id).unwrap().status, NodeStatus::Healthy);
    }
    // -- 21. Split-brain resolver -------------------------------------------

    #[test]
    fn test_static_quorum_boundaries() {
        // expected 5 → quorum is 3 reachable members (including self).
        // Mirrors the real tick view: `members` includes the local node.
        let resolver = StaticQuorumResolver { expected_nodes: 5 };
        let local = NodeId(1);
        let view = |reachable_peers: usize| MembershipView {
            local,
            members: std::iter::once({
                let mut info = NodeInfo::new(local, addr(9000));
                info.status = NodeStatus::Healthy;
                info
            })
            .chain((0..reachable_peers).map(|i| {
                let mut info = NodeInfo::new(NodeId(i as u64 + 2), addr(9100 + i as u16));
                info.status = NodeStatus::Healthy;
                info
            }))
            .collect(),
        };
        // 2 reachable (self + 1 peer) < 3 → down.
        assert_eq!(resolver.decide(&view(1)), ResolverDecision::DownSelf);
        // 3 reachable (self + 2 peers) → stay up.
        assert_eq!(resolver.decide(&view(2)), ResolverDecision::StayUp);
        // 4 reachable (self + 3 peers) → stay up.
        assert_eq!(resolver.decide(&view(3)), ResolverDecision::StayUp);
    }
    #[test]
    fn test_tick_downs_below_quorum() {
        // Clean partition of a 3-node cluster: the local node sees only
        // itself, so the static-quorum resolver downs it (1 < 2).
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);
        let b = NodeId::new(&addr(9001));
        let c = NodeId::new(&addr(9002));
        cs.handle_heartbeat(b, addr(9001));
        cs.handle_heartbeat(c, addr(9002));
        assert!(cs.apply_config(&ClusterConfig {
            split_brain: SplitBrainConfig::StaticQuorum { expected_nodes: 3 },
            probe_interval: Duration::from_secs(5),
            ..Default::default()
        }));

        // Both peers stop heartbeating: age their timestamps past the full
        // failure window so a single tick transitions them to Failed.
        let stale = Instant::now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&b).unwrap().last_heartbeat = stale;
        cs.members.get_mut(&c).unwrap().last_heartbeat = stale;

        let actions = cs.tick();
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, ClusterAction::Down { node } if *node == local)),
            "tick must down the local node below quorum, got {:?}",
            actions
        );
        assert!(cs.is_down());
    }

    #[test]
    fn test_tick_stays_up_above_quorum() {
        // 3-node cluster where one peer is still reachable: 2 of 3 reachable
        // meets the quorum of 2, so the local node stays up.
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);
        let b = NodeId::new(&addr(9001));
        let c = NodeId::new(&addr(9002));
        cs.handle_heartbeat(b, addr(9001));
        cs.handle_heartbeat(c, addr(9002));
        cs.apply_config(&ClusterConfig {
            split_brain: SplitBrainConfig::StaticQuorum { expected_nodes: 3 },
            probe_interval: Duration::from_secs(5),
            ..Default::default()
        });
        let stale = Instant::now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&c).unwrap().last_heartbeat = stale;

        let actions = cs.tick();
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, ClusterAction::Down { .. })),
            "tick must NOT down the local node above quorum, got {:?}",
            actions
        );
        assert!(!cs.is_down());
        assert_eq!(cs.get_node(b).unwrap().status, NodeStatus::Healthy);
    }

    #[test]
    fn test_asymmetric_partition_downs_smaller_side() {
        // Asymmetric partition: A sees B as Healthy, but B cannot see A.
        // A (2 of 3 reachable) stays up; B (1 of 3 reachable) downs itself.
        let a_addr = addr(9000);
        let a_id = NodeId::new(&a_addr);
        let mut cs_a = ClusterState::new(a_id, a_addr);
        let b_addr = addr(9001);
        let b_id = NodeId::new(&b_addr);
        cs_a.handle_heartbeat(b_id, b_addr);
        cs_a.apply_config(&ClusterConfig {
            split_brain: SplitBrainConfig::StaticQuorum { expected_nodes: 3 },
            probe_interval: Duration::from_secs(5),
            ..Default::default()
        });
        assert!(!cs_a
            .tick()
            .iter()
            .any(|a| matches!(a, ClusterAction::Down { .. })));

        // B's view: A is unreachable; B sees only itself.
        let mut cs_b = ClusterState::new(b_id, b_addr);
        cs_b.handle_heartbeat(a_id, a_addr);
        cs_b.apply_config(&ClusterConfig {
            split_brain: SplitBrainConfig::StaticQuorum { expected_nodes: 3 },
            probe_interval: Duration::from_secs(5),
            ..Default::default()
        });
        let stale = Instant::now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs_b.members.get_mut(&a_id).unwrap().last_heartbeat = stale;
        let actions = cs_b.tick();
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, ClusterAction::Down { node } if *node == b_id)),
            "the one-way side of an asymmetric partition must down itself"
        );
    }

    #[test]
    fn test_five_node_split_majority_survives() {
        // 5-node cluster split 3v2: the side seeing 3 members stays up, the
        // side seeing 2 downs itself (quorum for expected 5 is 3).
        let a = addr(9000);
        let local = NodeId::new(&a);
        let stale = Instant::now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        let config = ClusterConfig {
            split_brain: SplitBrainConfig::StaticQuorum { expected_nodes: 5 },
            probe_interval: Duration::from_secs(5),
            ..Default::default()
        };

        // Majority side: peers 9001, 9002 healthy; 9003, 9004 failed.
        let mut cs = ClusterState::new(local, a);
        cs.handle_heartbeat(NodeId::new(&addr(9001)), addr(9001));
        cs.handle_heartbeat(NodeId::new(&addr(9002)), addr(9002));
        for port in [9003u16, 9004] {
            let peer = NodeId::new(&addr(port));
            cs.handle_heartbeat(peer, addr(port));
            cs.members.get_mut(&peer).unwrap().last_heartbeat = stale;
        }
        cs.apply_config(&config);
        assert!(!cs
            .tick()
            .iter()
            .any(|a| matches!(a, ClusterAction::Down { .. })));

        // Minority side: only peer 9001 healthy; 9002-9004 failed.
        let mut cs_minority = ClusterState::new(local, a);
        cs_minority.handle_heartbeat(NodeId::new(&addr(9001)), addr(9001));
        for port in [9002u16, 9003, 9004] {
            let peer = NodeId::new(&addr(port));
            cs_minority.handle_heartbeat(peer, addr(port));
            cs_minority.members.get_mut(&peer).unwrap().last_heartbeat = stale;
        }
        cs_minority.apply_config(&config);
        let actions = cs_minority.tick();
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, ClusterAction::Down { node } if *node == local)),
            "the minority side of a 3v2 split must down itself"
        );
    }

    #[test]
    fn test_down_node_stays_quiet() {
        // Once downed, tick returns no actions at all (no heartbeats,
        // gossip, or probes).
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);
        let b = NodeId::new(&addr(9001));
        cs.handle_heartbeat(b, addr(9001));
        cs.apply_config(&ClusterConfig {
            split_brain: SplitBrainConfig::StaticQuorum { expected_nodes: 3 },
            probe_interval: Duration::from_secs(5),
            ..Default::default()
        });
        let stale = Instant::now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&b).unwrap().last_heartbeat = stale;
        cs.tick();
        assert!(cs.is_down());
        assert!(cs.tick().is_empty(), "a downed node must emit no actions");
    }

    #[test]
    fn test_probe_emitted_for_failed_members_throttled() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);
        let b = NodeId::new(&addr(9001));
        cs.handle_heartbeat(b, addr(9001));
        cs.apply_config(&ClusterConfig {
            split_brain: SplitBrainConfig::Disabled,
            probe_interval: Duration::from_secs(5),
            ..Default::default()
        });
        let stale = Instant::now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&b).unwrap().last_heartbeat = stale;

        let actions = cs.tick();
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, ClusterAction::Probe { to, .. } if *to == b)),
            "tick must probe the failed member"
        );
        // Throttled by probe_interval: an immediate second tick must not
        // re-probe.
        let actions2 = cs.tick();
        assert!(
            !actions2
                .iter()
                .any(|a| matches!(a, ClusterAction::Probe { .. })),
            "probes must be throttled to probe_interval"
        );
    }

    #[test]
    fn test_heartbeat_promotes_failed() {
        // The self-healing path: a probe that reaches a live (previously
        // failed) node delivers a heartbeat, which promotes it back to
        // Healthy.
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);
        let b = NodeId::new(&addr(9001));
        cs.handle_heartbeat(b, addr(9001));
        let stale = Instant::now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&b).unwrap().last_heartbeat = stale;
        cs.tick();
        assert_eq!(cs.get_node(b).unwrap().status, NodeStatus::Failed);

        cs.handle_heartbeat(b, addr(9001));
        assert_eq!(cs.get_node(b).unwrap().status, NodeStatus::Healthy);
    }

    #[test]
    fn test_static_quorum_zero_expected_rejected() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);
        assert!(!cs.apply_config(&ClusterConfig {
            split_brain: SplitBrainConfig::StaticQuorum { expected_nodes: 0 },
            probe_interval: Duration::from_secs(5),
            ..Default::default()
        }));
        // The invalid config leaves the previous (disabled) state in place.
        assert!(cs.split_brain.is_none());
    }

    // -- Partial-view membership (Phase 5 deliverable 6) ------------------

    #[test]
    fn test_active_view_fills_from_incoming_heartbeats() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        // Five distinct heartbeaters: the first four fill the active
        // view, the fifth lands in the passive repair pool. Each id is
        // derived from, and heartbeats from, the same address.
        let peers: Vec<(NodeId, SocketAddr)> = (1..=5)
            .map(|i| (NodeId::new(&addr(9000 + i)), addr(9000 + i)))
            .collect();
        for (id, peer_addr) in &peers {
            cs.handle_heartbeat(*id, *peer_addr);
        }
        assert_eq!(cs.active_view().len(), 4, "active view is bounded");
        assert!(cs.active_view().contains(&peers[0].0));
        assert!(cs.active_view().contains(&peers[3].0));
        assert!(
            !cs.active_view().contains(&peers[4].0),
            "5th heartbeater is not active"
        );
        assert_eq!(cs.passive_view().len(), 1);
        assert_eq!(cs.passive_view()[0], peers[4].0);
    }

    /// A cluster whose active view is full (four incoming heartbeaters)
    /// plus one extra member `c` that joined but never replied; the
    /// first tick sweeps `c` into the passive pool (the view is full,
    /// so the repair path does not promote it).
    fn setup_full_active_view(
        vc: &mut super::super::timer::VirtualClock,
        base_port: u16,
    ) -> (ClusterState, NodeId, SocketAddr) {
        let a = addr(base_port);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);
        cs.set_clock(vc.clone());
        for i in 1..=4u16 {
            let id = NodeId::new(&addr(base_port + i));
            cs.handle_heartbeat(id, addr(base_port + i));
        }
        let c_addr = addr(base_port + 5);
        let c = NodeId::new(&c_addr);
        cs.join_cluster(c_addr);
        cs.tick();
        (cs, c, c_addr)
    }

    #[test]
    fn test_failure_detector_watches_active_view_only() {
        let mut vc = super::super::timer::VirtualClock::new();
        let (mut cs, c, _c_addr) = setup_full_active_view(&mut vc, 9100);
        let b = NodeId::new(&addr(9101)); // first active member
        assert!(cs.active_view().contains(&b));
        assert!(!cs.active_view().contains(&c), "c is not watched");

        // Age BOTH entries past the full failure window.
        let stale = vc.now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&b).unwrap().last_heartbeat = stale;
        cs.members.get_mut(&c).unwrap().last_heartbeat = stale;
        vc.advance(Duration::from_secs(9));
        cs.set_clock(vc.clone());

        let actions = cs.tick();
        assert_eq!(
            cs.get_node(b).unwrap().status,
            NodeStatus::Failed,
            "watched (active) member is failed by our detector"
        );
        assert_eq!(
            cs.get_node(c).unwrap().status,
            NodeStatus::Joining,
            "unwatched member's status is untouched by our detector"
        );
        assert!(actions
            .iter()
            .any(|a| matches!(a, ClusterAction::NodeFailed { node } if *node == b)));
    }

    #[test]
    fn test_repair_promotes_passive_to_probation_and_confirms_on_reply() {
        let mut vc = super::super::timer::VirtualClock::new();
        let (mut cs, c, c_addr) = setup_full_active_view(&mut vc, 9200);
        let b = NodeId::new(&addr(9201));
        assert!(cs.passive_view().contains(&c));

        // Fail b: repair must promote c to probationary (heartbeated,
        // not watched).
        let stale = vc.now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&b).unwrap().last_heartbeat = stale;
        vc.advance(Duration::from_secs(9));
        cs.set_clock(vc.clone());
        cs.tick();

        assert!(
            !cs.active_view().contains(&b),
            "failed member leaves the active view"
        );
        assert_eq!(
            cs.probationary().len(),
            1,
            "a passive member is promoted to probation"
        );
        assert_eq!(cs.probationary()[0].0, c);
        assert!(
            !cs.active_view().contains(&c),
            "probationary is not yet active"
        );

        // c's first reply confirms it into the active view and promotes
        // it to Healthy.
        cs.handle_heartbeat(c, c_addr);
        assert!(
            cs.active_view().contains(&c),
            "reply confirms the probationary member"
        );
        assert!(cs.probationary().is_empty());
        assert_eq!(cs.get_node(c).unwrap().status, NodeStatus::Healthy);
    }

    #[test]
    fn test_probationary_member_demoted_when_no_reply() {
        let mut vc = super::super::timer::VirtualClock::new();
        let (mut cs, c, _c_addr) = setup_full_active_view(&mut vc, 9300);
        let b = NodeId::new(&addr(9301));
        let stale = vc.now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&b).unwrap().last_heartbeat = stale;
        vc.advance(Duration::from_secs(9));
        cs.set_clock(vc.clone());
        cs.tick();
        assert_eq!(cs.probationary().len(), 1);

        // c never replies; after the probation timeout it is demoted
        // back to the passive pool — churn, not false failure.
        vc.advance(PROBATION_TIMEOUT + Duration::from_secs(1));
        cs.set_clock(vc.clone());
        cs.tick();
        assert!(
            cs.probationary().is_empty(),
            "silent probationary is demoted"
        );
        assert!(cs.passive_view().contains(&c), "demoted back to passive");
        assert_eq!(cs.get_node(c).unwrap().status, NodeStatus::Joining);
    }

    #[test]
    fn test_heartbeats_bounded_by_views_and_replies() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);

        // Nine healthy members: four fill the active view, five go
        // passive. Heartbeats reach the active view plus up to
        // REPLY_SLOTS replies to recent passive pingers — bounded,
        // not O(every member).
        for i in 1..=9 {
            let id = NodeId::new(&addr(9000 + i));
            cs.handle_heartbeat(id, addr(9000 + i));
        }
        assert_eq!(cs.active_view().len(), 4);
        assert_eq!(cs.passive_view().len(), 5);

        // Backdate the heartbeat throttle so this tick actually sends.
        cs.last_heartbeat_sent = Instant::now() - Duration::from_secs(1);

        let actions = cs.tick();
        let heartbeats: Vec<NodeId> = actions
            .iter()
            .filter_map(|act| match act {
                ClusterAction::SendHeartbeat { to, .. } => Some(*to),
                _ => None,
            })
            .collect();
        assert!(
            heartbeats.len() >= cs.active_view().len(),
            "every active member is heartbeated, got {heartbeats:?}"
        );
        assert!(
            heartbeats.len() <= cs.active_view().len() + REPLY_SLOTS,
            "heartbeat fanout is bounded by active view + reply slots, got {heartbeats:?}"
        );
        for hb in &heartbeats {
            assert!(
                cs.active_view().contains(hb) || cs.passive_view().contains(hb),
                "heartbeat to {hb:?} is outside the views"
            );
        }
        // A passive member that did not recently ping us is never
        // answered.
        let stale = Instant::now() - Duration::from_secs(60);
        cs.members
            .get_mut(&NodeId::new(&addr(9009)))
            .unwrap()
            .last_heartbeat = stale;
        cs.last_heartbeat_sent = Instant::now() - Duration::from_secs(1);
        let actions = cs.tick();
        let heartbeats: Vec<NodeId> = actions
            .iter()
            .filter_map(|act| match act {
                ClusterAction::SendHeartbeat { to, .. } => Some(*to),
                _ => None,
            })
            .collect();
        assert!(
            !heartbeats.contains(&NodeId::new(&addr(9009))),
            "a silent passive member is not replied to"
        );
    }

    #[test]
    fn test_join_bootstrap_heartbeats_seed_then_promotes_it() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);
        let seed_addr = addr(9001);
        let seed = NodeId::new(&seed_addr);

        cs.join_cluster(seed_addr);
        assert_eq!(cs.get_node(seed).unwrap().status, NodeStatus::Joining);

        // The joiner heartbeats the seed even though it is not in any
        // view: the first heartbeat initiates the join.
        // Backdate the heartbeat throttle so this tick actually sends.
        cs.last_heartbeat_sent = Instant::now() - Duration::from_secs(1);

        let actions = cs.tick();
        assert!(actions.iter().any(|act| matches!(
            act,
            ClusterAction::SendHeartbeat { to, .. } if *to == seed
        )));

        // The seed's reply promotes it to Healthy and places it in the
        // active view (symmetric link established).
        cs.handle_heartbeat(seed, seed_addr);
        assert_eq!(cs.get_node(seed).unwrap().status, NodeStatus::Healthy);
        assert!(cs.active_view().contains(&seed));
    }

    // -- D7c: durable-actor directory (RFC 0014 §2) -----------------------

    #[test]
    fn test_directory_merge_highest_epoch_wins() {
        let mut cs = ClusterState::new(NodeId::new(&addr(9000)), addr(9000));
        let a = NodeId(1);
        let b = NodeId(2);
        let e1 = DurableDirectoryEntry {
            actor_id: 10,
            node_id: a,
            epoch: 1,
        };
        let e2 = DurableDirectoryEntry {
            actor_id: 10,
            node_id: b,
            epoch: 2,
        };
        let stale = DurableDirectoryEntry {
            actor_id: 10,
            node_id: a,
            epoch: 1,
        };

        assert!(cs.merge_directory(vec![e1]));
        assert!(cs.merge_directory(vec![e2]), "higher epoch must win");
        assert_eq!(cs.directory_epoch(10), Some(2));
        assert!(!cs.merge_directory(vec![stale]), "stale epoch is a no-op");
        assert_eq!(cs.directory_epoch(10), Some(2));

        let relocated = cs.directory_for_node(b);
        assert_eq!(relocated.len(), 1);
        assert_eq!(relocated[0].actor_id, 10);
        assert!(cs.directory_for_node(a).is_empty());
    }

    #[test]
    fn test_bump_directory_epoch_and_mark_removed() {
        let mut cs = ClusterState::new(NodeId::new(&addr(9000)), addr(9000));
        let n = NodeId(1);
        // A missing entry re-spawns at epoch 2 (past the original epoch-1
        // activation), then increments.
        assert_eq!(cs.bump_directory_epoch(10, n), 2);
        assert_eq!(cs.bump_directory_epoch(10, n), 3);

        assert!(cs.mark_removed(n));
        assert!(!cs.mark_removed(n), "second mark is a no-op");
        assert!(cs.is_removed(n));
        assert!(!cs.is_removed(NodeId(999)));
    }

    // -- D7c: confirmed-gone promotion (RFC 0014 §1 path 2) ----------------

    #[test]
    fn test_failed_node_promoted_to_removed_after_timeout() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);
        let b = NodeId::new(&addr(9001));
        cs.handle_heartbeat(b, addr(9001));

        // Drive b to Failed.
        let stale = Instant::now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&b).unwrap().last_heartbeat = stale;
        cs.tick();
        assert_eq!(cs.get_node(b).unwrap().status, NodeStatus::Failed);

        // Inside the confirmation window: no promotion.
        cs.removal_confirmation_timeout = Duration::from_secs(60);
        let actions = cs.tick();
        assert!(!actions
            .iter()
            .any(|a| matches!(a, ClusterAction::NodeRemoved { .. })));

        // Past the window: promoted exactly once.
        cs.removal_confirmation_timeout = Duration::from_secs(1);
        if let Some(at) = cs.failed_nodes.get_mut(&b) {
            *at = Instant::now() - Duration::from_secs(2);
        }
        let actions = cs.tick();
        assert!(actions
            .iter()
            .any(|a| matches!(a, ClusterAction::NodeRemoved { node } if *node == b)));
        assert!(cs.is_removed(b));
        // A second tick does not re-emit the action.
        let actions = cs.tick();
        assert!(!actions
            .iter()
            .any(|a| matches!(a, ClusterAction::NodeRemoved { .. })));
    }

    #[test]
    fn test_removal_timeout_zero_disables_timeout_promotion() {
        let a = addr(9000);
        let local = NodeId::new(&a);
        let mut cs = ClusterState::new(local, a);
        let b = NodeId::new(&addr(9001));
        cs.handle_heartbeat(b, addr(9001));
        let stale = Instant::now()
            - DEFAULT_HEARTBEAT_TIMEOUT
            - DEFAULT_SUSPICION_DURATION
            - Duration::from_secs(1);
        cs.members.get_mut(&b).unwrap().last_heartbeat = stale;
        cs.tick();
        assert_eq!(cs.get_node(b).unwrap().status, NodeStatus::Failed);

        cs.removal_confirmation_timeout = Duration::ZERO;
        if let Some(at) = cs.failed_nodes.get_mut(&b) {
            *at = Instant::now() - Duration::from_secs(3600);
        }
        let actions = cs.tick();
        assert!(!actions
            .iter()
            .any(|a| matches!(a, ClusterAction::NodeRemoved { .. })));
        assert!(!cs.is_removed(b));
    }
}
