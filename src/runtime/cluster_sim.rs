//! Deterministic cluster simulation (Phase 5 deliverable 2).
//!
//! [`SimCluster`] runs N real [`ClusterState`] machines — the production
//! membership / failure-detection / split-brain-resolver / probe state
//! machine — against a shared [`VirtualClock`] advanced in lockstep, with
//! a controllable directed message fabric that drops heartbeats, gossip,
//! and probes to model partitions.
//!
//! Unlike the real-TCP chaos tests (`src/runtime/tests.rs`), a scenario
//! here is fully deterministic: same scenario, same result, every run.
//! This is the verification vehicle for the split-brain resolver
//! (PLAN.md Phase 5 deliverable 1): clean partitions (mutually-invisible
//! healthy sub-clusters), asymmetric (one-way) partitions, and 5-node
//! topologies are asserted as invariants — minority side downs itself,
//! majority side survives, and a healed partition re-joins via the probe
//! path without an external rejoin.
//!
//! Fidelity notes:
//! - A probe is an ordinary heartbeat packet on the wire
//!   (`distribution.rs` `ClusterAction::Probe` handling), so the fabric
//!   delivers probes through `handle_heartbeat` on the target.
//! - A downed node (`ClusterState::is_down`) has had its transport shut
//!   down in the real runtime; the fabric drops all deliveries to it.
//! - `pick_gossip_targets` uses `OsRng` internally, so gossip target
//!   choice is not bit-reproducible; the asserted invariants (down /
//!   stay-up / rejoin convergence) do not depend on which targets gossip
//!   picks, only on heartbeat staleness, the resolver, and probes.
//!   (For bit-reproducible gossip, the full-runtime
//!   `cluster_dst::DeterministicCluster` seeds every node's `ClusterState`
//!   via `set_rng` — this ClusterState-only harness predates that.)

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;

use super::cluster::{ClusterAction, ClusterConfig, ClusterState, NodeId, NodeStatus};
use super::timer::VirtualClock;
use super::GOSSIP_PAYLOAD_MAX_ENTRIES;

/// Wall-clock step per simulated round (the real runtime ticks cluster
/// maintenance roughly every 100 ms).
const ROUND_STEP: Duration = Duration::from_millis(100);

/// A deterministic simulation of N cluster nodes.
pub struct SimCluster {
    /// The simulated nodes, indexed by position.
    pub nodes: Vec<ClusterState>,
    /// Node addresses, index-aligned with `nodes`; each node's id is
    /// derived from its address.
    pub addrs: Vec<SocketAddr>,
    /// Directed links that are cut: `(sender_index, receiver_index)`.
    cut: HashSet<(usize, usize)>,
    /// Per-node virtual clocks (one base, advanced in lockstep).
    clocks: Vec<VirtualClock>,
    /// Rounds executed.
    pub round: u64,
    /// Heartbeats sent per node in the most recent round (index-aligned
    /// with `nodes`); lets scenarios assert the partial-view fanout
    /// bound.
    pub last_round_heartbeats: Vec<usize>,
}

impl SimCluster {
    /// Create `addrs.len()` nodes, each deriving its [`NodeId`] from its
    /// address, sharing one virtual clock base, and configured with
    /// `config`. Every node explicitly joins every other, so the mesh is
    /// knowledge-complete from the start; the bounded reply rule (see
    /// `ClusterState::tick`) keeps the heartbeat data plane at
    /// O(active + probationary + replies) even though every node knows
    /// everyone.
    pub fn new(addrs: &[SocketAddr], config: &ClusterConfig) -> Self {
        assert!(config.is_valid(), "cluster config must be valid");
        let base = VirtualClock::new();
        let mut nodes = Vec::with_capacity(addrs.len());
        let mut clocks = Vec::with_capacity(addrs.len());
        for addr in addrs {
            let mut cs = ClusterState::new(NodeId::new(addr), *addr);
            let clock = base.clone();
            cs.set_clock(clock.clone());
            assert!(cs.apply_config(config), "config must apply");
            nodes.push(cs);
            clocks.push(clock);
        }
        for (i, cs) in nodes.iter_mut().enumerate() {
            for (j, addr) in addrs.iter().enumerate() {
                if i != j {
                    cs.join_cluster(*addr);
                }
            }
        }
        SimCluster {
            nodes,
            addrs: addrs.to_vec(),
            cut: HashSet::new(),
            clocks,
            round: 0,
            last_round_heartbeats: vec![0; addrs.len()],
        }
    }

    /// The node id of the node at `index`.
    pub fn id(&self, index: usize) -> NodeId {
        NodeId::new(&self.addrs[index])
    }

    /// The index of the node with the given id, if present.
    pub fn index_of(&self, node_id: NodeId) -> Option<usize> {
        self.addrs.iter().position(|a| NodeId::new(a) == node_id)
    }

    /// Immutable access to the node at `index`.
    pub fn node(&self, index: usize) -> &ClusterState {
        &self.nodes[index]
    }

    /// Cut the directed link `from -> to`: messages from `from` to `to`
    /// are dropped.
    pub fn cut_link(&mut self, from: usize, to: usize) {
        self.cut.insert((from, to));
    }

    /// Restore the directed link `from -> to`.
    pub fn heal_link(&mut self, from: usize, to: usize) {
        self.cut.remove(&(from, to));
    }

    /// True when the directed link `from -> to` is cut.
    pub fn is_cut(&self, from: usize, to: usize) -> bool {
        self.cut.contains(&(from, to))
    }

    /// Advance the shared virtual clock by `dur` (all nodes see the
    /// same time progression).
    pub fn advance(&mut self, dur: Duration) {
        for (i, clock) in self.clocks.iter_mut().enumerate() {
            clock.advance(dur);
            self.nodes[i].set_clock(clock.clone());
        }
    }

    /// Run one round: every node ticks in index order, then every
    /// emitted action is delivered through the fabric (subject to cuts
    /// and to the target being up).
    pub fn tick_round(&mut self) {
        let actions: Vec<(usize, Vec<ClusterAction>)> = (0..self.nodes.len())
            .map(|i| (i, self.nodes[i].tick()))
            .collect();
        for (i, counts) in self.last_round_heartbeats.iter_mut().enumerate() {
            *counts = actions[i]
                .1
                .iter()
                .filter(|a| matches!(a, ClusterAction::SendHeartbeat { .. }))
                .count();
        }
        for (from, acts) in actions {
            for act in acts {
                match act {
                    ClusterAction::SendHeartbeat { to, addr } => {
                        self.deliver_heartbeat(from, to, addr);
                    }
                    ClusterAction::Probe { to, addr } => {
                        // A probe is an ordinary heartbeat packet on the
                        // wire; deliver it the same way.
                        self.deliver_heartbeat(from, to, addr);
                    }
                    ClusterAction::SendGossip { targets } => {
                        for (to, _addr) in targets {
                            if let Some(to_idx) = self.index_of(to) {
                                if !self.is_cut(from, to_idx) && !self.nodes[to_idx].is_down() {
                                    let payload =
                                        self.nodes[from].gossip_payload(GOSSIP_PAYLOAD_MAX_ENTRIES);
                                    let sender = self.id(from);
                                    self.nodes[to_idx]
                                        .merge_membership_from_sender(payload, sender);
                                }
                            }
                        }
                    }
                    // Down/NodeFailed/NodeLeft/NodeJoined are
                    // bookkeeping notifications; ClusterState already
                    // applied their effects internally.
                    _ => {}
                }
            }
        }
        self.round += 1;
    }

    /// Advance the clock by `ROUND_STEP` and run one round.
    pub fn step(&mut self) {
        self.advance(ROUND_STEP);
        self.tick_round();
    }

    /// Run `rounds` steps (100 ms each).
    pub fn run_rounds(&mut self, rounds: usize) {
        for _ in 0..rounds {
            self.step();
        }
    }

    /// Run steps until every node sees every other node as `Healthy`
    /// (bounded by `max_rounds`). Returns the number of rounds used.
    pub fn run_to_healthy_mesh(&mut self, max_rounds: usize) -> usize {
        for _ in 0..max_rounds {
            self.step();
            if self.mesh_healthy() {
                break;
            }
        }
        self.round as usize
    }

    /// True when every node sees every other node as `Healthy`.
    pub fn mesh_healthy(&self) -> bool {
        for i in 0..self.nodes.len() {
            for j in 0..self.nodes.len() {
                if i != j && self.status_of(i, j) != Some(NodeStatus::Healthy) {
                    return false;
                }
            }
        }
        true
    }

    /// Node `i`'s current view of node `j`'s status.
    pub fn status_of(&self, i: usize, j: usize) -> Option<NodeStatus> {
        self.nodes[i].get_node(self.id(j)).map(|info| info.status)
    }

    /// Number of members node `i` considers reachable, using the same
    /// freshness-aware counting the resolver's view uses (delegates to
    /// `ClusterState::reachable_count`).
    pub fn reachable_count(&self, i: usize) -> usize {
        self.nodes[i].reachable_count()
    }
    pub fn is_down(&self, i: usize) -> bool {
        self.nodes[i].is_down()
    }

    fn deliver_heartbeat(&mut self, from: usize, to: NodeId, addr: SocketAddr) {
        if let Some(to_idx) = self.index_of(to) {
            if !self.is_cut(from, to_idx) && !self.nodes[to_idx].is_down() {
                let from_id = self.id(from);
                self.nodes[to_idx].handle_heartbeat(from_id, addr);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::cluster::{SplitBrainConfig, REPLY_SLOTS};
    use std::net::{IpAddr, Ipv4Addr};

    fn addrs(n: u16, base_port: u16) -> Vec<SocketAddr> {
        (0..n)
            .map(|i| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), base_port + i))
            .collect()
    }

    fn static_quorum(expected: usize) -> ClusterConfig {
        ClusterConfig {
            split_brain: SplitBrainConfig::StaticQuorum {
                expected_nodes: expected,
            },
            probe_interval: Duration::from_secs(5),
            ..Default::default()
        }
    }

    /// Enough rounds at 100 ms steps to drive a full failure-detection
    /// cycle (2 s timeout + 5 s suspicion) with margin.
    fn run_partition(sim: &mut SimCluster) {
        sim.run_rounds(100); // 10 s
    }

    #[test]
    fn test_clean_partition_minority_downs_itself() {
        // 5-node cluster, static quorum 3 of 5. Clean partition into a
        // 2-node minority and a 3-node majority; both sides healthy
        // among themselves.
        let mut sim = SimCluster::new(&addrs(5, 9100), &static_quorum(5));
        sim.run_to_healthy_mesh(200);
        assert!(sim.mesh_healthy(), "mesh must converge before partition");

        // Cut the minority {0,1} off from the majority {2,3,4} in both
        // directions.
        for from in [0, 1] {
            for to in [2, 3, 4] {
                sim.cut_link(from, to);
                sim.cut_link(to, from);
            }
        }

        run_partition(&mut sim);

        // The minority side sees 2 of 5 reachable (< 3) and downs
        // itself; the majority sees 3 of 5 (>= 3) and survives.
        assert!(sim.is_down(0), "minority node 0 must down itself");
        assert!(sim.is_down(1), "minority node 1 must down itself");
        for i in [2, 3, 4] {
            assert!(!sim.is_down(i), "majority node {i} must stay up");
        }
        // The majority's view of the minority is Failed.
        assert_eq!(sim.status_of(2, 0), Some(NodeStatus::Failed));
        assert_eq!(sim.status_of(3, 1), Some(NodeStatus::Failed));

        // Heal the partition: the downed minority stays down (transport
        // shut down; operator restart is the recovery path) and the
        // majority keeps it marked Failed — no silent resurrection.
        for from in [0, 1] {
            for to in [2, 3, 4] {
                sim.heal_link(from, to);
                sim.heal_link(to, from);
            }
        }
        sim.run_rounds(100); // well past the 5 s probe interval
        assert!(sim.is_down(0), "downed node stays down after heal");
        assert!(sim.is_down(1), "downed node stays down after heal");
        for i in [2, 3, 4] {
            assert!(!sim.is_down(i), "majority node {i} stays up");
        }
        assert_eq!(sim.status_of(2, 0), Some(NodeStatus::Failed));
        assert_eq!(sim.status_of(2, 1), Some(NodeStatus::Failed));
    }

    #[test]
    fn test_asymmetric_partition_isolates_the_silent_node() {
        // One-way partition: nodes 1-4 cannot send to node 0, but 0 can
        // send to them. From 0's perspective everyone is dead; from
        // everyone else's perspective 0 is perfectly healthy.
        let mut sim = SimCluster::new(&addrs(5, 9200), &static_quorum(5));
        sim.run_to_healthy_mesh(200);
        // Run long enough for active views to fully populate via repair_active_view
        // (1 node per 2s heartbeat_timeout = 8s for 4 nodes).
        sim.run_rounds(100);
        assert!(sim.mesh_healthy());

        for from in 1..5 {
            sim.cut_link(from, 0);
        }

        // Phase 1 (t = 2.8 s): the asymmetry window. The resolver counts
        // only Healthy/Joining members as reachable, so as soon as 0's
        // detector goes Suspicious on everyone (2 s timeout — it hears
        // nothing) 0 drops below quorum and downs itself. The other
        // four still receive 0's heartbeats (sent up to the 2 s mark)
        // and see it Healthy.
        sim.run_rounds(28);
        assert!(sim.is_down(0), "silent node must down itself at ~2 s");
        assert_eq!(sim.status_of(0, 1), Some(NodeStatus::Suspicious));
        for i in 1..5 {
            assert_eq!(sim.status_of(i, 0), Some(NodeStatus::Healthy));
            assert!(!sim.is_down(i));
        }

        // Phase 2 (t = 10 s): 0 stopped heartbeating at the 2 s mark, so
        // the other side ages it through Suspicious to Failed. Only 0
        // lost quorum (1 of 5 < 3); 1-4 keep all five reachable and stay
        // up. The downed 0's own view still progresses to Failed via its
        // (silent) ticks.
        sim.run_rounds(75);
        assert!(sim.is_down(0), "silent node must down itself");
        for i in 1..5 {
            assert!(!sim.is_down(i), "node {i} must stay up");
            assert_eq!(sim.status_of(i, 0), Some(NodeStatus::Failed));
        }
        assert_eq!(sim.status_of(0, 1), Some(NodeStatus::Failed));

        // Phase 3: heal — probes from 1-4 to 0 are dropped (0's
        // transport is down); 0 requires an operator restart.
        for from in 1..5 {
            sim.heal_link(from, 0);
        }
        sim.run_rounds(100);
        assert!(sim.is_down(0));
        for i in 1..5 {
            assert!(!sim.is_down(i));
        }
    }
    #[test]
    fn test_probe_rejoins_healed_partition_without_resolver_down() {
        // 5-node cluster with quorum 2 of 3: neither side of a 2/3 split
        // loses quorum, so no one downs itself — and the probe path must
        // re-join the healed partition without an external rejoin.
        let mut sim = SimCluster::new(&addrs(5, 9300), &static_quorum(3));
        sim.run_to_healthy_mesh(200);

        assert!(sim.mesh_healthy());

        for from in [0, 1] {
            for to in [2, 3, 4] {
                sim.cut_link(from, to);
                sim.cut_link(to, from);
            }
        }

        run_partition(&mut sim);

        // Both sides keep quorum (2 and 3 reachable >= 2): nobody down.
        for i in 0..5 {
            assert!(!sim.is_down(i), "node {i} must stay up at quorum 2");
        }
        assert_eq!(sim.status_of(0, 2), Some(NodeStatus::Failed));
        assert_eq!(sim.status_of(2, 0), Some(NodeStatus::Failed));

        // Heal and let the probes fire: within the probe interval plus
        // heartbeat rounds the mesh must fully converge again.
        for from in [0, 1] {
            for to in [2, 3, 4] {
                sim.heal_link(from, to);
                sim.heal_link(to, from);
            }
        }
        sim.run_rounds(200); // 20 s, well past probe_interval (5 s)
        assert!(sim.mesh_healthy(), "mesh must re-converge via probes");
        for i in 0..5 {
            assert!(!sim.is_down(i));
        }
    }

    #[test]
    fn test_two_node_cluster_fails_closed_on_partition() {
        // Documented caveat of StaticQuorumResolver: with expected_nodes
        // == 2 both sides of a partition down themselves (1 < 2).
        let mut sim = SimCluster::new(&addrs(2, 9400), &static_quorum(2));
        sim.run_to_healthy_mesh(200);
        assert!(sim.mesh_healthy());

        sim.cut_link(0, 1);
        sim.cut_link(1, 0);
        run_partition(&mut sim);

        assert!(sim.is_down(0), "both sides of a 2-node split fail closed");
        assert!(sim.is_down(1), "both sides of a 2-node split fail closed");
    }

    #[test]
    fn test_quorum_holds_under_random_partitions() {
        // Seed-driven invariant sweep: for 50 random directed cut sets on
        // a 5-node mesh, the resolver must never down a node that still
        // sees the full quorum (3 of 5) reachable. Each pattern runs on a
        // fresh, fully-healthy mesh (a downed node stays down, so reusing
        // one sim would degenerate later patterns).
        let mut rng = crate::dst::DeterministicRng::new(42);
        for _ in 0..50 {
            let mut sim = SimCluster::new(&addrs(5, 9500), &static_quorum(5));
            sim.run_to_healthy_mesh(200);
            for from in 0..5 {
                for to in 0..5 {
                    if from != to && rng.next() % 4 == 0 {
                        sim.cut_link(from, to);
                    }
                }
            }
            sim.run_rounds(100);
            for i in 0..5 {
                let reachable = sim.reachable_count(i);
                if sim.is_down(i) {
                    assert!(
                        reachable < 3,
                        "node {i} downed with {reachable} reachable (>= quorum 3)"
                    );
                }
            }
        }
    }
    // -- Partial-view membership (Phase 5 deliverable 6) ------------------

    const ACTIVE_VIEW_SIZE: usize = 4;

    #[test]
    fn test_partial_view_fanout_bounded() {
        // 30-node cluster with 4-wide active views: after convergence,
        // each node heartbeats at most active + probationary + reply
        // slots per round — O(1) in the cluster size, never full-mesh
        // (which would be 29 heartbeats per node per round here).
        let mut sim = SimCluster::new(&addrs(30, 9600), &static_quorum(30));
        sim.run_to_healthy_mesh(400);
        assert!(sim.mesh_healthy());

        const HEARTBEAT_BOUND: usize = ACTIVE_VIEW_SIZE + ACTIVE_VIEW_SIZE + REPLY_SLOTS;
        sim.run_rounds(10);
        for i in 0..30 {
            assert!(
                sim.last_round_heartbeats[i] <= HEARTBEAT_BOUND,
                "node {i} sent {} heartbeats in one round (bound {HEARTBEAT_BOUND})",
                sim.last_round_heartbeats[i]
            );
            assert!(
                sim.last_round_heartbeats[i] < 29,
                "node {i} must not full-mesh heartbeat, sent {}",
                sim.last_round_heartbeats[i]
            );
            assert!(
                sim.node(i).active_view().len() <= ACTIVE_VIEW_SIZE,
                "node {i} active view exceeds the bound"
            );
        }
        // The mesh is still fully healthy — bounded heartbeats did not
        // fragment membership knowledge (gossip carries the statuses).
        assert!(sim.mesh_healthy());
    }

    #[test]
    fn test_partial_view_converges_at_ten_nodes() {
        // The full-mesh bootstrap (everyone joins everyone) converges to
        // an all-Healthy mesh at 10 nodes with 4-wide views.
        let mut sim = SimCluster::new(&addrs(10, 9700), &static_quorum(10));
        let rounds = sim.run_to_healthy_mesh(300);
        assert!(
            sim.mesh_healthy(),
            "10-node mesh must converge, took {rounds} rounds"
        );
    }

    #[test]
    fn test_partial_view_death_detected_no_false_failures() {
        // Kill node 0 (cut every link touching it). Its active partners
        // fail it via the detector; the rest learn via gossip. No
        // healthy node may be falsely failed: only 0's status changes.
        let mut sim = SimCluster::new(&addrs(10, 9800), &static_quorum(10));
        sim.run_to_healthy_mesh(300);
        assert!(sim.mesh_healthy());

        for from in 1..10 {
            sim.cut_link(0, from);
            sim.cut_link(from, 0);
        }

        sim.run_rounds(200); // 20 s: failure window (7 s) + gossip spread

        for i in 1..10 {
            assert_eq!(
                sim.status_of(i, 0),
                Some(NodeStatus::Failed),
                "node {i} must learn that node 0 failed"
            );
            assert!(!sim.is_down(i), "node {i} must stay up");
            // Active views shrink by at most the one lost member.
            assert!(
                sim.node(i).active_view().len() >= ACTIVE_VIEW_SIZE - 1,
                "node {i} active view must not shrink below {ACTIVE_VIEW_SIZE} - 1"
            );
        }
        // No healthy pair may be Failed: mutual heartbeats keep every
        // survivor watched and fresh.
        for i in 1..10 {
            for j in 1..10 {
                if i != j {
                    assert_ne!(
                        sim.status_of(i, j),
                        Some(NodeStatus::Failed),
                        "node {i} falsely failed healthy node {j}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_partial_view_rejoin_after_heal() {
        // Continue the death scenario: heal every link. Node 0 was
        // downed by the resolver (1 of 10 reachable < 6) and stays down;
        // the survivors must re-converge among themselves via probes and
        // gossip, with all active views repaired.
        let mut sim = SimCluster::new(&addrs(10, 9900), &static_quorum(10));
        sim.run_to_healthy_mesh(300);
        assert!(sim.mesh_healthy());

        for from in 1..10 {
            sim.cut_link(0, from);
            sim.cut_link(from, 0);
        }
        sim.run_rounds(200);
        assert!(sim.is_down(0), "isolated node downs itself below quorum");

        for from in 1..10 {
            sim.heal_link(0, from);
            sim.heal_link(from, 0);
        }
        sim.run_rounds(200);

        for i in 1..10 {
            assert!(!sim.is_down(i), "node {i} must stay up");
            // Probes to the downed node are dropped, so it stays Failed
            // on every survivor's side (operator restart is its path).
            assert_eq!(sim.status_of(i, 0), Some(NodeStatus::Failed));
            assert!(
                sim.node(i).active_view().len() >= ACTIVE_VIEW_SIZE - 1,
                "node {i} active view must repair after the heal"
            );
        }
        for i in 1..10 {
            for j in 1..10 {
                if i != j {
                    assert_eq!(
                        sim.status_of(i, j),
                        Some(NodeStatus::Healthy),
                        "survivors must re-converge after the heal ({i} -> {j})"
                    );
                }
            }
        }
    }
    #[test]
    fn test_rolling_restart_rejoins_cluster() {
        // 5-node cluster. One node gets isolated (partition), downs
        // itself, then is "restarted" by replacing its ClusterState with
        // a fresh one at the same address. The fresh node must rejoin
        // via the probe path and the mesh must re-converge.
        let mut sim = SimCluster::new(&addrs(5, 10000), &static_quorum(5));
        sim.run_to_healthy_mesh(200);
        assert!(sim.mesh_healthy(), "mesh must converge before partition");

        // Isolate node 0 (minority of 1): it downs itself.
        for from in 1..5 {
            sim.cut_link(0, from);
            sim.cut_link(from, 0);
        }
        sim.run_rounds(100); // well past failure window
        assert!(sim.is_down(0), "isolated node must down itself");
        for i in 1..5 {
            assert!(!sim.is_down(i), "majority must stay up");
        }

        // "Restart" node 0: replace its ClusterState with a fresh one at
        // the same address (same NodeId). The old state is dropped; the
        // new one joins the cluster via the seed mechanism.
        let addr0 = sim.addrs[0];
        let clock0 = sim.clocks[0].clone();
        let mut fresh = ClusterState::new(NodeId::new(&addr0), addr0);
        fresh.set_clock(clock0.clone());
        assert!(fresh.apply_config(&static_quorum(5)));
        // Join using the existing majority as seeds. Use handle_heartbeat
        // instead of join_cluster so last_heartbeat is set via the virtual
        // clock (self.now()), not Instant::now(). This keeps the fresh
        // node's view time-synced with the rest of the simulation.
        for i in 1..5 {
            let seed_id = sim.id(i);
            let seed_addr = sim.addrs[i];
            fresh.handle_heartbeat(seed_id, seed_addr);
        }
        sim.nodes[0] = fresh;
        // Heal all links
        for from in 1..5 {
            sim.heal_link(0, from);
            sim.heal_link(from, 0);
        }

        // Advance and let probes fire. The restarted node should rejoin.
        sim.run_rounds(200); // well past probe interval

        assert!(!sim.is_down(0), "restarted node must rejoin");
        assert!(sim.mesh_healthy(), "mesh must fully converge after restart");
        for i in 0..5 {
            assert_eq!(sim.status_of(0, i), Some(NodeStatus::Healthy));
            assert_eq!(sim.status_of(i, 0), Some(NodeStatus::Healthy));
        }
    }

    #[test]
    #[ignore]
    fn test_quorum_holds_under_random_partitions_1000_seeds() {
        // Higher-seed invariant sweep for CI: 1000 random directed cut
        // sets on a 5-node mesh. Same invariant as the 50-seed test:
        // the resolver must never down a node that still sees >= quorum
        // reachable. Marked `#[ignore]` by default; run with
        // `-- --ignored` in CI to hit the 10³ seeds/commit target.
        let mut rng = crate::dst::DeterministicRng::new(1337);
        for seed in 0..1000 {
            let mut sim = SimCluster::new(&addrs(5, 10100), &static_quorum(5));
            sim.run_to_healthy_mesh(200);
            for from in 0..5 {
                for to in 0..5 {
                    if from != to && rng.next() % 4 == 0 {
                        sim.cut_link(from, to);
                    }
                }
            }
            sim.run_rounds(100);
            for i in 0..5 {
                let reachable = sim.reachable_count(i);
                if sim.is_down(i) {
                    assert!(
                        reachable < 3,
                        "seed {seed}: node {i} downed with {reachable} reachable (>= quorum 3)"
                    );
                }
            }
        }
    }
}
