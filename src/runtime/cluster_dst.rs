//! Deterministic multi-node cluster harness (PLAN.md Phase 1 bullet 2:
//! cluster/network determinism).
//!
//! [`DeterministicCluster`] drives N REAL [`Runtime`] instances over the
//! in-memory [`DeterministicNetworkTransport`] — no threads, no sleeps, no
//! wall-clock reads that affect state — with per-node virtual clocks
//! advanced in lockstep and ONE seeded RNG governing node execution order
//! and per-node actor selection, so the same seed reproduces the same run
//! while different seeds explore different interleavings.
//!
//! This is the vehicle for the 10³-seeds-per-commit cluster invariant
//! sweep the real-TCP chaos tests (`tests.rs`) cannot scale to: each round
//! is pure compute in virtual time (microseconds), so hundreds of seeds ×
//! hundreds of rounds complete in seconds.
//!
//! Fidelity notes (mirroring `cluster_sim.rs`):
//! - The transport is zero-latency FIFO per link (TCP-like), so within a
//!   link ordering is preserved; cross-link ordering is seeded via the
//!   per-round node execution order.
//! - Heartbeats, gossip, probes, and actor messages all cross the same
//!   fabric; `set_partition` drops outbound packets exactly like a
//!   firewall (the production failure detector then reacts in virtual
//!   time).
//! - The cluster tick cadence runs on the virtual clock
//!   (`ClusterState::set_clock`), and every node's `ClusterState` carries
//!   a seeded RNG (`set_rng`) so gossip/repair picks are bit-reproducible.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::dst::DeterministicRng;
use crate::runtime::network::DeterministicNetworkTransport;
use crate::runtime::{ActorAddress, NodeId, Runtime};

/// Wall-clock step per simulated round (the real runtime ticks cluster
/// maintenance roughly every 100 ms; heartbeats fire on the virtual clock
/// at the same cadence).
const ROUND_STEP: Duration = Duration::from_millis(100);

/// Scheduler step budget per node per round. A node runs until its local
/// actors Quiesce or this budget is exhausted; the budget makes a runaway
/// behavior fail as `StepLimitExceeded` instead of hanging the harness.
const STEPS_BUDGET: u64 = 100_000;

/// Deterministic multi-node cluster harness.
pub(crate) struct DeterministicCluster {
    /// The real runtimes, one per simulated node (index-aligned with
    /// `addrs`).
    pub nodes: Vec<Runtime>,
    /// Node addresses; each node's id is derived from its address.
    pub addrs: Vec<SocketAddr>,
    /// Master seeded RNG: drives per-round node order and hands each
    /// node's scheduler its selections from one shared stream.
    rng: DeterministicRng,
    /// Outbound partition sets, index-aligned with `nodes`. The transport
    /// contract replaces the whole set on `set_partition`, so the harness
    /// owns the sets and re-applies them (multiple `partition` calls
    /// accumulate; `heal` clears one node's set).
    partitions: Vec<std::collections::HashSet<NodeId>>,
    /// Hard-crashed nodes: skipped by the pump, with every peer's link to
    /// them dropped (dead socket). `restart_node` replaces the Runtime.
    crashed: Vec<bool>,
    /// Bounded-adjacent-reorder mode on every link (see
    /// [`DeterministicCluster::set_reorder_all`]).
    reorder: bool,
    /// Shared in-memory packet bus (kept so `restart_node` can create a
    /// fresh transport on the same bus).
    bus: Arc<
        parking_lot::Mutex<
            HashMap<
                NodeId,
                (
                    std::sync::mpsc::SyncSender<super::network::IncomingPacket>,
                    std::sync::mpsc::SyncSender<super::network::OutgoingPacket>,
                ),
            >,
        >,
    >,
    /// Rounds executed.
    pub round: u64,
    /// How many times a node hit the per-round step budget
    /// (`STEPS_BUDGET`, i.e. `StepLimitExceeded`) — a livelock signal.
    pub limit_hits: u64,
}

impl DeterministicCluster {
    /// Create `addrs.len()` real `Runtime`s, each with its own virtual
    /// clock and an in-memory `DeterministicNetworkTransport` registered
    /// on a shared bus, joined into a full mesh (every node seeds every
    /// other). `seed` seeds the master RNG; each node's `ClusterState`
    /// gets its own derived seeded RNG.
    pub fn new(addrs: &[SocketAddr], seed: u64) -> Self {
        let bus = Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let mut rng = DeterministicRng::new(seed);
        let mut nodes = Vec::with_capacity(addrs.len());
        for &addr in addrs {
            let mut rt = Runtime::new();
            // Clock BEFORE distribution is enabled so `enable_distribution`
            // clones it into the ClusterState (all cluster time queries —
            // heartbeat cadence, suspicion, probes — then run virtual).
            rt.install_virtual_clock();
            let transport = DeterministicNetworkTransport::bind_with_bus(addr, bus.clone())
                .expect("dst transport binds");
            // Register on the shared bus while still concrete (the trait
            // object hides `register_on_bus`).
            transport.register_on_bus();
            rt.enable_distribution_with_transport(Box::new(transport))
                .expect("dst distribution enables");
            if let Some(cluster) = rt.distributed.cluster.as_mut() {
                cluster.set_rng(Box::new(DeterministicRng::new(rng.next())));
            }
            nodes.push(rt);
        }
        // Join the mesh: every node seeds every other.
        for rt in nodes.iter_mut() {
            for &peer in addrs {
                rt.join_cluster(peer);
            }
        }
        DeterministicCluster {
            nodes,
            addrs: addrs.to_vec(),
            rng,
            partitions: vec![std::collections::HashSet::new(); addrs.len()],
            crashed: vec![false; addrs.len()],
            reorder: false,
            bus,
            round: 0,
            limit_hits: 0,
        }
    }

    /// The node id of the node at `index`.
    pub fn id(&self, index: usize) -> NodeId {
        NodeId::new(&self.addrs[index])
    }

    /// Immutable access to the runtime at `index`.
    pub fn node(&self, index: usize) -> &Runtime {
        &self.nodes[index]
    }

    /// Mutable access to the runtime at `index`.
    pub fn node_mut(&mut self, index: usize) -> &mut Runtime {
        &mut self.nodes[index]
    }

    /// Cut `from`'s outbound link to `to` (a firewall-style partition;
    /// every packet from `from` to `to` is silently dropped). Accumulates:
    /// multiple `partition` calls on the same node stay active together.
    pub fn partition(&mut self, from: usize, to: usize) {
        let pid = self.id(to);
        self.partitions[from].insert(pid);
        self.apply_partitions();
    }

    /// Restore every outbound link of the node at `index`.
    pub fn heal(&mut self, index: usize) {
        self.partitions[index].clear();
        self.apply_partitions();
    }

    /// Hard-crash the node at `index`: it is removed from the pump and
    /// every peer's outbound link to it is dropped (a dead socket). The
    /// survivors' real failure detector marks it `Failed` in virtual
    /// time. The crashed Runtime is replaced wholesale by
    /// [`DeterministicCluster::restart_node`] — a restart is a fresh
    /// node, exactly like the real-TCP crash/rejoin test.
    pub fn crash_node(&mut self, index: usize) {
        assert!(!self.crashed[index], "node {index} already crashed");
        self.crashed[index] = true;
        let pid = self.id(index);
        for (i, peers) in self.partitions.iter_mut().enumerate() {
            if i != index {
                peers.insert(pid);
            }
        }
        self.apply_partitions();
    }

    /// Restart the node at `index`: replace its Runtime with a fresh one
    /// (same address identity — same node id — new virtual clock,
    /// transport registered on the same bus) and rejoin the mesh through
    /// the first non-crashed peer. The old transport's channel drops, so
    /// in-flight packets to the dead node vanish like a closed socket.
    pub fn restart_node(&mut self, index: usize) {
        assert!(self.crashed[index], "node {index} is not crashed");
        let addr = self.addrs[index];
        let bus = self.bus.clone();
        let mut rt = Runtime::new();
        rt.install_virtual_clock();
        let transport =
            DeterministicNetworkTransport::bind_with_bus(addr, bus).expect("dst transport binds");
        transport.register_on_bus();
        rt.enable_distribution_with_transport(Box::new(transport))
            .expect("dst distribution enables");
        if let Some(cluster) = rt.distributed.cluster.as_mut() {
            cluster.set_rng(Box::new(DeterministicRng::new(self.rng.next())));
        }
        // Rejoin through the first non-crashed peer (a real restart joins
        // through a seed).
        if let Some(seed) = self
            .addrs
            .iter()
            .position(|a| {
                *a != addr && !self.crashed[self.addrs.iter().position(|x| x == a).unwrap()]
            })
            .map(|i| self.addrs[i])
        {
            rt.join_cluster(seed);
        }
        self.nodes[index] = rt;
        self.crashed[index] = false;
        self.partitions[index].clear();
        let pid = self.id(index);
        for peers in self.partitions.iter_mut() {
            peers.remove(&pid);
        }
        self.apply_partitions();
    }

    /// Push the harness-owned partition sets into the transports
    /// (`set_partition` replaces, so the full set is always re-applied).
    fn apply_partitions(&mut self) {
        for (i, peers) in self.partitions.iter().enumerate() {
            let transport = self.nodes[i]
                .distributed
                .transport
                .as_mut()
                .expect("transport");
            transport.set_partition(peers.clone());
        }
    }

    /// Enable bounded adjacent reordering on every node's transport link.
    /// Deterministic fault injection: consecutive packets to a peer are
    /// delivered swapped (P2 before P1) — nothing is lost or duplicated,
    /// only delayed one slot. The cluster protocol (heartbeats, gossip,
    /// acks, actor messages, CRDT sync) must form and stay correct under
    /// reordered delivery; the seed still permutes node order on top.
    pub fn set_reorder_all(&mut self, enabled: bool) {
        self.reorder = enabled;
        for rt in self.nodes.iter_mut() {
            if let Some(transport) = rt.distributed.transport.as_mut() {
                transport.set_reorder(enabled);
            }
        }
    }

    /// Run one round: advance every node's virtual clock by `ROUND_STEP`,
    /// then execute the nodes in a seed-permuted order — each node first
    /// drains its transport (packet delivery + cluster tick) and then runs
    /// its deterministic scheduler until its local actors Quiesce or the
    /// step budget is exhausted.
    pub fn step_round(&mut self) {
        let n = self.nodes.len();
        // Seeded Fisher-Yates over node indices: which node runs first in
        // this round is part of what the seed permutes.
        let mut order: Vec<usize> = (0..n).collect();
        for i in 0..n {
            let j = (self.rng.next() as usize) % (n - i);
            order.swap(i, i + j);
        }
        for idx in order {
            if self.crashed[idx] {
                // Hard-crashed: not pumped; peers' links to it are
                // dropped, so the failure detector handles it in virtual
                // time like a dead socket.
                continue;
            }
            let rt = &mut self.nodes[idx];
            rt.advance_time(ROUND_STEP);
            rt.process_network();
            // CRDT delta/full-state sync to healthy members, driven on the
            // cluster cadence exactly as a Rust embedder drives it
            // (`Runtime::sync_crdts` is deliberately NOT auto-called by
            // the production loop — SPEC2 §12.5 documents it as an
            // embedder API; the harness models the embedder).
            rt.sync_crdts();
            let result = rt.run_scheduler_deterministic_with_rng(&mut self.rng, STEPS_BUDGET);
            if matches!(
                result,
                crate::runtime::DeterministicRunResult::StepLimitExceeded { .. }
            ) {
                self.limit_hits += 1;
            }
            // Deliver any packets still held by the reorder buffer (odd
            // tails are never stranded; no-op when reorder is off).
            if self.reorder {
                if let Some(transport) = rt.distributed.transport.as_mut() {
                    transport.flush_held();
                }
            }
        }
        self.round += 1;
    }

    /// Run `rounds` rounds.
    pub fn run_rounds(&mut self, rounds: u64) {
        for _ in 0..rounds {
            self.step_round();
        }
    }

    /// The status of `node` in node `viewer`'s cluster view.
    pub fn cluster_status(
        &self,
        viewer: usize,
        node: NodeId,
    ) -> Option<crate::runtime::NodeStatus> {
        self.nodes[viewer]
            .distributed
            .cluster
            .as_ref()
            .and_then(|c| c.get_node(node))
            .map(|info| info.status)
    }

    /// True once every node has every OTHER node in its ACTIVE view (the
    /// failure detector watches only the active view, which fills through
    /// the repair cycle + reciprocal heartbeat confirmation).
    pub fn active_views_converged(&self) -> bool {
        let ids: Vec<NodeId> = self
            .nodes
            .iter()
            .map(|rt| rt.distributed.node_id.unwrap())
            .collect();
        self.nodes.iter().all(|rt| {
            let c = rt.distributed.cluster.as_ref().expect("cluster");
            let active: Vec<NodeId> = c.active_view().to_vec();
            let local = rt.distributed.node_id.unwrap();
            ids.iter().all(|id| *id == local || active.contains(id))
        })
    }

    /// Send a message from node `from` to a remote actor on node `to`'s
    /// runtime (location-transparent addressing over the in-memory fabric).
    pub fn send_remote(
        &mut self,
        from: usize,
        to: usize,
        actor_id: u64,
        behavior: &str,
        args: &[crate::vm::Value],
    ) {
        let target = ActorAddress::remote(self.id(to), actor_id);
        self.nodes[from].send_distributed(target, behavior, args);
    }

    /// A compact digest of the cluster's observable state — every node's
    /// view of every peer's status — for same-seed reproducibility checks
    /// (the scheduler interleaving itself is not directly observable, but
    /// a different interleaving that changed membership/gossip timing
    /// shows up here).
    pub fn digest(&self) -> String {
        let mut out = String::new();
        for (i, rt) in self.nodes.iter().enumerate() {
            let local = rt.distributed.node_id.unwrap();
            out.push_str(&format!("n{i}[{:x}]:", local.0));
            if let Some(c) = &rt.distributed.cluster {
                for peer in &self.addrs {
                    let pid = NodeId::new(peer);
                    if pid == local {
                        continue;
                    }
                    let status = c
                        .get_node(pid)
                        .map(|info| info.status)
                        .unwrap_or(crate::runtime::NodeStatus::Joining);
                    out.push_str(&format!("{:x}:{:?};", pid.0, status));
                }
            }
            out.push('|');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::NodeStatus;
    use crate::vm::Value;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    }

    /// Spawn a counter actor with an `inc` behavior that adds its arg to a
    /// `count` state field, and return its id.
    fn spawn_counter(rt: &mut Runtime) -> u64 {
        let id = rt.spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
        {
            let actor = rt.actors.get_mut(&id).unwrap();
            actor.register_behavior("inc", |actor, args| {
                let n = actor
                    .get_state_field("count")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                let by = args.get(0).and_then(|v| v.as_int()).unwrap_or(0);
                actor.set_state_field("count", Value::int(n + by));
            });
        }
        id
    }

    fn counter_value(cluster: &DeterministicCluster, node: usize, id: u64) -> i64 {
        cluster
            .node(node)
            .actors
            .get(&id)
            .and_then(|a| a.get_state_field("count"))
            .and_then(|v| v.as_int())
            .unwrap_or(-1)
    }

    /// PLAN.md Phase 1 bullet 2 (DST): cluster/network determinism — the
    /// in-memory-fabric harness is bit-reproducible per seed. Two runs with
    /// the same seed must produce identical per-round digests (node order,
    /// gossip/repair picks, actor selection are all seed-driven).
    #[test]
    fn test_dst_cluster_same_seed_reproducible() {
        const ROUNDS: u64 = 40;
        let run = |seed: u64| -> (Vec<(String, i64)>, i64) {
            let mut cluster = DeterministicCluster::new(&[addr(9101), addr(9102)], seed);
            // Converge membership first: the resolver refuses to route to a
            // node that is not yet Healthy, so early sends would be dropped.
            cluster.run_rounds(20);
            let counter = spawn_counter(&mut cluster.node_mut(0));
            // Burst of remote messages from node 1 to the counter on node 0.
            for _ in 0..20 {
                cluster.send_remote(1, 0, counter, "inc", &[Value::int(1)]);
            }
            let mut trace = Vec::new();
            for _ in 0..ROUNDS {
                cluster.step_round();
                trace.push((cluster.digest(), counter_value(&cluster, 0, counter)));
            }
            (trace, counter_value(&cluster, 0, counter))
        };
        let (trace_a, count_a) = run(42);
        let (trace_b, count_b) = run(42);
        assert_eq!(
            trace_a, trace_b,
            "same seed must produce the same cluster evolution (digest + counter per round)"
        );
        assert_eq!(count_a, count_b, "same seed must produce the same count");
        assert_eq!(count_a, 20, "all 20 remote messages delivered");
    }

    /// PLAN.md Phase 1 bullet 2 (DST): the cluster seed-sweep invariant
    /// test. N seeds × a burst of M remote messages across the in-memory
    /// fabric; for EVERY seed:
    ///  1. The cluster converges to a full-Healthy membership (the fabric
    ///     plus virtual-clock gossip actually forms a cluster).
    ///  2. No node hits the step budget (no deadlock/livelock).
    ///  3. AtMostOnce delivery: the counter reaches exactly M.
    #[test]
    fn test_dst_cluster_remote_delivery_seed_sweep() {
        const MESSAGES: i64 = 30;
        let seeds = crate::dst::dst_seed_count(50);
        const ROUNDS: u64 = 40;

        for seed in 0..seeds {
            let mut cluster = DeterministicCluster::new(&[addr(9111), addr(9112)], seed);
            // Converge membership before sending (resolver refuses to route
            // to a non-Healthy node).
            cluster.run_rounds(20);
            let counter = spawn_counter(&mut cluster.node_mut(0));
            for _ in 0..MESSAGES {
                cluster.send_remote(1, 0, counter, "inc", &[Value::int(1)]);
            }
            cluster.run_rounds(ROUNDS);

            assert_eq!(
                cluster.limit_hits, 0,
                "seed {seed}: step budget exceeded — possible deadlock/livelock"
            );
            // Membership converged: each node sees the other Healthy.
            let id1 = cluster.id(1);
            let id0 = cluster.id(0);
            assert_eq!(
                cluster.cluster_status(0, id1),
                Some(NodeStatus::Healthy),
                "seed {seed}: node 0 must see node 1 healthy"
            );
            assert_eq!(
                cluster.cluster_status(1, id0),
                Some(NodeStatus::Healthy),
                "seed {seed}: node 1 must see node 0 healthy"
            );
            let count = counter_value(&cluster, 0, counter);
            assert_eq!(
                count, MESSAGES,
                "seed {seed}: counter must reach exactly {MESSAGES} (AtMostOnce), got {count}"
            );
        }
    }

    /// PLAN.md Phase 1 bullet 2 (DST): partition + failure detection +
    /// self-healing over the deterministic fabric, end to end through the
    /// REAL runtime. A 3-node cluster forms, C is partitioned away from
    /// {A, B} (firewall-style drop both directions), both sides detect the
    /// other as `Failed` through the REAL virtual-clock failure detector,
    /// the partition heals, all three reconverge to `Healthy` via the
    /// probe path, and a remote message then delivers across the former
    /// partition boundary.
    #[test]
    fn test_dst_cluster_partition_detects_heals_and_delivers() {
        let mut cluster = DeterministicCluster::new(&[addr(9121), addr(9122), addr(9123)], 7);
        let a = cluster.id(0);
        let b = cluster.id(1);
        let c = cluster.id(2);
        let counter = spawn_counter(&mut cluster.node_mut(2)); // actor on C

        // Phase 1: converge. Active views fill only through the repair
        // cycle (~5 s virtual = 50 rounds), so run plenty of rounds.
        cluster.run_rounds(80);
        assert!(
            cluster.active_views_converged(),
            "cluster must converge before the partition (round {})",
            cluster.round
        );
        for (viewer, peer) in [(0, b), (0, c), (1, a), (1, c), (2, a), (2, b)] {
            assert_eq!(
                cluster.cluster_status(viewer, peer),
                Some(NodeStatus::Healthy),
                "node {viewer} must see node {} healthy before partition",
                peer.0
            );
        }

        // Phase 2: partition C away from {A, B}.
        cluster.partition(0, 2); // A -> C dropped
        cluster.partition(2, 0); // C -> A dropped
        cluster.partition(1, 2); // B -> C dropped
        cluster.partition(2, 1); // C -> B dropped
                                 // Failure needs 2 s (Suspicious) + 5 s (Failed) of silence = 70
                                 // rounds; the last heartbeat could have landed up to 500 ms into
                                 // the partition, so 120 rounds gives comfortable headroom.
        cluster.run_rounds(120);
        assert_eq!(
            cluster.cluster_status(0, c),
            Some(NodeStatus::Failed),
            "A must mark C failed"
        );
        assert_eq!(
            cluster.cluster_status(1, c),
            Some(NodeStatus::Failed),
            "B must mark C failed"
        );
        assert_eq!(
            cluster.cluster_status(2, a),
            Some(NodeStatus::Failed),
            "C must mark A failed"
        );
        assert_eq!(
            cluster.cluster_status(2, b),
            Some(NodeStatus::Failed),
            "C must mark B failed"
        );
        // The majority sub-cluster stays internally healthy.
        assert_eq!(
            cluster.cluster_status(0, b),
            Some(NodeStatus::Healthy),
            "A and B stay healthy through the partition"
        );

        // Phase 3: heal the partition. Probes re-promote via the
        // heartbeat-reply path (probe interval 5 s = 50 rounds); gossip
        // reconverges membership.
        cluster.heal(0);
        cluster.heal(1);
        cluster.heal(2);
        cluster.run_rounds(120);
        for (viewer, peer) in [(0, b), (0, c), (1, a), (1, c), (2, a), (2, b)] {
            assert_eq!(
                cluster.cluster_status(viewer, peer),
                Some(NodeStatus::Healthy),
                "node {viewer} must reconverge to healthy with node {} after healing",
                peer.0
            );
        }

        // Phase 4: real work across the former boundary — A sends to the
        // actor on C.
        cluster.send_remote(0, 2, counter, "inc", &[Value::int(99)]);
        cluster.run_rounds(20);
        let count = counter_value(&cluster, 2, counter);
        assert_eq!(
            count, 99,
            "remote message must deliver across the healed boundary"
        );
    }

    /// PLAN.md Phase 1 bullet 2 (DST): cross-shard determinism. In sharded
    /// mode (`new_sharded`), messages route through the cross-shard
    /// channels; `run_scheduler_deterministic` must drain them (the
    /// production scheduler does) so sharded runs are deterministic too.
    /// Sweep seeds: every run delivers exactly `MESSAGES` increments to
    /// the actor on the owning shard and Quiesces.
    #[test]
    fn test_dst_cross_shard_delivery_seed_sweep() {
        const MESSAGES: i64 = 25;
        let seeds = crate::dst::dst_seed_count(30);

        for seed in 0..seeds {
            let mut shards = Runtime::new_sharded(2);
            assert_eq!(shards.len(), 2);
            // Actor ids come from a process-global counter, so spawn on
            // shard 1 until the fresh id's parity routes there too
            // (`target % shard_count == 1`): a spawn lands on the calling
            // shard, and cross-shard sends route by `id % shard_count`, so
            // the actor must sit on its routing shard or messages go to
            // the wrong shard's DLQ. Parity alternates per spawn, so the
            // loop terminates within 2 iterations barring interference
            // from parallel tests.
            let mut target =
                shards[1].spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
            while target % 2 != 1 {
                target =
                    shards[1].spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
            }
            {
                let actor = shards[1].actors.get_mut(&target).unwrap();
                actor.register_behavior("inc", |actor, args| {
                    let n = actor
                        .get_state_field("count")
                        .and_then(|v| v.as_int())
                        .unwrap_or(0);
                    let by = args.get(0).and_then(|v| v.as_int()).unwrap_or(0);
                    actor.set_state_field("count", Value::int(n + by));
                });
            }
            // Shard 0 sends to the actor on shard 1: cross-shard routing.
            for _ in 0..MESSAGES {
                shards[0].send_message(target, "inc", &[Value::int(1)]);
            }
            // Drive both shards deterministically, interleaving from one
            // seeded stream.
            let mut rng = DeterministicRng::new(seed);
            let mut steps = 0u64;
            loop {
                if steps >= 100_000 {
                    panic!("seed {seed}: step limit exceeded — possible deadlock");
                }
                let quiescent = shards.iter_mut().all(|shard| {
                    matches!(
                        shard.run_scheduler_deterministic_with_rng(&mut rng, 100),
                        crate::runtime::DeterministicRunResult::Quiescent { .. }
                    )
                });
                steps += 1;
                if quiescent {
                    break;
                }
            }
            let count = shards[1]
                .actors
                .get(&target)
                .and_then(|a| a.get_state_field("count"))
                .and_then(|v| v.as_int())
                .unwrap_or(-1);
            assert_eq!(
                count, MESSAGES,
                "seed {seed}: cross-shard counter must reach exactly {MESSAGES}, got {count}"
            );
        }
    }

    /// PLAN.md Phase 1 bullet 2 (DST): CRDT-sync-race scenario, seed-driven.
    /// Two real nodes over the in-memory fabric with per-round
    /// `sync_crdts` driven by the harness (CRDT replication is a
    /// Rust-embedder API per SPEC2 §12.5 — the harness calls it the way
    /// an embedder would, on the cluster cadence). A GCounter is
    /// created on node A; the round-1 full-state sync must create the
    /// replica on node B. Both nodes then increment their LOCAL replicas
    /// repeatedly, interleaved with sync rounds — which node's increment
    /// ships first is part of what the seed permutes. Invariants for every
    /// seed:
    ///  1. Full-state sync actually creates the entry on the receiver
    ///     (node B's manager has the counter).
    ///  2. Both replicas converge to the same value.
    ///  3. The converged value is the SUM of every increment on both nodes
    ///     (GCounter is commutative — no lost update under any
    ///     interleaving).
    #[test]
    fn test_dst_cluster_crdt_convergence_seed_sweep() {
        const SEEDS: u64 = 40;
        const ROUNDS: u64 = 60;
        const A_INCS: u64 = 3;
        const B_INCS: u64 = 4;

        for seed in 0..SEEDS {
            let mut cluster = DeterministicCluster::new(&[addr(9131), addr(9132)], seed);
            cluster.run_rounds(20); // converge membership before CRDT sync

            let counter_id = {
                let a = &mut cluster.nodes[0];
                a.crdt_manager.as_mut().unwrap().create_gcounter().0
            };
            let counter_value = |rt: &mut Runtime| -> Option<u64> {
                rt.crdt_manager
                    .as_mut()
                    .and_then(|m| m.get_gcounter_mut(counter_id))
                    .map(|c| c.value())
            };

            // Interleave local increments from both sides with sync
            // rounds: the seeded node order decides which side's deltas
            // reach the other first.
            let mut a_inc = 0u64;
            let mut b_inc = 0u64;
            let mut b_created = false;
            for _ in 0..ROUNDS {
                cluster.step_round();
                if !b_created && counter_value(&mut cluster.nodes[1]).is_some() {
                    b_created = true; // full-state sync created B's replica
                }
                if a_inc < A_INCS {
                    cluster.nodes[0]
                        .crdt_manager
                        .as_mut()
                        .unwrap()
                        .get_gcounter_mut(counter_id)
                        .unwrap()
                        .increment();
                    a_inc += 1;
                }
                if b_created && b_inc < B_INCS {
                    cluster.nodes[1]
                        .crdt_manager
                        .as_mut()
                        .unwrap()
                        .get_gcounter_mut(counter_id)
                        .unwrap()
                        .increment();
                    b_inc += 1;
                }
            }

            assert!(
                b_created,
                "seed {seed}: full-state CRDT sync must create the replica on node B"
            );
            let va = counter_value(&mut cluster.nodes[0]).unwrap_or(0);
            let vb = counter_value(&mut cluster.nodes[1]).unwrap_or(0);
            let expected = A_INCS + B_INCS;
            assert_eq!(
                va, expected,
                "seed {seed}: node A replica must converge to the sum of all increments"
            );
            assert_eq!(
                vb, expected,
                "seed {seed}: node B replica must converge to the sum of all increments"
            );
            assert_eq!(
                va, vb,
                "seed {seed}: replicas must converge to the same value"
            );
            assert_eq!(
                cluster.limit_hits, 0,
                "seed {seed}: step budget exceeded — possible deadlock"
            );
        }
    }

    /// PLAN.md Phase 1 bullet 2 (DST): node-crash scenario, seed-driven —
    /// the sleep-free, seed-sweepable counterpart of the real-TCP
    /// `test_three_node_cluster_survives_hard_node_failure_and_rejoin`.
    /// 3 nodes converge; node 2 is hard-crashed (dropped from the pump,
    /// every link to it cut); the survivors mark it `Failed` through the
    /// REAL virtual-clock failure detector; the node restarts as a FRESH
    /// Runtime (same node id — new state) joining through a survivor;
    /// the cluster reconverges to full `Healthy`; a remote message then
    /// delivers to an actor on the restarted node. Seed sweep: the
    /// per-round node order (and the gossip/repair picks) vary, but the
    /// invariants hold for every seed.
    #[test]
    fn test_dst_cluster_crash_restart_seed_sweep() {
        let seeds = crate::dst::dst_seed_count(20);

        for seed in 0..seeds {
            let mut cluster =
                DeterministicCluster::new(&[addr(9141), addr(9142), addr(9143)], seed);
            let a = cluster.id(0);
            let b = cluster.id(1);
            let c = cluster.id(2);

            // Phase 1: converge (active views fill through the repair
            // cycle, ~50 rounds).
            cluster.run_rounds(80);
            assert!(
                cluster.active_views_converged(),
                "seed {seed}: cluster must converge before the crash"
            );

            // Phase 2: hard-crash C. Failure needs 2 s + 5 s = 70 rounds
            // of silence; 120 gives headroom.
            cluster.crash_node(2);
            cluster.run_rounds(120);
            assert_eq!(
                cluster.cluster_status(0, c),
                Some(NodeStatus::Failed),
                "seed {seed}: A must mark C failed"
            );
            assert_eq!(
                cluster.cluster_status(1, c),
                Some(NodeStatus::Failed),
                "seed {seed}: B must mark C failed"
            );
            assert_eq!(
                cluster.cluster_status(0, b),
                Some(NodeStatus::Healthy),
                "seed {seed}: A and B stay healthy through the crash"
            );

            // Phase 3: restart C as a fresh node joining through A.
            cluster.restart_node(2);
            cluster.run_rounds(120);
            for (viewer, peer) in [(0, b), (0, c), (1, a), (1, c), (2, a), (2, b)] {
                assert_eq!(
                    cluster.cluster_status(viewer, peer),
                    Some(NodeStatus::Healthy),
                    "seed {seed}: node {viewer} must reconverge with node {} after restart",
                    peer.0
                );
            }

            // Phase 4: real work across the restart — A sends to an actor
            // on the fresh C.
            let counter = spawn_counter(&mut cluster.node_mut(2));
            cluster.send_remote(0, 2, counter, "inc", &[Value::int(99)]);
            cluster.run_rounds(20);
            assert_eq!(
                counter_value(&cluster, 2, counter),
                99,
                "seed {seed}: remote message must deliver to the restarted node"
            );
        }
    }

    /// PLAN.md Phase 1 bullet 2 (DST): message-reorder scenario, seed-driven.
    /// Every link reorders packets with bounded adjacent swaps (P2 before
    /// P1; nothing lost or duplicated, only delayed one slot) from the very
    /// first packet. Invariants for every seed:
    ///  1. The cluster still forms — heartbeats, gossip merges, and acks
    ///     are order-independent, so membership converges to full
    ///     `Healthy` even when packet pairs arrive swapped.
    ///  2. AtMostOnce remote delivery still holds — the counter reaches
    ///     exactly `MESSAGES` (reorder never duplicates or loses a packet).
    ///  3. No node hits the step budget (no deadlock/livelock).
    ///  4. CRDT replication still converges — delta/full-state sync packets
    ///     merge order-independently, so both replicas reach the summed
    ///     total under reordered delivery.
    #[test]
    fn test_dst_cluster_message_reorder_seed_sweep() {
        const MESSAGES: i64 = 30;
        const ROUNDS: u64 = 60;
        let seeds = crate::dst::dst_seed_count(25);

        for seed in 0..seeds {
            let mut cluster =
                DeterministicCluster::new(&[addr(9151), addr(9152), addr(9153)], seed);
            // Reorder from the very first packet: the cluster must FORM
            // under out-of-order heartbeats/gossip/acks.
            cluster.set_reorder_all(true);

            // CRDT replica on node A, incremented on both nodes mid-run.
            let counter_id = {
                let rt = &mut cluster.node_mut(0);
                rt.crdt_manager
                    .as_mut()
                    .expect("crdt manager")
                    .create_gcounter()
                    .0
            };
            // Converge membership under reorder before sending (the
            // resolver refuses to route to a non-Healthy node).
            cluster.run_rounds(120);
            assert!(
                cluster.active_views_converged(),
                "seed {seed}: cluster must form under reordered delivery"
            );

            let counter = spawn_counter(&mut cluster.node_mut(0));
            for _ in 0..MESSAGES {
                cluster.send_remote(1, 0, counter, "inc", &[Value::int(1)]);
            }
            // Interleave local replica increments with the sync rounds;
            // which side's delta ships first is seed-permuted by the node
            // order on top of the reorder.
            for i in 0..20 {
                cluster
                    .node_mut(0)
                    .crdt_manager
                    .as_mut()
                    .unwrap()
                    .get_gcounter_mut(counter_id)
                    .unwrap()
                    .increment();
                cluster
                    .node_mut(1)
                    .crdt_manager
                    .as_mut()
                    .unwrap()
                    .get_gcounter_mut(counter_id)
                    .unwrap()
                    .increment();
                cluster.run_rounds(ROUNDS / 20);
                let _ = i;
            }
            cluster.run_rounds(ROUNDS);

            assert_eq!(
                cluster.limit_hits, 0,
                "seed {seed}: step budget exceeded — possible deadlock/livelock"
            );
            let id0 = cluster.id(0);
            let id1 = cluster.id(1);
            let id2 = cluster.id(2);
            for (viewer, peer) in [(0, id1), (0, id2), (1, id0), (1, id2), (2, id0), (2, id1)] {
                assert_eq!(
                    cluster.cluster_status(viewer, peer),
                    Some(NodeStatus::Healthy),
                    "seed {seed}: node {viewer} must see node {} healthy under reorder",
                    peer.0
                );
            }
            let count = counter_value(&cluster, 0, counter);
            assert_eq!(
                count, MESSAGES,
                "seed {seed}: counter must reach exactly {MESSAGES} under reorder (AtMostOnce), got {count}"
            );
            let va = cluster
                .node_mut(0)
                .crdt_manager
                .as_mut()
                .unwrap()
                .get_gcounter_mut(counter_id)
                .unwrap()
                .value();
            let vb = cluster
                .node_mut(1)
                .crdt_manager
                .as_mut()
                .unwrap()
                .get_gcounter_mut(counter_id)
                .unwrap()
                .value();
            assert_eq!(
                va, 40,
                "seed {seed}: both replicas must converge to the summed total (20 increments each side) under reorder, got {va}"
            );
            assert_eq!(
                vb, va,
                "seed {seed}: replicas must converge under reordered CRDT sync"
            );
        }
    }

    // -- D7c: durable-actor re-spawn on node failure (RFC 0014) -----------

    /// Compile `.nula` source through the full frontend into a bytecode
    /// module (mirrors the integration-test `compile_source`; the effect /
    /// capability passes are skipped — no linear values in these fixtures).
    fn compile_module(source: &str) -> crate::bytecode::CodeModule {
        let tokens = crate::lexer::Lexer::new(source).lex().unwrap();
        let ast = crate::parser::Parser::new(tokens).parse_module().unwrap();
        let mut tc = crate::typechecker::TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).unwrap();
        crate::mir_codegen::compile_mir(&mut mir, "test").unwrap()
    }

    /// A bytecode `persistent actor` with a `Durable` `count` field and an
    /// `inc(by)` behavior. Re-spawn requires a bytecode module (the shadow
    /// replica serializes it as NBC), so native-closure actors can't be used.
    const COUNTER_SOURCE: &str = r#"
        persistent actor Counter {
            state durable count: Int = 0
            behavior inc(by: Int) { self.count = self.count + by }
            behavior get() { self.count }
        }
    "#;

    /// Spawn a bytecode `Counter`, then opt it into `RespawnOnNodeLoss`
    /// under a supervisor. Returns the actor id.
    fn spawn_respawnable_counter(rt: &mut Runtime) -> u64 {
        use crate::runtime::supervisor::{ChildSpec, RestartPolicy, RestartStrategy};

        let module = compile_module(COUNTER_SOURCE);
        let idx = module.actor_metadata[0].behavior_indices[0];
        let id = rt
            .spawn_from_module(&module, idx, vec![])
            .as_actor_id()
            .expect("spawn returns an actor id");
        let sup = rt.create_supervisor("sup", RestartStrategy::OneForOne);
        rt.supervise_child(
            sup,
            ChildSpec::new("counter", RestartPolicy::RespawnOnNodeLoss),
            id,
        );
        id
    }

    /// D7c timeout path (RFC 0014 §1 path 2): a durable actor opted into
    /// `RespawnOnNodeLoss` on a hard-crashed node is re-spawned on its
    /// deterministic shadow from the last replicated snapshot — exactly one
    /// live copy, durable field equal to the last checkpoint.
    #[test]
    fn test_d7c_respawn_on_node_failure() {
        use crate::runtime::{ClusterConfig, SplitBrainConfig};
        use std::time::Duration;

        let mut cluster = DeterministicCluster::new(&[addr(9201), addr(9202), addr(9203)], 7);
        // A short confirmation window keeps the test from waiting the
        // default 60 s (failure detection itself is 2 s + 5 s).
        let config = ClusterConfig {
            split_brain: SplitBrainConfig::Disabled,
            probe_interval: Duration::from_secs(5),
            removal_confirmation_timeout: Duration::from_secs(2),
            directory_gossip: true,
        };
        for i in 0..3 {
            assert!(cluster.node_mut(i).set_cluster_config(config.clone()));
        }
        cluster.run_rounds(20); // converge membership

        let counter = spawn_respawnable_counter(&mut cluster.node_mut(0));
        // Two checkpoints within one activation: the shadow must retain the
        // LATEST replica (41, then 51), not the first.
        cluster
            .node_mut(0)
            .send_message(counter, "inc", &[Value::int(41)]);
        cluster.run_rounds(1);
        cluster.node_mut(0).checkpoint_actor(counter);
        cluster
            .node_mut(0)
            .send_message(counter, "inc", &[Value::int(10)]);
        cluster.run_rounds(1);
        cluster.node_mut(0).checkpoint_actor(counter);
        assert_eq!(counter_value(&cluster, 0, counter), 51);
        // Gossip the directory to the survivors and deliver the replica.
        cluster.run_rounds(20);

        cluster.crash_node(0);
        // Failure (2 s + 5 s) + confirmation window (2 s), with margin.
        cluster.run_rounds(120);

        let mut hosts = Vec::new();
        for i in 1..3 {
            if cluster.node(i).actors.contains_key(&counter) {
                hosts.push((i, counter_value(&cluster, i, counter)));
            }
        }
        assert_eq!(
            hosts.len(),
            1,
            "exactly one survivor must re-spawn the actor (no duplicate)"
        );
        assert_eq!(
            hosts[0].1, 51,
            "re-spawned actor must restore the LAST durable snapshot (not the first)"
        );
    }

    /// D7c goodbye path (RFC 0014 §1 path 1): a self-downing node must
    /// checkpoint AND terminate its re-spawn-opted durable actors before
    /// declaring them dead, or the shadow re-spawn would race a still-live
    /// local copy. `goodbye_self` is what makes the declaration true.
    #[test]
    fn test_d7c_goodbye_self_checkpoints_and_terminates() {
        use crate::runtime::{ClusterConfig, SplitBrainConfig};
        use std::time::Duration;

        let mut cluster = DeterministicCluster::new(&[addr(9301), addr(9302), addr(9303)], 11);
        let config = ClusterConfig {
            split_brain: SplitBrainConfig::Disabled,
            probe_interval: Duration::from_secs(5),
            removal_confirmation_timeout: Duration::from_secs(2),
            directory_gossip: true,
        };
        for i in 0..3 {
            assert!(cluster.node_mut(i).set_cluster_config(config.clone()));
        }
        cluster.run_rounds(20);

        let counter = spawn_respawnable_counter(&mut cluster.node_mut(0));
        cluster
            .node_mut(0)
            .send_message(counter, "inc", &[Value::int(7)]);
        cluster.run_rounds(1);
        cluster.node_mut(0).checkpoint_actor(counter);
        cluster.run_rounds(20);

        // The self-down path: checkpoint + terminate, then the goodbye.
        cluster.node_mut(0).goodbye_self();

        // The goodbye declaration is now true: the local copy is gone.
        assert!(
            !cluster.node(0).actors.contains_key(&counter),
            "goodbye_self must terminate the opted actor, not just list it"
        );
        // A survivor holds the replica (it received the shadow replicate).
        let replica_holders = (1..3)
            .filter(|&i| cluster.node(i).shadow_replicas.contains_key(&counter))
            .count();
        assert_eq!(
            replica_holders, 1,
            "exactly one survivor must hold the shadow replica"
        );
    }

    /// D7c two-live-copies resolution (§5 self-demote): a node whose local
    /// durable actor was superseded by a higher directory epoch must reap its
    /// stale copy, so only the re-spawned survivor keeps writing.
    #[test]
    fn test_d7c_self_demote_superseded_actor() {
        use crate::runtime::cluster::DurableDirectoryEntry;

        let mut cluster = DeterministicCluster::new(&[addr(9401), addr(9402)], 13);
        cluster.run_rounds(20);

        let counter = spawn_respawnable_counter(&mut cluster.node_mut(0));
        assert_eq!(cluster.node(0).respawn_opted.get(&counter), Some(&1));
        let other_node = cluster.id(1);

        // Simulate a re-joined node learning that a survivor re-spawned the
        // actor at a higher epoch (the directory merge in gossip).
        cluster
            .node_mut(0)
            .distributed
            .cluster
            .as_mut()
            .unwrap()
            .merge_directory(vec![DurableDirectoryEntry {
                actor_id: counter,
                node_id: other_node,
                epoch: 2,
            }]);

        cluster.node_mut(0).self_demote_superseded();

        assert!(
            !cluster.node(0).actors.contains_key(&counter),
            "self-demote must reap the superseded local copy"
        );
        assert!(
            !cluster.node(0).respawn_opted.contains_key(&counter),
            "self-demote must forget the superseded opt-in"
        );
        // The forwarding entry must point at the replacement node, not self.
        assert_eq!(
            cluster
                .node(0)
                .migrated_actors
                .get(&counter)
                .map(|(n, _)| *n),
            Some(other_node),
            "self-demote must forward sends to the directory's replacement node"
        );
    }
}
