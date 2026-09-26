//! ORCA Cycle Detector — Stage A3 of the Nulang v0.4 Garbage Collector.
//!
//! This module implements the centralized cycle detection component of ORCA
//! (Optimized Reference Counting Architecture). While per-actor reference
//! counting handles acyclic garbage efficiently, cross-actor references can
//! form cycles that naive reference counting never reclaims. This module
//! detects and breaks such cycles.
//!
//! # Algorithm Overview
//!
//! ORCA's key insight is that **cycles can only form through cross-actor
//! references** (foreign references). The cycle detector maintains a directed
//! graph of all foreign references between objects owned by different actors,
//! then periodically searches this graph for cycles.
//!
//! The detection pipeline has five phases:
//!
//! 1. **Registration** — Build the foreign reference graph as actors send
//!    and receive references.
//! 2. **Suspicion** — Use a weighted heuristic to flag objects that are
//!    likely to be part of cycles, avoiding expensive DFS on every object.
//! 3. **Detection** — For each suspect, run depth-first search following
//!    foreign edges. A path back to the starting node indicates a cycle.
//! 4. **Trial Decrement** — Temporarily decrement reference counts along
//!    a detected cycle to test whether the objects are truly garbage.
//! 5. **Reclamation** — If trial decrements cause counts to reach zero,
//!    the cycle is garbage and all objects in it are reclaimed.
//!
//! # Weighted Heuristic
//!
//! To avoid scanning the entire graph, ORCA assigns each object a *weight*:
//!
//! ```text
//! weight(object) = foreign_count(object) - Σ(ref_count of outgoing foreign edges)
//! ```
//!
//! An object with `weight <= suspect_threshold` has no "extra" foreign
//! references beyond what we can account for in the graph — it is a strong
//! candidate for being in a cycle. Objects with high weight have external
//! references keeping them alive and can be skipped.
//!
//! # Thread Safety
//!
//! The cycle detector runs on the single scheduler thread (the runtime is a
//! single-threaded synchronous coordinator).  It does **not** require
//! internal locking for the graph itself, and header refcounts plus the
//! statistics counters are plain integers — no atomics anywhere in this
//! module.
//!
//! # Safety
//!
//! This module uses `unsafe` blocks when dereferencing `*mut OrcaHeader`
//! pointers stored in the graph. These pointers are valid only while the
//! corresponding object is alive. Every dereference checks the object's
//! reference count first (non-zero count implies the object is alive).
//!
//! The invariant that the cycle detector's graph reflects the actual runtime
//! foreign reference graph is maintained by the runtime calling
//! `register_foreign_ref` and `remove_foreign_ref` on every foreign
//! reference operation.

use crate::runtime::gc::ForeignRefOp;
use crate::runtime::heap::OrcaHeader;
use std::collections::{HashMap, HashSet, VecDeque};

// ---------------------------------------------------------------------------
//  CycleRuntime trait
// ---------------------------------------------------------------------------

/// Runtime capability required by the cycle detector to reclaim garbage cycles.
///
/// The cycle detector is intentionally decoupled from [`Runtime`](super::Runtime);
/// this trait is the only bridge it needs to free objects on the correct actor
/// heap and to drop any pending deferred-decrement entries for those objects.
pub trait CycleRuntime {
    /// Free the object identified by `header` on `actor_id`'s heap.
    ///
    /// # Safety
    /// `header` must point to a live object owned by `actor_id`.
    unsafe fn free_object(&mut self, actor_id: u64, header: *mut OrcaHeader);
}

// ---------------------------------------------------------------------------
//  Data Structures
// ---------------------------------------------------------------------------

/// Color used during the cycle detector's own mark phase.
///
/// These colors are conceptually similar to tricolor GC marking but serve a
/// different purpose: they track which nodes have been visited during a
/// single DFS traversal for cycle detection, not for general GC reachability.
///
/// - **White** — Node has not been visited in the current detection pass.
/// - **Gray** — Node is currently on the DFS recursion stack (part of the
///   active path being explored).
/// - **Black** — Node has been fully explored and is not part of any cycle
///   rooted in the current search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeColor {
    White,
    Gray,
    Black,
}

/// A directed edge in the foreign reference graph.
///
/// Represents a foreign reference from one object to another. The edge is
/// *directed* from the source object (which holds the reference) to the
/// target object (which is referenced). Multiple references between the
/// same pair of objects are collapsed into a single edge with a `ref_count`
/// indicating how many references exist.
///
/// # Invariants
///
/// - `ref_count` is always > 0 for edges stored in the graph.
/// - `target_actor` must differ from the source object's owning actor
///   (otherwise it would not be a *foreign* reference).
#[derive(Debug, Clone, Copy)]
pub struct ForeignEdge {
    /// The actor that owns the target object.
    pub target_actor: u64,
    /// Pointer to the target object's header.
    pub target_object: *mut OrcaHeader,
    /// Number of references along this edge (>= 1).
    pub ref_count: u32,
}

// Deliberately no Send/Sync impl: these edges are scheduler-local graph
// records owned by CycleDetector. The raw pointer is compared/hashed as an
// address here and must not acquire broader cross-thread guarantees.

impl PartialEq for ForeignEdge {
    fn eq(&self, other: &Self) -> bool {
        self.target_actor == other.target_actor && self.target_object == other.target_object
    }
}

impl Eq for ForeignEdge {}

impl std::hash::Hash for ForeignEdge {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.target_actor.hash(state);
        self.target_object.hash(state);
    }
}

/// A node in the foreign reference graph.
///
/// Each node represents a single heap object (identified by its header
/// pointer) and tracks all outgoing foreign reference edges from that object.
///
/// # Lifetime
///
/// A node is valid as long as its `object_header` pointer is valid. The
/// cycle detector should remove nodes when their corresponding objects are
/// freed. The `is_alive` method checks whether the object still exists by
/// verifying that its reference count is non-zero.
#[derive(Debug)]
pub struct ForeignRefNode {
    /// The actor that owns this object.
    pub actor_id: u64,
    /// Pointer to the object's GC header.
    ///
    /// # Safety
    ///
    /// This pointer is valid only while the object is alive. Always check
    /// `is_alive()` before dereferencing.
    pub object_header: *mut OrcaHeader,
    /// All outgoing foreign reference edges from this object.
    pub foreign_refs: Vec<ForeignEdge>,
    /// Weight for the heuristic: `foreign_count - sum(outgoing_edge_ref_counts)`.
    /// Updated lazily during suspicion phase.
    pub weight: u32,
    /// Color for mark phase during detection.
    pub color: NodeColor,
    /// Epoch when this node was last visited (for incremental detection).
    pub visited_epoch: u64,
}

// Deliberately no Send/Sync impl: CycleDetector owns these nodes on its
// single scheduler/coordinator path. Pointer dereferences still require the
// explicit liveness/provenance contract on the unsafe methods below.

impl ForeignRefNode {
    /// Check whether the object this node represents is still alive.
    ///
    /// An object is considered alive if its total reference count
    /// (local + foreign) is greater than zero. If the count has reached
    /// zero, the object may have been or is about to be reclaimed.
    ///
    /// # Safety
    ///
    /// This method dereferences `self.object_header`. The caller must ensure
    /// that the header pointer is still valid (i.e., points to mapped memory).
    /// In practice, the runtime only frees objects after the cycle detector
    /// has been notified, so this is safe as long as the notification
    /// protocol is followed.
    pub unsafe fn is_alive(&self) -> bool {
        // Dereference the header pointer to read reference counts.
        // SAFETY: The runtime guarantees that headers are not freed until
        // the cycle detector processes the removal notification.
        let header = &*self.object_header;
        let local = header.ref_count;
        let foreign = header.foreign_count;
        (local + foreign) > 0
    }

    /// Compute the current weight of this node.
    ///
    /// Weight = foreign_count(object) - sum of ref_counts of all outgoing edges.
    /// A low weight means the object's foreign references are mostly
    /// accounted for by edges in our graph, suggesting it may be in a cycle.
    ///
    /// # Safety
    ///
    /// Reads `self.object_header`. Caller must ensure the pointer is valid.
    pub unsafe fn compute_weight(&self) -> u32 {
        let header = &*self.object_header;
        let foreign_count = header.foreign_count;
        let outgoing_sum: u32 = self.foreign_refs.iter().map(|e| e.ref_count).sum();
        // Saturating subtraction to avoid underflow.
        foreign_count.saturating_sub(outgoing_sum)
    }
}

impl PartialEq for ForeignRefNode {
    fn eq(&self, other: &Self) -> bool {
        self.actor_id == other.actor_id && self.object_header == other.object_header
    }
}

impl Eq for ForeignRefNode {}

/// A suspect object flagged by the weighted heuristic.
///
/// When the cycle detector scans the graph, objects with weight below the
/// threshold are enqueued as suspects. Suspects are processed incrementally
/// (one at a time) to avoid long pause times.
#[derive(Debug, Clone, Copy)]
pub struct Suspect {
    /// Actor owning the suspected object.
    pub actor_id: u64,
    /// Pointer to the object's header.
    ///
    /// # Safety
    ///
    /// Must be checked for liveness before dereferencing.
    pub object_header: *mut OrcaHeader,
    /// Computed weight at the time of flagging.
    pub weight: u32,
    /// Epoch when this suspect was flagged.
    pub flagged_epoch: u64,
}

/// The centralized ORCA cycle detector.
///
/// The cycle detector maintains a directed graph of all cross-actor
/// (foreign) object references and periodically searches it for cycles.
/// It uses a weighted heuristic to prioritize which objects to examine,
/// reducing the overhead of full graph traversal.
///
/// # Design
///
/// - Single-threaded: The cycle detector runs on one coordinator thread.
///   No internal synchronization is needed for graph mutations.
/// - Incremental: Detection work is spread across multiple calls to
///   `incremental_detect`, each processing a bounded amount of work.
/// - Conservative: Trial decrements ensure we only reclaim objects that
///   are provably unreachable.
///
/// # Example
///
/// ```ignore
/// let mut detector = CycleDetector::new();
/// // When actor 1 sends a ref to actor 2:
/// detector.register_foreign_ref(1, obj_a, 2, obj_b);
/// // Periodically:
/// detector.incremental_detect(&runtime);
/// ```
pub struct CycleDetector {
    /// The foreign reference graph.
    ///
    /// Key: (actor_id, object_header_address) — using the header pointer's
    /// address as a usize gives us a stable, hashable identifier.
    ///
    /// Value: The node representing this object and its outgoing edges.
    graph: HashMap<(u64, usize), ForeignRefNode>,

    /// Queue of suspect objects to investigate.
    ///
    /// Suspects are processed FIFO. New suspects are appended at the back;
    /// `incremental_detect` pops from the front.
    suspects: VecDeque<Suspect>,

    /// Current epoch counter.
    ///
    /// Incremented on each detection pass. Used to avoid revisiting nodes
    /// multiple times within the same pass and to age out old suspects.
    epoch: u64,

    /// Set of actor IDs that are local to this node. When `Some`, the
    /// detector only processes suspects and references belonging to local
    /// actors, keeping it strictly intra-node. `None` disables filtering
    /// (used in unit tests with mock runtimes).
    local_actors: Option<HashSet<u64>>,

    /// Threshold for flagging objects as suspects.
    ///
    /// Objects with `weight <= suspect_threshold` are enqueued for deeper
    /// inspection. The default value of 1 catches objects whose foreign
    /// references are fully accounted for by graph edges.
    suspect_threshold: u32,

    /// How many epochs between full detection passes.
    ///
    /// A full pass scans all nodes and rebuilds the suspect queue.
    /// Incremental steps process one suspect at a time.
    detection_interval: u64,

    /// Number of cycles found and reclaimed (for statistics).
    cycles_found: u64,

    /// Number of objects reclaimed from broken cycles (for statistics).
    objects_reclaimed: u64,

    /// Snapshot of graph keys for the current incremental full scan.
    /// `None` when no scan is in progress.
    scan_keys: Option<Vec<(u64, usize)>>,
    /// How many keys have been processed in the current scan.
    scan_cursor: usize,
    /// Maximum number of nodes to scan per `refresh_suspects` call.
    scan_batch_size: usize,
}

// ---------------------------------------------------------------------------
//  CycleDetector Implementation
// ---------------------------------------------------------------------------

impl CycleDetector {
    /// Create a new cycle detector with sensible default parameters.
    ///
    /// Defaults:
    /// - `suspect_threshold`: 1 (objects with weight 0 or 1 are suspects)
    /// - `detection_interval`: 10 (full scan every 10 epochs)
    pub fn new() -> Self {
        Self {
            scan_keys: None,
            scan_cursor: 0,
            scan_batch_size: 100, // process at most 100 nodes per call
            graph: HashMap::new(),
            suspects: VecDeque::new(),
            epoch: 0,
            suspect_threshold: 1,
            detection_interval: 10,
            local_actors: None,
            cycles_found: 0,
            objects_reclaimed: 0,
        }
    }

    /// Restrict cycle detection to the given set of local actor IDs.
    ///
    /// When called with a non-empty set, the detector ignores any foreign
    /// reference whose target actor is not in the set, and skips suspects
    /// owned by non-local actors. This satisfies the v1.0 requirement that
    /// the centralized cycle detector operate intra-node only.
    pub fn set_local_actors(&mut self, local_actor_ids: HashSet<u64>) {
        self.local_actors = Some(local_actor_ids);
    }

    /// Remove the local-actor restriction.
    pub fn clear_local_actors(&mut self) {
        self.local_actors = None;
    }

    /// Return the current local-actor restriction, if any.
    pub fn local_actors(&self) -> Option<&HashSet<u64>> {
        self.local_actors.as_ref()
    }

    fn is_local(&self, actor_id: u64) -> bool {
        match &self.local_actors {
            Some(set) => set.contains(&actor_id),
            None => true,
        }
    }

    // -- Graph construction ------------------------------------------------

    /// Register a new foreign reference edge in the graph.
    ///
    /// Called by the runtime whenever an actor sends a reference to an
    /// object it owns to another actor. This creates (or strengthens) a
    /// directed edge from the source object to the target object.
    ///
    /// # Parameters
    ///
    /// - `from_actor` — ID of the actor that owns the source object.
    /// - `from_object` — Header pointer of the source object.
    /// - `to_actor` — ID of the actor that owns the target object.
    /// - `to_object` — Header pointer of the target object.
    ///
    /// # Safety
    ///
    /// `from_object` and `to_object` must point to valid, live `OrcaHeader`
    /// structures. The runtime guarantees this by only calling this method
    /// during active reference-sending operations.
    pub fn register_foreign_ref(
        &mut self,
        from_actor: u64,
        from_object: *mut OrcaHeader,
        to_actor: u64,
        to_object: *mut OrcaHeader,
    ) {
        // Ignore self-references within the same actor — these are not
        // *foreign* references and cannot participate in cross-actor cycles.
        if from_actor == to_actor {
            return;
        }
        // When restricted to local actors, ignore edges that target (or originate from)
        // remote actors. This keeps the centralized detector intra-node only.
        if !self.is_local(to_actor) {
            return;
        }
        if self.local_actors.is_some() && !self.is_local(from_actor) {
            return;
        }

        let key = (from_actor, from_object as usize);

        // Get or create the source node.
        let node = self.graph.entry(key).or_insert_with(|| ForeignRefNode {
            actor_id: from_actor,
            object_header: from_object,
            foreign_refs: Vec::new(),
            weight: 0,
            color: NodeColor::White,
            visited_epoch: 0,
        });

        // Check if an edge to this target already exists; if so, increment
        // its ref_count. Otherwise, create a new edge.
        if let Some(edge) = node
            .foreign_refs
            .iter_mut()
            .find(|e| e.target_actor == to_actor && e.target_object == to_object)
        {
            edge.ref_count = edge.ref_count.saturating_add(1);
        } else {
            node.foreign_refs.push(ForeignEdge {
                target_actor: to_actor,
                target_object: to_object,
                ref_count: 1,
            });
        }
    }

    /// Remove a foreign reference edge from the graph.
    ///
    /// Called by the runtime when a foreign reference is dropped (e.g., via
    /// `drop_local_ref` on a received reference). Decrements or removes the
    /// corresponding edge.
    ///
    /// # Parameters
    ///
    /// Same as `register_foreign_ref`.
    ///
    /// # Edge Cases
    ///
    /// - If the edge's ref_count drops to zero, the edge is removed.
    /// - If the node has no remaining edges after removal, the node itself
    ///   is removed from the graph.
    /// - If the node or edge does not exist, this is a no-op (idempotent).
    pub fn remove_foreign_ref(
        &mut self,
        from_actor: u64,
        from_object: *mut OrcaHeader,
        to_actor: u64,
        to_object: *mut OrcaHeader,
    ) {
        let key = (from_actor, from_object as usize);

        let should_remove_node = if let Some(node) = self.graph.get_mut(&key) {
            if let Some(pos) = node
                .foreign_refs
                .iter()
                .position(|e| e.target_actor == to_actor && e.target_object == to_object)
            {
                let edge = &mut node.foreign_refs[pos];
                if edge.ref_count <= 1 {
                    node.foreign_refs.swap_remove(pos);
                } else {
                    edge.ref_count -= 1;
                }
            }
            // Remove the node if it has no more outgoing foreign refs.
            node.foreign_refs.is_empty()
        } else {
            false
        };

        if should_remove_node {
            self.graph.remove(&key);
        }
    }

    // -- Detection entry points --------------------------------------------

    /// Run one incremental cycle detection step.
    ///
    /// This method is designed to be called frequently (e.g., every N
    /// scheduling rounds) with a bounded amount of work per call. It
    /// processes at most one suspect per invocation, keeping pause times low.
    ///
    /// # Algorithm
    ///
    /// 1. Increment the epoch.
    /// 2. If the epoch aligns with `detection_interval`, run a full scan
    ///    to refresh the suspect queue (`refresh_suspects`).
    /// 3. Otherwise, if suspects are queued, pop one and process it
    ///    (`process_suspect`).
    ///
    /// # Parameters
    ///
    /// - `runtime` — The runtime context, used to send trial decrements
    ///   and reclaim objects.
    pub fn incremental_detect<R: CycleRuntime>(&mut self, runtime: &mut R) {
        self.epoch += 1;

        if self.epoch % self.detection_interval == 0 {
            // Full scan: rebuild the suspect queue from the current graph.
            self.refresh_suspects(runtime);
        }

        // Process one suspect per incremental step.
        if let Some(suspect) = self.suspects.pop_front() {
            // SAFETY: process_suspect reads object headers. We verify
            // liveness before any dereference.
            unsafe {
                self.process_suspect(&suspect, runtime);
            }
        }
    }
    fn refresh_suspects<R>(&mut self, _runtime: &R) {
        // Start a new scan if none is in progress.
        if self.scan_keys.is_none() {
            self.suspects.clear();
            self.scan_keys = Some(self.graph.keys().copied().collect());
            self.scan_cursor = 0;
        }

        let keys = match self.scan_keys.as_ref() {
            Some(k) => k,
            None => return,
        };
        let end = (self.scan_cursor + self.scan_batch_size).min(keys.len());

        for &key in &keys[self.scan_cursor..end] {
            let is_local = if let Some(node) = self.graph.get(&key) {
                self.is_local(node.actor_id)
            } else {
                true
            };
            if !is_local {
                continue;
            }
            if let Some(node) = self.graph.get_mut(&key) {
                let alive = unsafe {
                    (*node.object_header).ref_count + (*node.object_header).foreign_count > 0
                };
                if !alive {
                    node.weight = u32::MAX;
                    continue;
                }
                let weight = unsafe { node.compute_weight() };
                node.weight = weight;
                if weight <= self.suspect_threshold {
                    self.suspects.push_back(Suspect {
                        actor_id: node.actor_id,
                        object_header: node.object_header,
                        weight,
                        flagged_epoch: self.epoch,
                    });
                }
            }
        }

        self.scan_cursor = end;

        // Scan complete: clean up dead nodes and reset state.
        if self.scan_cursor >= keys.len() {
            self.graph.retain(|_, node| node.weight != u32::MAX);
            self.scan_keys = None;
            self.scan_cursor = 0;
        }
    }

    /// Run a full cycle detection pass.
    ///
    /// This is the core algorithm that searches the entire foreign reference
    /// graph for cycles. Unlike `incremental_detect`, this processes **all**
    /// suspects in a single call. Use with care, as it may have longer pause
    /// times.
    ///
    /// # Algorithm
    ///
    /// 1. Refresh the suspect queue (scan all nodes, recompute weights).
    /// 2. For each suspect, perform DFS following foreign edges.
    /// 3. If DFS returns to the starting node, a cycle is found.
    /// 4. Send trial decrements, confirm, and reclaim if garbage.
    ///
    /// # Parameters
    ///
    /// - `runtime` — Mutable reference to the runtime for sending operations
    ///   and reclaiming objects.
    pub fn detect_cycles<R: CycleRuntime>(&mut self, runtime: &mut R) {
        self.epoch += 1;
        self.refresh_suspects(runtime);

        // Process all suspects. We collect them first to avoid borrowing
        // issues between suspect iteration and graph mutation.
        let suspects: Vec<Suspect> = self.suspects.drain(..).collect();

        for suspect in &suspects {
            // SAFETY: process_suspect dereferences object headers.
            unsafe {
                self.process_suspect(suspect, runtime);
            }
        }
    }

    // -- Per-suspect processing --------------------------------------------

    /// Process a single suspect object.
    ///
    /// Uses the weighted heuristic: if the object's total foreign weight
    /// is below the threshold, it's a candidate for cycle testing. This
    /// method performs a DFS from the suspect to detect cycles.
    ///
    /// # Algorithm
    ///
    /// 1. Verify the suspect object is still alive.
    /// 2. Reset colors for all nodes to White (new detection pass).
    /// 3. Run DFS from the suspect, following foreign_ref edges.
    /// 4. If DFS finds a path back to the starting node, extract the cycle.
    /// 5. Send trial decrements, confirm, and reclaim if appropriate.
    ///
    /// # Parameters
    ///
    /// - `suspect` — The suspect object to investigate.
    /// - `runtime` — The runtime context.
    ///
    /// # Safety
    ///
    /// Dereferences `suspect.object_header` and potentially other headers
    /// during DFS. All headers are checked for liveness before dereferencing.
    unsafe fn process_suspect<R: CycleRuntime>(&mut self, suspect: &Suspect, runtime: &mut R) {
        if !self.is_local(suspect.actor_id) {
            return;
        }
        // Verify the suspect object is still alive.
        let key = (suspect.actor_id, suspect.object_header as usize);
        let Some(start_node) = self.graph.get(&key) else {
            // Object no longer has any foreign refs — not in a cycle.
            return;
        };

        // Check liveness.
        if !start_node.is_alive() {
            return;
        }

        // Reset all node colors to White for a fresh detection pass.
        for node in self.graph.values_mut() {
            node.color = NodeColor::White;
        }

        // Run DFS from the suspect to find cycles.
        let mut path: Vec<(u64, *mut OrcaHeader)> = Vec::new();
        if let Some(cycle) = self.dfs_find_cycle(suspect.actor_id, suspect.object_header, &mut path)
        {
            // Cycle found! Send trial decrements.
            self.send_trial_decrements(&cycle);

            // Check if the cycle is garbage (all objects have zero count).
            if self.is_cycle_garbage(&cycle) {
                self.confirm_and_reclaim(&cycle, runtime);
            } else {
                self.cancel_trial_decrements(&cycle);
            }
        }
    }

    /// Depth-first search for cycles starting from a given node.
    ///
    /// Follows foreign reference edges recursively. If we encounter a Gray
    /// node (on the current path), a cycle is found. If we encounter a
    /// Black node, that subtree has already been explored and has no cycles.
    ///
    /// # Parameters
    ///
    /// - `actor_id` — Actor ID of the current node.
    /// - `object` — Header pointer of the current node.
    /// - `path` — Current DFS path (stack of visited nodes).
    ///
    /// # Returns
    ///
    /// `Some(cycle)` if a cycle is found, where `cycle` is the sequence of
    /// nodes forming the cycle. `None` if no cycle was found from this node.
    ///
    /// # Safety
    ///
    /// Dereferences header pointers in the graph. Nodes are checked for
    /// existence in the graph before use.
    unsafe fn dfs_find_cycle(
        &mut self,
        actor_id: u64,
        object: *mut OrcaHeader,
        path: &mut Vec<(u64, *mut OrcaHeader)>,
    ) -> Option<Vec<(u64, *mut OrcaHeader)>> {
        // Only follow edges within the local node set when restricted.
        if !self.is_local(actor_id) {
            return None;
        }
        let key = (actor_id, object as usize);

        // If the node is not in our graph, dead end.
        let edge_targets: Vec<(u64, *mut OrcaHeader)> = {
            let node = self.graph.get_mut(&key)?;

            match node.color {
                NodeColor::Black => {
                    return None;
                }
                NodeColor::Gray => {
                    let cycle_start = path
                        .iter()
                        .position(|(a, o)| *a == actor_id && *o == object)?;
                    let cycle = path[cycle_start..].to_vec();
                    return Some(cycle);
                }
                NodeColor::White => {
                    node.color = NodeColor::Gray;
                }
            }

            // Collect edge targets before recursing — avoids cloning the
            // full ForeignEdge vec (which includes ref_count, unused here).
            node.foreign_refs
                .iter()
                .map(|e| (e.target_actor, e.target_object))
                .collect()
        };
        // `node` borrow is dropped; safe to borrow `self.graph` again below.

        // Push current node onto the path.
        path.push((actor_id, object));

        // Explore each outgoing edge.
        for &(target_actor, target_object) in &edge_targets {
            if !self.is_local(target_actor) {
                continue;
            }
            let child_key = (target_actor, target_object as usize);

            if self.graph.contains_key(&child_key) {
                if let Some(cycle) = self.dfs_find_cycle(target_actor, target_object, path) {
                    return Some(cycle);
                }
            }
        }

        // All children explored — mark Black and backtrack.
        path.pop();
        if let Some(node) = self.graph.get_mut(&key) {
            node.color = NodeColor::Black;
        }

        None
    }

    // -- Trial decrement protocol ------------------------------------------

    /// Send trial decrements along a detected cycle.
    ///
    /// For each edge in the cycle, this constructs a `ForeignRefOp` with
    /// delta = -1 (a trial decrement). The caller must later either confirm
    /// (reclaim) or cancel (restore) these decrements.
    ///
    /// # Parameters
    ///
    /// - `cycle` — A sequence of `(actor_id, object_header)` tuples
    ///   forming a cycle. Each consecutive pair represents an edge.
    ///
    /// # Returns
    ///
    /// A vector of `ForeignRefOp` representing the trial decrements.
    fn send_trial_decrements(&mut self, cycle: &[(u64, *mut OrcaHeader)]) -> Vec<ForeignRefOp> {
        let mut ops = Vec::with_capacity(cycle.len());

        // For a cycle [A, B, C], we send decrements along edges A->B, B->C, C->A.
        for i in 0..cycle.len() {
            let (_from_actor, _from_object) = cycle[i];
            let (to_actor, to_object) = cycle[(i + 1) % cycle.len()];

            // SAFETY: We are constructing an operation, not yet applying it.
            // The header pointer is validated before any decrement is applied.
            let op = ForeignRefOp {
                target_actor: to_actor,
                owner_actor: to_actor,
                object_header: to_object as *mut crate::runtime::OrcaHeader,
                delta: -1,
            };
            ops.push(op);

            // Decrement the target's foreign count
            // to simulate the reference being dropped within the cycle.
            unsafe {
                // SAFETY: The objects were verified alive during DFS, and the
                // single scheduler thread is the only mutator of any header.
                let target_header = &mut *to_object;
                target_header.foreign_count -= 1;
            }
        }

        ops
    }

    /// Check whether all objects in a cycle have zero reference counts.
    ///
    /// This is called after trial decrements have been sent. If every
    /// object's total count (local + foreign) is zero, the cycle is
    /// unreachable garbage and can be reclaimed.
    ///
    /// # Parameters
    ///
    /// - `cycle` — The cycle to check.
    ///
    /// # Returns
    ///
    /// `true` if all objects in the cycle have zero total count.
    ///
    /// # Safety
    ///
    /// Dereferences object headers. All headers are verified alive.
    fn is_cycle_garbage(&self, cycle: &[(u64, *mut OrcaHeader)]) -> bool {
        for &(_actor, object) in cycle {
            // SAFETY: Object headers were validated during DFS and have not
            // been freed (trial decrements prevent concurrent reclamation).
            let (local, foreign) = unsafe {
                let header = &*object;
                (header.ref_count, header.foreign_count)
            };

            if local + foreign > 0 {
                // At least one object still has references — the cycle is
                // still reachable from outside.
                return false;
            }
        }
        true
    }

    /// Confirm trial decrements and reclaim all objects in a garbage cycle.
    ///
    /// This is called when `is_cycle_garbage` returns `true`. All objects
    /// in the cycle are freed and removed from the graph.
    ///
    /// # Parameters
    ///
    /// - `cycle` — The confirmed garbage cycle.
    /// - `runtime` — The runtime context for freeing objects.
    fn confirm_and_reclaim<R: CycleRuntime>(
        &mut self,
        cycle: &[(u64, *mut OrcaHeader)],
        runtime: &mut R,
    ) {
        self.cycles_found += 1;
        self.objects_reclaimed += cycle.len() as u64;

        for &(actor_id, object) in cycle {
            let key = (actor_id, object as usize);

            // Remove the node from the graph.
            self.graph.remove(&key);

            // SAFETY: `object` was verified alive during DFS and the trial
            // decrements proved it has no remaining references.
            unsafe {
                runtime.free_object(actor_id, object);
            }
        }
    }

    /// Cancel trial decrements for a cycle that is still reachable.
    ///
    /// This restores the reference counts that were temporarily decremented
    /// by `send_trial_decrements`. The cycle is not garbage — external
    /// references keep it alive — so we must undo the trial.
    ///
    /// # Parameters
    ///
    /// - `cycle` — The cycle whose trial decrements should be cancelled.
    fn cancel_trial_decrements(&mut self, cycle: &[(u64, *mut OrcaHeader)]) {
        // For each edge in the cycle, increment the counts back.
        for i in 0..cycle.len() {
            let (_from_actor, _from_object) = cycle[i];
            let (_to_actor, to_object) = cycle[(i + 1) % cycle.len()];

            // SAFETY: The object was alive during DFS and hasn't been freed
            // (we only free after confirming garbage), and the single
            // scheduler thread is the only mutator of any header.
            unsafe {
                let target_header = &mut *to_object;
                target_header.foreign_count += 1;
            }
        }

        // Mark nodes as Black so we don't re-examine them in this pass.
        for &(actor_id, object) in cycle {
            let key = (actor_id, object as usize);
            if let Some(node) = self.graph.get_mut(&key) {
                node.color = NodeColor::Black;
            }
        }
    }

    // -- Queries & configuration -------------------------------------------

    /// Check whether a full cycle detection should be triggered.
    ///
    /// Returns `true` when the current epoch aligns with the detection
    /// interval. The runtime can use this to decide whether to call
    /// `detect_cycles` (full pass) or just `incremental_detect`.
    pub fn should_detect(&self) -> bool {
        self.epoch % self.detection_interval == 0
    }

    /// Get cycle detector statistics.
    ///
    /// Returns a tuple of `(cycles_found, objects_reclaimed)`.
    pub fn stats(&self) -> (u64, u64) {
        (self.cycles_found, self.objects_reclaimed)
    }

    /// Update the suspect threshold.
    ///
    /// A lower threshold means fewer objects are flagged as suspects,
    /// reducing detection overhead but potentially missing cycles.
    /// A higher threshold casts a wider net, catching more potential
    /// cycles at the cost of increased detection work.
    ///
    /// # Parameters
    ///
    /// - `threshold` — New threshold value. Objects with `weight <= threshold`
    ///   will be flagged as suspects.
    pub fn set_threshold(&mut self, threshold: u32) {
        self.suspect_threshold = threshold;
    }

    /// Get the number of nodes currently in the foreign reference graph.
    ///
    /// Primarily useful for diagnostics and testing.
    pub fn graph_size(&self) -> usize {
        self.graph.len()
    }

    /// Get the number of suspects currently queued for investigation.
    ///
    /// Primarily useful for diagnostics and testing.
    pub fn suspect_queue_len(&self) -> usize {
        self.suspects.len()
    }

    /// Get the current epoch.
    ///
    /// Primarily useful for testing epoch progression.
    pub fn current_epoch(&self) -> u64 {
        self.epoch
    }
}

impl Default for CycleDetector {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
//  Unit Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod mock {
    //! Minimal mocks for testing the cycle detector in isolation.
    //!
    //! Since the cycle detector depends on `Runtime`, `OrcaHeader`, and
    //! heap operations, we provide mock versions that simulate the runtime
    //! environment without requiring the full system.

    use super::*;
    use crate::runtime::heap::{GcColor, SizeClass, TypeTag};
    use std::alloc::{alloc, dealloc, Layout};

    /// A mock runtime for testing.
    ///
    /// Provides minimal functionality needed by the cycle detector:
    /// - A registry of mock objects (actor-owned heap objects)
    /// - A queue of foreign reference operations to be processed
    /// - Statistics tracking for reclaimed objects
    #[allow(dead_code)]
    pub struct MockRuntime {
        /// Objects "owned" by each actor, indexed by actor_id.
        /// The outer Vec is indexed by actor_id; the inner Vec contains
        /// header pointers for that actor's objects.
        pub objects_by_actor: HashMap<u64, Vec<*mut OrcaHeader>>,
        /// Queue of foreign reference operations that the cycle detector
        /// would send to actors during trial decrements.
        pub pending_ops: Vec<ForeignRefOp>,
        /// Objects that have been reclaimed during testing.
        pub reclaimed: Vec<*mut OrcaHeader>,
    }

    impl MockRuntime {
        /// Create a new empty mock runtime.
        pub fn new() -> Self {
            Self {
                objects_by_actor: HashMap::new(),
                pending_ops: Vec::new(),
                reclaimed: Vec::new(),
            }
        }

        /// Create a mock object and register it as owned by the given actor.
        ///
        /// Allocates a real `OrcaHeader` on the heap so the cycle detector
        /// can safely dereference it during tests.
        ///
        /// # Parameters
        ///
        /// - `actor_id` — The actor that will own this object.
        /// - `local_refs` — Initial local reference count.
        /// - `foreign_refs` — Initial foreign reference count.
        ///
        /// # Returns
        ///
        /// A pointer to the newly allocated header.
        pub fn create_object(
            &mut self,
            actor_id: u64,
            local_refs: u32,
            foreign_refs: u32,
        ) -> *mut OrcaHeader {
            // Allocate a header on the heap.
            let layout = Layout::new::<OrcaHeader>();
            // SAFETY: Layout is valid for OrcaHeader.
            let ptr = unsafe { alloc(layout) as *mut OrcaHeader };
            assert!(!ptr.is_null(), "alloc failed");

            // SAFETY: ptr is valid and properly aligned. We zero the whole
            // header (including private padding) and then initialize every
            // public field through a raw pointer.
            unsafe {
                std::ptr::write_bytes(ptr, 0, 1);
                std::ptr::addr_of_mut!((*ptr).ref_count).write(local_refs);
                std::ptr::addr_of_mut!((*ptr).foreign_count).write(foreign_refs);
                std::ptr::addr_of_mut!((*ptr).sticky).write(false);
                (*ptr).size_class = SizeClass::Small;
                (*ptr).gc_color = GcColor::White;
                (*ptr).type_tag = TypeTag::Record;
                (*ptr).actor_id = actor_id;
                (*ptr).size = std::mem::size_of::<OrcaHeader>();
                (*ptr).live_next = std::ptr::null_mut();
                (*ptr).live_prev = std::ptr::null_mut();
            }

            self.objects_by_actor.entry(actor_id).or_default().push(ptr);

            ptr
        }

        /// Free a mock object and remove it from the actor's registry.
        ///
        /// # Safety
        ///
        /// `ptr` must have been allocated by `create_object` and not yet freed.
        pub unsafe fn free_object(&mut self, ptr: *mut OrcaHeader) {
            let header = &*ptr;
            let actor_id = header.actor_id;

            if let Some(objects) = self.objects_by_actor.get_mut(&actor_id) {
                objects.retain(|&p| p != ptr);
            }

            let layout = Layout::new::<OrcaHeader>();
            dealloc(ptr as *mut u8, layout);
        }
    }

    impl Default for MockRuntime {
        fn default() -> Self {
            Self::new()
        }
    }

    impl CycleRuntime for MockRuntime {
        unsafe fn free_object(&mut self, actor_id: u64, header: *mut OrcaHeader) {
            let header_actor = unsafe { (*header).actor_id };
            assert_eq!(header_actor, actor_id, "free_object actor_id mismatch");
            self.reclaimed.push(header);
            unsafe { self.free_object(header) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockRuntime;
    use super::*;

    // ------------------------------------------------------------------
    // Test 1: Basic graph construction
    // ------------------------------------------------------------------
    /// Verify that `register_foreign_ref` correctly builds the graph
    /// and `remove_foreign_ref` correctly tears it down.
    #[test]
    fn test_register_and_remove_ref() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        // Create two objects owned by different actors.
        let obj_a = runtime.create_object(1, 1, 0);
        let obj_b = runtime.create_object(2, 1, 0);

        // Register a foreign ref from actor 1's object to actor 2's object.
        detector.register_foreign_ref(1, obj_a, 2, obj_b);
        assert_eq!(detector.graph_size(), 1, "graph should have one node");

        // The node for obj_a should have one edge to obj_b.
        let key = (1, obj_a as usize);
        let node = detector.graph.get(&key).unwrap();
        assert_eq!(node.foreign_refs.len(), 1);
        assert_eq!(node.foreign_refs[0].target_actor, 2);
        assert_eq!(node.foreign_refs[0].ref_count, 1);

        // Remove the foreign ref.
        detector.remove_foreign_ref(1, obj_a, 2, obj_b);
        assert_eq!(
            detector.graph_size(),
            0,
            "graph should be empty after removal"
        );

        // Clean up.
        unsafe {
            runtime.free_object(obj_a);
            runtime.free_object(obj_b);
        }
    }

    // ------------------------------------------------------------------
    // Test 2: Simple 2-actor cycle detection
    // ------------------------------------------------------------------
    /// Verify that a simple cycle between two actors is detected.
    ///
    /// Setup: Actor 1 owns object A, Actor 2 owns object B.
    /// A references B (foreign ref), B references A (foreign ref).
    /// Both objects have only the cross-actor reference keeping them alive.
    ///
    /// Expected: The cycle detector should find the cycle.
    #[test]
    fn test_simple_cycle_detection() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        // Create objects: each has 0 local refs (not reachable from roots).
        let obj_a = runtime.create_object(1, 0, 0);
        let obj_b = runtime.create_object(2, 0, 0);

        // Set up foreign counts: each object has one foreign ref from the other.
        // SAFETY: obj_a and obj_b are valid headers.
        unsafe {
            (*obj_a).foreign_count = 1;
            (*obj_b).foreign_count = 1;
        }

        // A -> B (actor 1's object references actor 2's object)
        detector.register_foreign_ref(1, obj_a, 2, obj_b);
        // B -> A (actor 2's object references actor 1's object)
        detector.register_foreign_ref(2, obj_b, 1, obj_a);

        // Run full detection.
        detector.detect_cycles(&mut runtime);

        // A cycle should have been found.
        let (cycles, reclaimed) = detector.stats();
        assert_eq!(cycles, 1, "should find exactly one cycle");
        assert_eq!(
            reclaimed, 2,
            "both objects in the cycle should be reclaimed"
        );

        // Clean up any remaining objects.
        unsafe {
            if runtime
                .objects_by_actor
                .get(&1)
                .map_or(false, |v| v.contains(&obj_a))
            {
                runtime.free_object(obj_a);
            }
            if runtime
                .objects_by_actor
                .get(&2)
                .map_or(false, |v| v.contains(&obj_b))
            {
                runtime.free_object(obj_b);
            }
        }
    }

    // ------------------------------------------------------------------
    // Test 3: Weighted heuristic
    // ------------------------------------------------------------------
    /// Verify that weight is computed correctly as:
    /// weight = foreign_count - sum(outgoing_edge_ref_counts)
    #[test]
    fn test_weighted_heuristic() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        // Object with foreign_count = 3 and 3 outgoing edges of count 1 each.
        // weight = 3 - 3 = 0.
        let obj = runtime.create_object(1, 1, 3);
        let target1 = runtime.create_object(2, 1, 0);
        let target2 = runtime.create_object(3, 1, 0);
        let target3 = runtime.create_object(4, 1, 0);

        detector.register_foreign_ref(1, obj, 2, target1);
        detector.register_foreign_ref(1, obj, 3, target2);
        detector.register_foreign_ref(1, obj, 4, target3);

        // Refresh suspects to trigger weight computation.
        detector.refresh_suspects(&runtime);

        let key = (1, obj as usize);
        let node = detector.graph.get(&key).unwrap();
        assert_eq!(node.weight, 0, "weight should be 3 - 3 = 0");

        // Clean up.
        unsafe {
            runtime.free_object(obj);
            runtime.free_object(target1);
            runtime.free_object(target2);
            runtime.free_object(target3);
        }
    }

    // ------------------------------------------------------------------
    // Test 4: Suspect flagging
    // ------------------------------------------------------------------
    /// Verify that objects with low weight are flagged as suspects.
    #[test]
    fn test_suspect_flagging() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        // Object with foreign_count = 1 and 1 outgoing edge of count 1.
        // weight = 1 - 1 = 0 <= threshold(1) => should be flagged.
        let obj = runtime.create_object(1, 1, 1);
        let target = runtime.create_object(2, 1, 0);

        detector.register_foreign_ref(1, obj, 2, target);
        detector.refresh_suspects(&runtime);

        assert!(
            detector.suspect_queue_len() >= 1,
            "low-weight object should be flagged as suspect"
        );

        // Verify the suspect's properties.
        let suspect = detector.suspects.front().unwrap();
        assert_eq!(suspect.actor_id, 1);
        assert_eq!(suspect.object_header, obj);
        assert_eq!(suspect.weight, 0);

        unsafe {
            runtime.free_object(obj);
            runtime.free_object(target);
        }
    }

    // ------------------------------------------------------------------
    // Test 5: No false positives
    // ------------------------------------------------------------------
    /// Verify that an acyclic graph (tree structure) produces no suspects.
    ///
    /// Setup: Actor 1 -> Actor 2 -> Actor 3 (a chain, not a cycle).
    /// Each object has extra foreign references beyond what the graph tracks.
    #[test]
    fn test_no_false_positive() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        let obj1 = runtime.create_object(1, 2, 5); // high foreign count
        let obj2 = runtime.create_object(2, 1, 3);
        let obj3 = runtime.create_object(3, 1, 2);

        // Chain: 1 -> 2 -> 3 (no back edge, so no cycle).
        detector.register_foreign_ref(1, obj1, 2, obj2);
        detector.register_foreign_ref(2, obj2, 3, obj3);

        detector.refresh_suspects(&runtime);

        // With high foreign counts, weights should be high — no suspects.
        // weight(obj1) = 5 - 1 = 4 > threshold(1)
        // weight(obj2) = 3 - 1 = 2 > threshold(1)
        assert_eq!(
            detector.suspect_queue_len(),
            0,
            "acyclic graph with high foreign counts should produce no suspects"
        );

        unsafe {
            runtime.free_object(obj1);
            runtime.free_object(obj2);
            runtime.free_object(obj3);
        }
    }

    // ------------------------------------------------------------------
    // Test 6: Trial decrement protocol
    // ------------------------------------------------------------------
    /// Verify that `send_trial_decrements` produces the correct set of
    /// decrement operations for a given cycle.
    #[test]
    fn test_trial_decrement() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        let obj_a = runtime.create_object(1, 0, 1);
        let obj_b = runtime.create_object(2, 0, 1);

        let cycle = vec![(1, obj_a), (2, obj_b)];
        let ops = detector.send_trial_decrements(&cycle);

        // For a 2-node cycle, we expect 2 decrement operations.
        assert_eq!(ops.len(), 2, "2-node cycle should produce 2 decrements");

        // The decrements should alternate: A's ref to B, then B's ref to A.
        assert_eq!(ops[0].target_actor, 2); // edge from A to B
        assert_eq!(ops[0].delta, -1);
        assert_eq!(ops[1].target_actor, 1); // edge from B to A
        assert_eq!(ops[1].delta, -1);

        unsafe {
            runtime.free_object(obj_a);
            runtime.free_object(obj_b);
        }
    }

    // ------------------------------------------------------------------
    // Test 7: Full cycle reclamation
    // ------------------------------------------------------------------
    /// Verify that a detected garbage cycle is fully reclaimed.
    #[test]
    fn test_cycle_reclamation() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        // Create a cycle of 3 objects, each with only cross-references.
        let obj1 = runtime.create_object(1, 0, 1);
        let obj2 = runtime.create_object(2, 0, 1);
        let obj3 = runtime.create_object(3, 0, 1);

        // Cycle: 1 -> 2 -> 3 -> 1
        detector.register_foreign_ref(1, obj1, 2, obj2);
        detector.register_foreign_ref(2, obj2, 3, obj3);
        detector.register_foreign_ref(3, obj3, 1, obj1);

        // All counts are zero (no external refs) — should be reclaimed.
        detector.detect_cycles(&mut runtime);

        let (cycles, reclaimed) = detector.stats();
        assert_eq!(cycles, 1, "should find one cycle");
        assert_eq!(reclaimed, 3, "should reclaim all 3 objects");

        // Graph should be empty after reclamation.
        assert_eq!(detector.graph_size(), 0);

        // Objects were reclaimed by the detector; do not free them again.
    }

    // ------------------------------------------------------------------
    // Test 8: Self-cycle (same actor)
    // ------------------------------------------------------------------
    /// Verify that a self-reference within the same actor is NOT tracked
    /// by the foreign reference graph (and therefore not detected here).
    ///
    /// Self-cycles are the responsibility of the per-actor GC, not the
    /// centralized cycle detector.
    #[test]
    fn test_self_cycle() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        let obj_a = runtime.create_object(1, 1, 1);
        let obj_b = runtime.create_object(1, 1, 1);

        // Both objects are owned by actor 1 — this is NOT a foreign ref.
        detector.register_foreign_ref(1, obj_a, 1, obj_b);

        // The self-ref should be ignored (from_actor == to_actor).
        assert_eq!(detector.graph_size(), 0);

        unsafe {
            runtime.free_object(obj_a);
            runtime.free_object(obj_b);
        }
    }

    // ------------------------------------------------------------------
    // Test 9: Complex cycle (3+ actors)
    // ------------------------------------------------------------------
    /// Verify that a cycle involving 4 actors is correctly detected.
    #[test]
    fn test_complex_cycle() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        let obj1 = runtime.create_object(1, 0, 1);
        let obj2 = runtime.create_object(2, 0, 1);
        let obj3 = runtime.create_object(3, 0, 1);
        let obj4 = runtime.create_object(4, 0, 1);

        // Cycle: 1 -> 2 -> 3 -> 4 -> 1
        detector.register_foreign_ref(1, obj1, 2, obj2);
        detector.register_foreign_ref(2, obj2, 3, obj3);
        detector.register_foreign_ref(3, obj3, 4, obj4);
        detector.register_foreign_ref(4, obj4, 1, obj1);

        detector.detect_cycles(&mut runtime);

        let (cycles, reclaimed) = detector.stats();
        assert_eq!(cycles, 1, "should find one 4-actor cycle");
        assert_eq!(reclaimed, 4, "should reclaim all 4 objects");

        // Objects were reclaimed by the detector; do not free them again.
    }

    // ------------------------------------------------------------------
    // Test 10: Incremental detection
    // ------------------------------------------------------------------
    /// Verify that `incremental_detect` processes suspects one at a time.
    #[test]
    fn test_incremental_detection() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        // With an empty graph, incremental_detect should advance the epoch
        // but do nothing else.
        let epoch_before = detector.current_epoch();
        detector.incremental_detect(&mut runtime);
        assert_eq!(detector.current_epoch(), epoch_before + 1);
    }

    // ------------------------------------------------------------------
    // Test 11: Threshold adjustment
    // ------------------------------------------------------------------
    /// Verify that changing the suspect threshold affects how many
    /// objects are flagged as suspects.
    #[test]
    fn test_threshold_adjustment() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        // Object with weight = 2.
        let obj = runtime.create_object(1, 1, 3);
        let target = runtime.create_object(2, 1, 0);

        detector.register_foreign_ref(1, obj, 2, target);

        // With threshold = 1, weight(2) > 1 => not a suspect.
        detector.set_threshold(1);
        detector.refresh_suspects(&runtime);
        assert_eq!(detector.suspect_queue_len(), 0, "weight=2 > threshold=1");

        // With threshold = 3, weight(2) <= 3 => IS a suspect.
        detector.set_threshold(3);
        detector.refresh_suspects(&runtime);
        assert_eq!(detector.suspect_queue_len(), 1, "weight=2 <= threshold=3");

        unsafe {
            runtime.free_object(obj);
            runtime.free_object(target);
        }
    }

    // ------------------------------------------------------------------
    // Test 12: Epoch progression
    // ------------------------------------------------------------------
    /// Verify that epochs advance correctly through multiple calls.
    #[test]
    fn test_epoch_progression() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        assert_eq!(detector.current_epoch(), 0);

        for expected in 1..=10 {
            detector.incremental_detect(&mut runtime);
            assert_eq!(
                detector.current_epoch(),
                expected,
                "epoch should advance by 1 each call"
            );
        }
    }

    // ------------------------------------------------------------------
    // Test 13: Edge ref count accumulation
    // ------------------------------------------------------------------
    /// Verify that multiple registrations between the same pair of objects
    /// accumulate the ref_count rather than creating duplicate edges.
    #[test]
    fn test_edge_ref_count_accumulation() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        let obj_a = runtime.create_object(1, 1, 0);
        let obj_b = runtime.create_object(2, 1, 0);

        // Register the same foreign ref three times.
        detector.register_foreign_ref(1, obj_a, 2, obj_b);
        detector.register_foreign_ref(1, obj_a, 2, obj_b);
        detector.register_foreign_ref(1, obj_a, 2, obj_b);

        let key = (1, obj_a as usize);
        let node = detector.graph.get(&key).unwrap();
        assert_eq!(node.foreign_refs.len(), 1, "should be a single edge");
        assert_eq!(
            node.foreign_refs[0].ref_count, 3,
            "ref_count should be accumulated to 3"
        );

        // Remove one ref.
        detector.remove_foreign_ref(1, obj_a, 2, obj_b);
        let node = detector.graph.get(&key).unwrap();
        assert_eq!(node.foreign_refs[0].ref_count, 2);

        // Remove remaining two.
        detector.remove_foreign_ref(1, obj_a, 2, obj_b);
        detector.remove_foreign_ref(1, obj_a, 2, obj_b);
        assert_eq!(detector.graph_size(), 0, "graph should be empty");

        unsafe {
            runtime.free_object(obj_a);
            runtime.free_object(obj_b);
        }
    }

    // ------------------------------------------------------------------
    // Test 14: should_detect interval
    // ------------------------------------------------------------------
    /// Verify that `should_detect` returns true at the configured interval.
    #[test]
    fn test_should_detect_interval() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        // Default interval is 10.
        for _ in 0..9 {
            detector.incremental_detect(&mut runtime);
            assert!(
                !detector.should_detect(),
                "should_detect should be false before interval"
            );
        }

        // 10th call: should_detect becomes true.
        detector.incremental_detect(&mut runtime);
        assert!(
            detector.should_detect(),
            "should_detect should be true at interval"
        );
    }

    // ------------------------------------------------------------------
    // Test 15: Cycle with external reference (not garbage)
    // ------------------------------------------------------------------
    /// Verify that a cycle with an external reference is NOT reclaimed.
    ///
    /// Setup: Objects A and B form a cycle, but A also has a local ref.
    /// After trial decrements, A still has a local ref, so the cycle
    /// is still reachable and should not be reclaimed.
    #[test]
    fn test_cycle_with_external_ref() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        let obj_a = runtime.create_object(1, 1, 1); // 1 local ref = external
        let obj_b = runtime.create_object(2, 0, 1);

        // Cycle: A <-> B
        detector.register_foreign_ref(1, obj_a, 2, obj_b);
        detector.register_foreign_ref(2, obj_b, 1, obj_a);

        detector.detect_cycles(&mut runtime);

        // The cycle should NOT be reclaimed because A has a local ref.
        let (cycles, reclaimed) = detector.stats();
        assert_eq!(cycles, 0, "should not find reclaimable cycle");
        assert_eq!(reclaimed, 0, "should not reclaim any objects");

        unsafe {
            runtime.free_object(obj_a);
            runtime.free_object(obj_b);
        }
    }

    /// Verify that the intra-node restriction prevents the detector from
    /// following edges to remote actors.
    #[test]
    fn test_intra_node_restriction_skips_remote_actors() {
        let mut detector = CycleDetector::new();
        let mut runtime = MockRuntime::new();

        let obj_a = runtime.create_object(1, 0, 1); // no local refs, 1 foreign ref
        let obj_b = runtime.create_object(2, 0, 1);

        // Cycle: A <-> B
        detector.register_foreign_ref(1, obj_a, 2, obj_b);
        detector.register_foreign_ref(2, obj_b, 1, obj_a);

        // Without the restriction, the cycle would be reclaimed.
        detector.detect_cycles(&mut runtime);
        let (cycles_before, reclaimed_before) = detector.stats();
        assert_eq!(
            cycles_before, 1,
            "cycle should be found without restriction"
        );
        assert_eq!(
            reclaimed_before, 2,
            "both objects should be reclaimed without restriction"
        );

        // Create a fresh detector and restrict to actor 1 only.
        let mut detector2 = CycleDetector::new();
        let mut runtime2 = MockRuntime::new();
        let obj_a2 = runtime2.create_object(1, 0, 1);
        let obj_b2 = runtime2.create_object(2, 0, 1);
        detector2.register_foreign_ref(1, obj_a2, 2, obj_b2);
        detector2.register_foreign_ref(2, obj_b2, 1, obj_a2);

        let mut local = std::collections::HashSet::new();
        local.insert(1);
        detector2.set_local_actors(local);

        detector2.detect_cycles(&mut runtime2);
        let (cycles_after, reclaimed_after) = detector2.stats();
        assert_eq!(
            cycles_after, 0,
            "remote actor should be excluded from detection"
        );
        assert_eq!(reclaimed_after, 0, "remote objects should not be reclaimed");

        unsafe {
            // runtime objects were reclaimed by the detector.
            // runtime2 objects were not reclaimed, so free them manually.
            runtime2.free_object(obj_a2);
            runtime2.free_object(obj_b2);
        }
    }
}
