//! Actor runtime system for Nulang.
//!
//! Provides: actor lifecycle, scheduler, mailbox, heap, GC, supervision,
//! distribution.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;

mod actor;
mod gc;
pub mod heap;
pub(crate) mod heap_serialize;
mod mailbox;
mod scheduler;
pub use heap_serialize::*;
mod cluster;
mod distributed;
mod distributed_context;
mod grain;
mod network;
mod object_store;
mod orca_cycle;
mod supervision;
mod supervisor;
use distributed_context::DistributedContext;
#[cfg(feature = "ai-runtime")]
mod agent;
#[cfg(feature = "ai-runtime")]
mod ai_impls;
pub(crate) mod callbacks;
pub mod crdt;
pub mod crdt_manager;
pub mod crdt_reg;
mod distribution;
mod exit;
mod http_server;
#[cfg(feature = "ai-runtime")]
mod llm;
mod metrics;
mod persistence;
mod process_groups;
mod registry;
mod spawn;
mod timer;
mod trace;
mod workflow;
pub use trace::TraceContext;

#[cfg(test)]
mod cluster_dst;

#[cfg(test)]
mod cluster_sim;

#[cfg(test)]
mod tests;

pub use actor::*;
pub use callbacks::RuntimeVmCallbacks;
pub(crate) use callbacks::{BytecodeDistributedCallbacks, BytecodeRuntimeCallbacks};
pub use cluster::*;
pub use crdt::*;
pub use crdt_manager::*;
pub use crdt_reg::{LWWRegister, MVRegister, RGAElement, RGA};
pub use distributed::*;
pub use gc::{ForeignRefOp, GcStats, OrcaCoordinator, OrcaGc, OrcaHeap};
pub use grain::*;
pub use heap::*;
pub use http_server::{render_route_handler, HttpServerState, WebDevServer, WebRoute};
pub use mailbox::*;
pub use network::NetworkTransport;
pub use network::*;
pub use object_store::*;
pub use orca_cycle::*;
pub use persistence::*;
pub use process_groups::*;
pub use registry::*;
pub use scheduler::*;
pub use supervisor::*;
pub use timer::*;

use crate::types::{ExitReason, NuError, Span, VmSuspension};
use crate::vm::Value;

#[cfg(feature = "ai-runtime")]
use nulang_ai::{
    AiRuntimeRegistry, LlmClient, LlmError, LlmMessage, LlmRequest, LlmResponse,
    SupervisorTeamRegistry,
};

// ---------------------------------------------------------------------------
// Global actor ID generator
// ---------------------------------------------------------------------------

static ACTOR_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a fresh, globally unique actor ID.
pub fn fresh_actor_id() -> u64 {
    ACTOR_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Sentinel actor id for `Runtime::main_heap`/`main_gc`, the fallback used
/// for allocation outside any real actor's behavior. Below
/// `ACTOR_ID_COUNTER`'s start value of 1, so it can never collide with a
/// real `fresh_actor_id()` result.
const MAIN_HEAP_ACTOR_ID: u64 = 0;

/// Maximum number of membership entries carried by a single gossip packet.
const GOSSIP_PAYLOAD_MAX_ENTRIES: usize = 256;

/// Native handler for durable workflow timer-fired messages.
///
/// Advances the workflow's step_index so the workflow can proceed past the
/// step that was waiting on the timer.
fn timer_fired_handler(actor: &mut Actor, _args: &[Value]) {
    if let Some(n) = actor.get_state_field("step_index").and_then(|v| v.as_int()) {
        actor.set_state_field("step_index", Value::int(n + 1));
    }
}

/// Placeholder native handler for bytecode workflow steps.
///
/// Workflow steps are dispatched via `bytecode_offsets`, but the behavior-id
/// space is shared with native handlers. Empty-name placeholders reserve the
/// step ids so internal runtime behaviors (e.g. `__timer_fired`) can live at
/// higher indices without colliding.
fn bytecode_step_placeholder(_actor: &mut Actor, _args: &[Value]) {}

/// Persisted `waiting_signal` marker for a workflow step suspended on a
/// background LLM call.  A signal wait stores the awaited signal's name so
/// recovery can re-trigger the in-flight step; an LLM suspend has no
/// signal, so this reserved marker plays the same role.  The suspended VM
/// state itself cannot be persisted, so recovery re-runs the step from
/// its last pre-suspend checkpoint and the re-executed `LLM.ask` starts
/// a fresh background call.
const LLM_SUSPEND_MARKER: &str = "__llm_ask_pending__";

/// How often (in scheduler ticks) the runtime scans resident grain actors for
/// dehydration.
const DEHYDRATE_CHECK_INTERVAL: u64 = 50;

/// Choose the `waiting_signal` value for a freshly captured suspension:
/// the awaited signal's name for a signal wait, or the reserved LLM
/// marker for a workflow step suspended on a background LLM call (plain
/// actors store nothing; their suspensions are not re-driven on
/// recovery).
fn suspension_marker(actor: &Actor, signal_name: Option<String>) -> Option<String> {
    match signal_name {
        Some(name) => Some(name),
        None if actor.is_workflow => Some(LLM_SUSPEND_MARKER.to_string()),
        None => None,
    }
}

/// Map the argument of `perform Actor.exit(reason)` onto an `ExitReason`.
/// Ints and strings select the reason kind (`0`/`"normal"`, `1`/`"error"`,
/// `2`/`"kill"`); any other value is a custom reason, and a missing or
/// non-int/non-string argument defaults to a normal exit.
fn actor_exit_reason(value: Option<&Value>, constants: &[crate::bytecode::Constant]) -> ExitReason {
    let Some(value) = value else {
        return ExitReason::Normal;
    };
    if let Some(n) = value.as_int() {
        return match n {
            0 => ExitReason::Normal,
            1 => ExitReason::Error("error".to_string()),
            2 => ExitReason::Kill,
            other => ExitReason::Custom(other.to_string()),
        };
    }
    if let Some(id) = value.as_string_id() {
        let name = match constants.get(id as usize) {
            Some(crate::bytecode::Constant::String(s)) => s.as_str(),
            _ => "",
        };
        return match name {
            "normal" => ExitReason::Normal,
            "error" => ExitReason::Error("error".to_string()),
            "kill" => ExitReason::Kill,
            other => ExitReason::Custom(other.to_string()),
        };
    }
    ExitReason::Normal
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Cross-shard message type for multi-threaded scheduler
// ---------------------------------------------------------------------------

/// A message routed between Runtime shards in a multi-threaded deployment.
///
/// Each shard owns a disjoint subset of actors (by `actor_id % shard_count`).
/// Cross-shard messages carry only value-type payloads (ints, strings, bools,
/// unit, nil) - heap pointers are stripped before sending, matching the
/// network wire-protocol restriction. This keeps ORCA reference counting
/// local to each shard.
#[derive(Debug)]
enum CrossShardMsg {
    /// Deliver a message to an actor on the target shard.  For grain targets
    /// the `grain_id` is included so the receiving shard can hydrate a grain
    /// that has never been resident there.
    DeliverMessage {
        target_id: u64,
        behavior_id: u16,
        payload: Vec<Value>,
        sender: u64,
        trace_id: Option<String>,
        grain_id: Option<GrainId>,
    },
    /// Deliver a message whose payload contains object-store refs.  The bytes
    /// are copied because each shard owns a separate `ObjectStore`.
    DeliverMessageWithObjects {
        target_id: u64,
        behavior_id: u16,
        payload: Vec<Value>,
        /// Object ids referenced in `payload` and their byte contents.
        objects: Vec<(crate::runtime::object_store::ObjectId, Vec<u8>)>,
        sender: u64,
        trace_id: Option<String>,
        grain_id: Option<GrainId>,
    },
    /// Enqueue an actor on the target shard (wake from idle/waiting).
    EnqueueActor {
        actor_id: u64,
        priority: ActorPriority,
    },
}

pub struct Runtime {
    pub actors: HashMap<u64, Actor>,
    pub supervisors: HashMap<u64, Supervisor>,
    pub scheduler: Scheduler,
    pub current_actor: Option<u64>,
    /// W3C trace context of the message currently being handled on this
    /// shard's scheduler thread. Sends performed while handling a message
    /// stamp their outgoing `traceparent` as a child of this context, so
    /// causal chains span actor, shard, and node boundaries.
    pub current_trace: Option<TraceContext>,
    // Fallback heap/GC for allocation performed OUTSIDE any actor's
    // behavior (e.g. `main()`'s own top-level bytecode: string
    // concatenation, `Int.to_string`, and similar). See
    // `RuntimeVmCallbacks::alloc`'s doc comment for why this exists.
    pub main_heap: ActorHeap,
    pub main_gc: OrcaGc,
    pub next_reductions: u32,
    pub coordinator: OrcaCoordinator,
    pub cycle_detector: CycleDetector,

    // Heaps of exited actors that still have outstanding foreign
    // references.  Dropping a heap while another actor holds a pointer
    // into it would dangle, so such heaps are retired here instead and
    // reclaimed by `reclaim_retired_heaps` once every foreign reference
    // (in-flight op or receiver hold) has drained.
    retired_heaps: Vec<ActorHeap>,

    // Distributed actor system (v0.5)
    pub distributed: DistributedContext,
    // Operator cluster configuration (split-brain resolver, probe interval),
    // applied when distribution is enabled.
    pub cluster_config: ClusterConfig,
    // Acknowledged packet sequence numbers (transport-level reliability).
    pub acked_packets: HashSet<u64>,

    // Cross-node supervision (RFC 0012)
    pub remote_links: supervision::RemoteLinkRegistry,
    pub remote_monitors: supervision::RemoteMonitorRegistry,

    /// Actors that have migrated to another node.  Key is the local
    /// actor id (the id they had here before migrating); value is
    /// `(target_node, migrated_at)`.  `send_message_by_id` checks this
    /// table and forwards messages to the new location.  Entries are
    /// garbage-collected after `MIGRATED_ACTOR_TTL` seconds.
    pub migrated_actors: HashMap<u64, (NodeId, Instant)>,

    /// Durable actors opted into `RespawnOnNodeLoss` (RFC 0014): actor id →
    /// current activation epoch. Enables shadow replication at checkpoint
    /// time and directory announcement.
    pub(crate) respawn_opted: HashMap<u64, u64>,
    /// Shadow replicas this node holds (RFC 0014 §3): actor id → replica,
    /// received via `Packet::ShadowReplicate` from the actor's home node.
    /// Consumed by the re-spawn driver when the home node is confirmed gone.
    pub(crate) shadow_replicas: HashMap<u64, ShadowReplica>,

    // CRDT manager (v0.6)
    pub crdt_manager: Option<CrdtManager>,

    // Number of `sync_crdts` calls made; delta-state syncs run on most
    // rounds, with a full-state repair sync every CRDT_FULL_SYNC_INTERVAL.
    pub(crate) crdt_sync_rounds: u64,

    // Timer wheel (v0.7)
    pub timer_wheel: TimerWheel,
    // Virtual clock for deterministic testing (v0.14). When set, all timer
    // expiry and deadline calculations use this clock instead of wall time.
    pub virtual_clock: Option<VirtualClock>,
    /// Prometheus-format metrics server (background TCP listener).
    /// Started via [`Runtime::enable_metrics_server`]; periodically
    /// updated via [`Runtime::publish_metrics`].
    pub metrics: Option<metrics::MetricsServer>,
    /// Python foreign-interop bridge (feature `python`).  Lazily initialised
    /// on first `Python.*` builtin effect invocation.
    #[cfg(feature = "python")]
    pub foreign_interop: Option<Box<dyn crate::backends::ForeignInterop>>,
    // LLM subsystem (v0.9 AI Runtime): client, worker thread, token budget,
    // completion channel, and non-blocking suspension state.
    #[cfg(feature = "ai-runtime")]
    pub llm: llm::LlmState,

    // Actor name registry (v0.7)
    pub registry: ActorRegistry,

    // Process groups (v0.7)
    pub process_groups: ProcessGroups,

    // Persistence engine (v0.7)
    pub persistence: Box<dyn PersistenceStore>,
    // Immutable shared object store for large `val` buffers.
    pub object_store: ObjectStore,
    // Virtual actor (grain) type registry and resident mapping.
    pub grain_registry: GrainRegistry,
    pub grain_residents: HashMap<GrainId, u64>,
    pub actor_grain_id: HashMap<u64, GrainId>,
    // Known grain actor ids -> GrainId, populated when a grain is first
    // hydrated.  Used to wake/dehydrate resident grains and to re-hydrate
    // a grain addressed by its stable actor id.
    pub grain_actor_ids: HashMap<u64, GrainId>,

    // VM used to execute bytecode behavior handlers.
    vm: Option<crate::vm::VM>,

    // Depth of in-flight calls on the shared runtime VM
    // (`run_bytecode_at_offset`, `resume_suspended_*`). While > 0 a
    // behavior is mid-execution, so receive-wait wakes requested by
    // `send_message_by_id` must be deferred: resuming the target would
    // nest a second `vm.resume()`/`run_from` inside the running one and
    // clobber the shared frames.
    vm_execution_depth: u32,

    /// True while executing a scheduler-driven bytecode behavior, enabling
    /// non-blocking suspension on `perform LLM.ask` and on `receive ...
    /// after ms =>` timed waits. Nested synchronous entry points
    /// (`ask_actor_sync`: pipelines, supervisors, debates) force it back
    /// to false so they keep blocking behavior. Not LLM-specific - lives
    /// on `Runtime` directly (not `LlmState`) because core receive-wait
    /// suspension depends on it regardless of the `ai-runtime` feature.
    pub(crate) suspend_enabled: bool,

    // Actors whose receive-wait wake was deferred while the shared VM was
    // executing (deduplicated). Drained by `vm_exec_end` once the
    // outermost VM call returns; a resumed behavior can itself send and
    // re-queue a wake, so the drain loops until empty.
    pending_receive_wakes: Vec<u64>,

    // True while `vm_exec_end` is draining `pending_receive_wakes`. Nested
    // `vm_exec_end` calls (from resumes issued by the drain) then skip
    // their own drain, so the backlog is processed iteratively instead of
    // by unbounded recursion.
    draining_receive_wakes: bool,

    // Bytecode modules for actors that may need to be recovered after a
    // runtime restart.  Maps actor_id -> (bytecode_module, behavior_offsets,
    // compensation_offsets).
    pub(crate) recovery_modules:
        HashMap<u64, (crate::bytecode::CodeModule, Vec<usize>, Vec<Option<usize>>)>,
    /// Content-addressed bytecode cache for fetch-on-demand.
    /// When a node receives a message for an unknown content hash, it can
    /// request the bytecode from the sender and cache it here keyed by hash.
    pub behavior_cache: HashMap<[u8; 32], crate::bytecode::CodeModule>,
    /// Messages pending retry after a bytecode fetch completes.
    /// Keyed by content hash; drained when the matching FetchBehaviorResponse
    /// arrives and the module is cached.
    pub(crate) pending_fetched_messages:
        HashMap<[u8; 32], Vec<(u64, String, Message, Vec<String>, Vec<(u64, Vec<u8>)>)>>,
    // Pipelines and debates (v0.9 AI Runtime) - extracted into a registry so
    // the god-object shrinks and the subsystems can evolve independently.
    #[cfg(feature = "ai-runtime")]
    pub ai: AiRuntimeRegistry,
    // Supervisor teams (v0.9 AI Runtime) - extracted into a registry so the
    // god-object shrinks and the subsystem can evolve independently.
    #[cfg(feature = "ai-runtime")]
    pub supervisor_teams: SupervisorTeamRegistry,

    // Remote spawn support (v0.5+): behaviors a remote node may spawn here
    // by name (see `register_spawnable_behavior`), plus the results of
    // spawn requests WE issued, keyed by request id
    // (`Some(actor_id)` = spawned, `None` = rejected).
    pub spawnable_behaviors: HashMap<String, fn(&mut Actor, &[Value])>,
    pub pending_spawn_responses: HashMap<u64, Option<u64>>,
    /// Bare actor id → hosting node for every remote actor this runtime
    /// can address BY VALUE (RFC-0007 cross-node routing): spawn@node
    /// placeholders (request id → target node, recorded at spawn time)
    /// and inbound senders recorded for reply-by-ref. Actor-ref Values
    /// carry only a 48-bit id — no node — so `send`/`ask` on a bare ref
    /// consult this index to decide wire vs local routing. Scheduler-
    /// thread confined like the rest of the distributed state. Bounded
    /// at `REMOTE_REFS_MAX`; when full, new entries are dropped (the
    /// forward `RemoteActorCache` still covers explicit
    /// `ActorAddress::remote` sends).
    pub(crate) remote_refs: HashMap<u64, NodeId>,
    /// Messages sent to a spawn@node placeholder before its SpawnResponse
    /// arrived, pre-resolved to wire form (string payloads rewritten to
    /// table indices + contents captured in `string_table`) so the flush
    /// on SpawnResponse doesn't need the sender's module-pool context.
    pub(crate) pending_spawn_messages: HashMap<u64, Vec<distribution::PendingSpawnMessage>>,
    /// Value ids that are spawn@node PLACEHOLDERS (request ids) whose
    /// SpawnResponse has not arrived. Distinct from `remote_refs`: a
    /// placeholder must QUEUE messages until its real actor id is known,
    /// while an ordinary remote ref (inbound sender, real spawned id)
    /// sends directly. Removed on SpawnResponse (success or failure).
    pub(crate) spawn_placeholders: HashSet<u64>,
    /// Placeholder VALUE id → real remote actor id, recorded on a
    /// successful SpawnResponse. Independent of `pending_spawn_responses`
    /// (consumed by `take_spawn_response`): the placeholder ref the
    /// program holds must keep routing to the real actor even after the
    /// response was observed (or not) by application code.
    pub(crate) spawn_translations: HashMap<u64, u64>,
    /// AOT-compiled modules registered for native behavior dispatch, keyed by
    /// actor type name → module pointer. Ownership lives in
    /// `aot_module_storage`; the pointers are stable (each module is Boxed).
    pub aot_modules: std::collections::HashMap<String, *const crate::aot::AotModule>,
    /// Owns the registered AOT modules so the raw pointers in `aot_modules`
    /// (and on actors) stay valid for the Runtime's lifetime.
    pub aot_module_storage: Vec<Box<crate::aot::AotModule>>,
    /// Actor ID of the dead-letter queue (created lazily).
    /// Undeliverable messages are routed here.
    pub dlq_actor_id: Option<u64>,
    /// Callback invoked when the scheduler loop reaches true quiescence
    /// (empty run queue, no inflight LLM calls, no pending timers).
    /// The embedder (e.g. NLC guest agent) wires this to host signaling.
    pub idle_callback: Option<Box<dyn FnMut()>>,
    // Test effect handlers - installed via `install_test_handler` to
    // intercept `perform Effect.op` calls in tests.  Key is the qualified
    // name (e.g. "IO.print", "DB.write").  A handler returns `Some(value)`
    // to mock the effect or `None` to fall through to real dispatch.
    // HTTP server state (v0.7+).
    pub http_server: Option<HttpServerState>,
    pub test_handlers: HashMap<String, Box<dyn Fn(&[Value]) -> Option<Value>>>,
    /// Cryptographic provider (hashing, random, signing).
    /// Defaults to [`crate::backends::DefaultCryptoProvider`].
    pub crypto: Box<dyn crate::backends::CryptoProvider>,

    /// HTTP provider for outbound requests (health checks, webhooks, etc.).
    /// Defaults to [`crate::backends::ReqwestHttpProvider`].
    #[cfg(any(feature = "ai-runtime", feature = "http-client"))]
    pub http: Box<dyn crate::backends::HttpProvider>,

    /// TLS provider for network encryption.
    /// Defaults to [`crate::backends::DefaultTlsProvider`] when TLS feature is enabled.
    #[cfg(feature = "tls")]
    pub tls_provider: Box<dyn crate::backends::TlsProvider>,
    // -- Multi-threaded scheduler sharding --
    /// This shard's index (0-based). Always 0 for a single-shard runtime.
    pub shard_idx: u16,
    /// Total number of shards. Always 1 for a single-shard runtime.
    pub shard_count: u16,
    /// Channels to send messages to every shard (including self - unused).
    /// `None` when `shard_count == 1` (single-shard, no cross-shard routing).
    cross_shard_tx: Option<Vec<mpsc::SyncSender<CrossShardMsg>>>,
    /// Channel to receive messages for this shard.
    /// `None` when `shard_count == 1`.
    cross_shard_rx: Option<mpsc::Receiver<CrossShardMsg>>,
}

// SAFETY: in sharded mode each Runtime runs on exactly one thread (shard
// ownership by actor_id % shard_count). Cross-shard communication uses
// mpsc channels; no two threads access the same Runtime's internal state.
// The contained VM, callback trait objects, and raw ORCA pointers are all
// thread-confined.
unsafe impl Send for Runtime {}

/// Outcome of `Runtime::run_scheduler_deterministic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeterministicRunResult {
    /// No actor has a non-empty mailbox; the run completed normally.
    Quiescent { steps: u64 },
    /// `max_steps` was reached with actors still having pending
    /// messages -- a real invariant violation (deadlock/livelock),
    /// since every step executed real actor code via `step_actor`, not
    /// a simulated stand-in.
    StepLimitExceeded { steps: u64 },
}

/// No-op foreign-interop bridge used when `DefaultForeignInterop` fails
/// to initialise (e.g. missing Python runtime).  Every call returns an
/// error, causing `perform_python_builtin` to emit a nil result.
#[cfg(feature = "python")]
struct NoOpForeignInterop;

#[cfg(feature = "python")]
impl crate::backends::ForeignInterop for NoOpForeignInterop {
    fn call(&mut self, _module: &str, _function: &str, _args: &[Value]) -> Result<Value, String> {
        Err("Python bridge not available".to_string())
    }
    fn import(&mut self, _name: &str) -> Result<(), String> {
        Err("Python bridge not available".to_string())
    }
}

impl Runtime {
    pub fn new() -> Self {
        Runtime {
            actors: HashMap::new(),
            supervisors: HashMap::new(),
            scheduler: Scheduler::new(4),
            current_actor: None,
            current_trace: None,
            main_heap: {
                let mut heap = ActorHeap::new(64 * 1024);
                heap.set_actor_id(MAIN_HEAP_ACTOR_ID);
                heap
            },
            main_gc: OrcaGc::new(MAIN_HEAP_ACTOR_ID),
            next_reductions: 1000,
            coordinator: OrcaCoordinator::new(),
            cycle_detector: CycleDetector::new(),
            vm_execution_depth: 0,
            suspend_enabled: false,
            retired_heaps: Vec::new(),
            distributed: DistributedContext::new(),
            cluster_config: ClusterConfig::default(),
            acked_packets: HashSet::new(),
            remote_links: supervision::RemoteLinkRegistry::new(),
            remote_monitors: supervision::RemoteMonitorRegistry::new(),
            migrated_actors: HashMap::new(),
            respawn_opted: HashMap::new(),
            shadow_replicas: HashMap::new(),
            // Standalone runtimes use node id 0; `enable_distribution` swaps
            // this for a real node-id manager. Initialized eagerly so
            // `state crdt` fields register and `Crdt.*` ops work without
            // distribution enabled.
            crdt_manager: Some(CrdtManager::new(0)),
            virtual_clock: None,
            metrics: None,
            #[cfg(feature = "python")]
            foreign_interop: None,
            crdt_sync_rounds: 0,
            timer_wheel: TimerWheel::new(),
            registry: ActorRegistry::new(),
            process_groups: ProcessGroups::new(),
            pending_fetched_messages: HashMap::new(),
            persistence: Box::new(MemoryStore::new()),
            object_store: ObjectStore::new(),
            grain_registry: GrainRegistry::new(),
            grain_residents: HashMap::new(),
            actor_grain_id: HashMap::new(),
            grain_actor_ids: HashMap::new(),
            vm: None,
            #[cfg(feature = "ai-runtime")]
            llm: llm::LlmState::new(),
            behavior_cache: HashMap::new(),
            pending_receive_wakes: Vec::new(),
            draining_receive_wakes: false,
            idle_callback: None,
            recovery_modules: HashMap::new(),
            #[cfg(feature = "ai-runtime")]
            ai: AiRuntimeRegistry::new(),
            #[cfg(feature = "ai-runtime")]
            supervisor_teams: SupervisorTeamRegistry::new(),
            crypto: Box::new(crate::backends::DefaultCryptoProvider::new()),
            spawnable_behaviors: HashMap::new(),
            aot_modules: std::collections::HashMap::new(),
            aot_module_storage: Vec::new(),
            #[cfg(any(feature = "ai-runtime", feature = "http-client"))]
            http: Box::new(crate::backends::ReqwestHttpProvider::new()),
            #[cfg(feature = "tls")]
            tls_provider: Box::new(crate::backends::DefaultTlsProvider::new()),
            pending_spawn_responses: HashMap::new(),
            remote_refs: HashMap::new(),
            pending_spawn_messages: HashMap::new(),
            spawn_placeholders: HashSet::new(),
            spawn_translations: HashMap::new(),
            dlq_actor_id: None,
            http_server: None,
            test_handlers: HashMap::new(),
            shard_idx: 0,
            shard_count: 1,
            cross_shard_tx: None,
            cross_shard_rx: None,
        }
    }

    /// Compute the BLAKE3 hash of `data` using the configured [`CryptoProvider`].
    pub fn hash_bytes(&self, data: &[u8]) -> [u8; 32] {
        self.crypto.hash(data)
    }

    /// Fill `buf` with cryptographically secure random bytes.
    pub fn random_bytes(&self, buf: &mut [u8]) {
        self.crypto.random_bytes(buf)
    }

    /// Make an HTTP POST request with a JSON body.
    /// Delegates to the configured [`HttpProvider`](crate::backends::HttpProvider).
    #[cfg(any(feature = "ai-runtime", feature = "http-client"))]
    pub fn http_post_json(&self, url: &str, body: &str) -> Result<String, String> {
        self.http.post_json(url, body)
    }

    /// Make an HTTP GET request.
    /// Delegates to the configured [`HttpProvider`](crate::backends::HttpProvider).
    #[cfg(any(feature = "ai-runtime", feature = "http-client"))]
    pub fn http_get(&self, url: &str) -> Result<String, String> {
        self.http.get(url)
    }

    /// Create a shard Runtime. Private - use [`Runtime::new_sharded`] to
    /// create a set of N shards with cross-shard channels wired.
    fn new_shard(
        shard_idx: u16,
        shard_count: u16,
        cross_shard_tx: Vec<mpsc::SyncSender<CrossShardMsg>>,
        cross_shard_rx: mpsc::Receiver<CrossShardMsg>,
    ) -> Self {
        let mut rt = Runtime::new();
        rt.shard_idx = shard_idx;
        rt.shard_count = shard_count;
        rt.cross_shard_tx = Some(cross_shard_tx);
        rt.cross_shard_rx = Some(cross_shard_rx);
        // Only shard 0 binds network transport; others skip it.
        if shard_idx != 0 {
            rt.distributed.enabled = false;
        }
        rt
    }

    /// Create `num_shards` Runtime instances, each wired with cross-shard
    /// channels. Returns a `Vec` of Runtimes ready to `run_scheduler()` in
    /// their own threads. Shard 0 owns network binding and cycle detection;
    /// all shards process their local actors independently.
    ///
    /// Actor assignment: `actor_id % num_shards` determines the owning shard.
    /// Cross-shard messages carry only value types (no heap pointers), keeping
    /// ORCA reference counting local to each shard.
    pub fn new_sharded(num_shards: usize) -> Vec<Runtime> {
        assert!(num_shards > 0, "num_shards must be >= 1");
        if num_shards == 1 {
            return vec![Runtime::new()];
        }

        // Create a sync_channel per shard (bounded, 1024 messages deep).
        let channels: Vec<(
            mpsc::SyncSender<CrossShardMsg>,
            mpsc::Receiver<CrossShardMsg>,
        )> = (0..num_shards).map(|_| mpsc::sync_channel(1024)).collect();

        let senders: Vec<mpsc::SyncSender<CrossShardMsg>> =
            channels.iter().map(|(tx, _)| tx.clone()).collect();

        let mut shards = Vec::with_capacity(num_shards);
        for (i, (_tx, rx)) in channels.into_iter().enumerate() {
            shards.push(Runtime::new_shard(
                i as u16,
                num_shards as u16,
                senders.clone(),
                rx,
            ));
        }
        shards
    }

    /// Install a test handler that intercepts `perform Effect.op` calls.
    ///
    /// The `effect_name` should be the qualified operation name (e.g.
    /// `"IO.print"`, `"DB.write"`).  The handler receives the frame
    /// registers (r0..rn as set up by the compiler before `Perform`) and
    /// returns `Some(value)` to mock the effect or `None` to fall through
    /// to real dispatch.
    ///
    /// # Example
    /// ```ignore
    /// rt.install_test_handler("DB.write", |regs| {
    ///     // regs[0] = key, regs[1] = value
    ///     Some(Value::unit())  // pretend write succeeded
    pub fn install_test_handler<F>(&mut self, effect_name: &str, handler: F)
    where
        F: Fn(&[Value]) -> Option<Value> + 'static,
    {
        self.test_handlers
            .insert(effect_name.to_string(), Box::new(handler));
    }

    /// Check whether a test handler is installed for `qualified_name` and
    /// return its result if so.
    pub fn check_test_handler(&self, qualified_name: &str, regs: &[Value]) -> Option<Value> {
        self.test_handlers
            .get(qualified_name)
            .and_then(|handler| handler(regs))
    }

    #[tracing::instrument(level = "trace", skip(self, init))]
    pub fn spawn_actor(&mut self, init: Box<dyn FnOnce() -> Vec<(String, Value)>>) -> u64 {
        spawn::spawn_actor_with_models(self, init, HashMap::new(), false, None)
    }

    /// Spawn an actor co-located on the same shard as `near_actor_id`.
    ///
    /// Runtime primitive for locality-aware placement (borrow P5): a child
    /// that communicates heavily with `near_actor_id` is placed on that
    /// actor's shard, turning cross-shard traffic into same-shard
    /// (thread-confined) mailbox traffic. In a single-shard runtime this is
    /// exactly [`Runtime::spawn_actor`].
    ///
    /// Ids are drawn from the global counter until one maps to the target
    /// shard, so the `actor_id % shard_count` ownership invariant is
    /// preserved; skipped ids are never assigned. Co-location is a *hint*:
    /// it changes placement, not correctness.
    pub fn spawn_actor_near(
        &mut self,
        near_actor_id: u64,
        init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    ) -> u64 {
        let id = if self.shard_count > 1 {
            let target_shard = (near_actor_id % self.shard_count as u64) as u16;
            loop {
                let candidate = fresh_actor_id();
                if (candidate % self.shard_count as u64) as u16 == target_shard {
                    break candidate;
                }
            }
        } else {
            fresh_actor_id()
        };
        spawn::spawn_actor_with_id(self, id, init, HashMap::new(), false, None)
    }

    pub fn spawn_persistent_actor(
        &mut self,
        init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
        state_models: HashMap<String, StateModel>,
    ) -> u64 {
        spawn::spawn_actor_with_models(self, init, state_models, true, None)
    }

    /// Spawn a durable workflow actor.  Workflows are always persistent and
    /// keep an append-only event journal in addition to snapshots.
    pub fn spawn_workflow_actor(
        &mut self,
        name: &str,
        init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
        state_models: HashMap<String, StateModel>,
    ) -> u64 {
        spawn::spawn_actor_with_models(self, init, state_models, true, Some(name))
    }

    /// Spawn an actor for `module`'s behavior `behavior_idx`, seeded with
    /// the `init` state fields, and wire up its bytecode handlers. Shared
    /// body of both VM-callback `spawn_actor` impls: `RuntimeVmCallbacks`
    /// (spawns from the top-level VM) and `BytecodeRuntimeCallbacks`
    /// (spawns from inside a scheduler-driven behavior on the shared
    /// runtime VM).
    pub fn spawn_from_module(
        &mut self,
        module: &crate::bytecode::CodeModule,
        behavior_idx: usize,
        init: Vec<(String, Value)>,
    ) -> Value {
        spawn::spawn_from_module(self, module, behavior_idx, init)
    }

    /// Register bytecode metadata so that a persistent actor can be recovered
    /// after a runtime restart.  The runtime stores the module, behavior
    /// offsets, and saga compensation offsets; `recover_actor` will restore
    /// them on the recreated actor.
    pub fn register_recovery_module(
        &mut self,
        actor_id: u64,
        module: crate::bytecode::CodeModule,
        offsets: Vec<usize>,
        compensation_offsets: Vec<Option<usize>>,
    ) {
        spawn::register_recovery_module(self, actor_id, module, offsets, compensation_offsets)
    }

    /// Register all `virtual entity` types declared in `module` with the
    /// grain registry so they can be hydrated on demand.  Safe to call
    /// multiple times for the same module (later calls are no-ops).
    pub fn register_module_grains(&mut self, module: &crate::bytecode::CodeModule) {
        for meta in &module.actor_metadata {
            if !meta.is_virtual {
                continue;
            }
            if self.grain_registry.contains(&meta.name) {
                continue;
            }
            let default_models: Vec<(String, StateModel)> = meta
                .state_models
                .iter()
                .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
                .collect();
            let bytecode_offsets =
                crate::runtime::spawn::bytecode_offsets_for(module, meta.is_workflow);
            let compensation_offsets: Vec<Option<usize>> = meta
                .behavior_indices
                .iter()
                .map(|&i| module.behaviors[i].compensate_offset)
                .collect();
            let grain_type = GrainType {
                module: module.clone(),
                default_models,
                bytecode_offsets,
                compensation_offsets,
                dehydrate_policy: DehydratePolicy::default(),
            };
            self.grain_registry.register(meta.name.clone(), grain_type);
        }
    }

    /// Install an LLM client for `perform LLM.ask(...)` calls.
    #[cfg(feature = "ai-runtime")]
    pub fn set_llm_client(&mut self, client: Box<dyn LlmClient>) {
        agent::set_llm_client(self, client)
    }

    /// Create a new empty pipeline and return its ID.
    #[cfg(feature = "ai-runtime")]
    pub fn pipeline_new(&mut self) -> u64 {
        agent::pipeline_new(self)
    }
    /// Add a stage to an existing pipeline. Returns the same pipeline ID on
    /// success so fluent construction can continue.
    #[cfg(feature = "ai-runtime")]
    pub fn pipeline_stage(
        &mut self,
        id: u64,
        name: &str,
        agent_id: u64,
        template: &str,
    ) -> Result<u64, String> {
        agent::pipeline_stage(self, id, name, agent_id, template)
    }
    /// Run a pipeline, returning the output of the final stage.
    #[cfg(feature = "ai-runtime")]
    pub fn pipeline_run(&mut self, id: u64, input: &str) -> Result<String, String> {
        agent::pipeline_run(self, id, input)
    }
    #[cfg(feature = "ai-runtime")]
    pub fn supervisor_new(&mut self) -> u64 {
        agent::supervisor_new(self)
    }

    #[cfg(feature = "ai-runtime")]
    pub fn supervisor_worker(
        &mut self,
        id: u64,
        name: &str,
        agent_id: u64,
        description: &str,
    ) -> Result<u64, String> {
        agent::supervisor_worker(self, id, name, agent_id, description)
    }

    #[cfg(feature = "ai-runtime")]
    pub fn supervisor_run(&mut self, id: u64, task: &str) -> Result<String, String> {
        agent::supervisor_run(self, id, task)
    }

    /// Create a new debate and return its ID.
    #[cfg(feature = "ai-runtime")]
    pub fn debate_new(&mut self, topic: &str, rounds: i64, threshold: f64) -> u64 {
        agent::debate_new(self, topic, rounds, threshold)
    }
    /// Add a participant to an existing debate. Returns the same debate ID on
    /// success so fluent construction can continue.
    #[cfg(feature = "ai-runtime")]
    pub fn debate_participant(
        &mut self,
        id: u64,
        name: &str,
        stance: &str,
        agent_id: u64,
    ) -> Result<u64, String> {
        agent::debate_participant(self, id, name, stance, agent_id)
    }
    /// Run a debate and return the moderator's synthesis.
    #[cfg(feature = "ai-runtime")]
    pub fn debate_run(&mut self, id: u64) -> Result<String, String> {
        agent::debate_run(self, id)
    }

    /// Convert a VM value to a Rust string using the actor's bytecode module
    /// constant pool for string-id values and reading pointer payloads as
    /// null-terminated UTF-8.
    #[cfg(feature = "ai-runtime")]
    fn vm_value_to_string(
        value: &crate::vm::Value,
        module: Option<&crate::bytecode::CodeModule>,
    ) -> Option<String> {
        agent::vm_value_to_string(value, module)
    }

    /// Execute an LLM request for an agent actor, reading the agent's model,
    /// system prompt, and episodic memory from durable state. The memory is
    /// updated with the user prompt and assistant response before being saved
    /// back to state.
    #[cfg(feature = "ai-runtime")]
    pub fn complete_agent_llm(&mut self, actor_id: u64, prompt: &str) -> Option<String> {
        agent::complete_agent_llm(self, actor_id, prompt)
    }

    /// Build a bare LLM request for a non-agent actor bytecode behavior,
    /// with `tools` filled from the actor's bytecode module. Pure
    /// read/build: safe to run before handing the request to a background
    /// worker thread.
    #[cfg(feature = "ai-runtime")]
    fn build_actor_llm_request(
        &self,
        actor_id: u64,
        model: &str,
        prompt: &str,
    ) -> Option<LlmRequest> {
        agent::build_actor_llm_request(self, actor_id, model, prompt)
    }

    /// Read an actor's state field as a plain string, resolving string-id
    /// values through the runtime VM's constant pools (heap pointer values
    /// are read directly). Useful for tests and tooling that inspect actor
    /// state produced by bytecode behaviors.
    #[cfg(feature = "ai-runtime")]
    pub fn actor_state_string(&self, actor_id: u64, field: &str) -> Option<String> {
        agent::actor_state_string(self, actor_id, field)
    }

    /// Set a token budget that caps total LLM token consumption.
    ///
    /// After the budget is exhausted `complete_llm_request` returns
    /// `LlmError::BudgetExceeded`.  Charges are applied after each
    /// successful response based on the actual token count returned
    /// by the provider.
    #[cfg(feature = "ai-runtime")]
    pub fn set_token_budget(&mut self, limit: u64) {
        agent::set_token_budget(self, limit)
    }

    /// Remove any configured token budget.
    #[cfg(feature = "ai-runtime")]
    pub fn clear_token_budget(&mut self) {
        agent::clear_token_budget(self)
    }
    /// Execute a chat-completion request using the configured LLM client.
    ///
    /// The provided `memory` messages are stored on the request before it is
    /// sent to the provider.
    #[cfg(feature = "ai-runtime")]
    pub fn complete_llm_request(
        &self,
        request: LlmRequest,
        memory: Vec<LlmMessage>,
    ) -> Result<LlmResponse, LlmError> {
        agent::complete_llm_request(self, request, memory)
    }

    /// Execute an LLM request, optionally running tool calls from the response.
    ///
    /// The request's `tools` list is populated from `module.tools`. If the
    /// response contains tool calls, the named functions are looked up in the
    /// module exports, invoked with the provided JSON arguments, and the results
    /// are sent back to the model for a final response. The supplied `memory`
    /// messages are preserved across tool-call rounds.
    #[cfg(feature = "ai-runtime")]
    pub fn complete_llm_with_tools(
        &mut self,
        request: LlmRequest,
        memory: Vec<LlmMessage>,
        module: &crate::bytecode::CodeModule,
    ) -> Result<LlmResponse, LlmError> {
        agent::complete_llm_with_tools(self, request, memory, module)
    }

    /// Post-process an LLM response on the scheduler thread: invoke any tool
    /// calls named in the response against `module` and synthesize the
    /// response content from their results.
    #[cfg(feature = "ai-runtime")]
    pub(crate) fn finish_tool_calls(
        &mut self,
        module: &crate::bytecode::CodeModule,
        response: LlmResponse,
    ) -> Result<LlmResponse, LlmError> {
        agent::finish_tool_calls(self, module, response)
    }

    /// Record an emitted event on an actor. Delegates to the workflow subsystem.
    pub fn emit_event(&mut self, actor_id: u64, event: &str, args: &[crate::vm::Value]) {
        workflow::emit_event(self, actor_id, event, args)
    }

    /// Append a `TimerSet` workflow event and checkpoint the actor.
    pub fn append_timer_set(
        &mut self,
        actor_id: u64,
        name: &str,
        duration_ms: u64,
    ) -> std::io::Result<()> {
        workflow::append_timer_set(self, actor_id, name, duration_ms)
    }

    /// Append a `TimerFired` workflow event and checkpoint the actor.
    pub fn append_timer_fired(&mut self, actor_id: u64, name: &str) -> std::io::Result<()> {
        workflow::append_timer_fired(self, actor_id, name)
    }

    /// Append a `SignalReceived` workflow event and checkpoint the actor.
    pub fn append_signal_received(
        &mut self,
        actor_id: u64,
        name: &str,
        payload: Option<String>,
    ) -> std::io::Result<()> {
        workflow::append_signal_received(self, actor_id, name, payload)
    }

    /// Append a `SagaCompensated` workflow event and checkpoint the actor.
    pub fn append_saga_compensated(
        &mut self,
        actor_id: u64,
        step_name: &str,
    ) -> std::io::Result<()> {
        workflow::append_saga_compensated(self, actor_id, step_name)
    }

    /// Send a named signal to a workflow actor.
    ///
    /// The signal is appended to the durable workflow journal and, if the actor
    /// is currently suspended waiting for this signal, its execution is resumed.
    /// Deliver a signal to a workflow actor. Delegates to workflow subsystem.
    pub fn signal_workflow(&mut self, actor_id: u64, name: &str, payload: Option<String>) {
        workflow::signal_workflow(self, actor_id, name, payload)
    }

    /// Register a read-only query handler on a workflow actor.
    ///
    /// The handler is a function/closure value invoked by `query_workflow`
    /// with the workflow actor bound as `self`, so it can read the actor's
    /// current state.  Registration is a no-op for missing or non-workflow
    /// actors: queries are a workflow-only concept.  Handlers are not
    /// journaled, so they must be re-registered after a node restart.
    /// Register a read-only query handler on a workflow actor.
    pub fn register_workflow_query(&mut self, actor_id: u64, name: &str, handler: Value) {
        workflow::register_workflow_query(self, actor_id, name, handler)
    }

    /// Invoke a registered query handler on a workflow actor and return its
    /// result.  Returns `None` when the actor is missing, is not a workflow,
    /// has no handler registered under `name`, or the handler value does not
    /// resolve to a function in the actor's bytecode module.
    ///
    /// Queries are read-only: unlike `signal_workflow` they append nothing
    /// to the durable workflow journal, force no checkpoint, and never
    /// resume a suspended step.  The handler runs on a private VM with the
    /// workflow actor bound as `self`, so a query performed from inside a
    /// running behavior cannot disturb that behavior's frames; handlers
    /// must therefore be immediate (non-capturing) functions, since closure
    /// environments live on the VM that created them.
    /// Invoke a registered query handler on a workflow actor. Delegates to workflow subsystem.
    pub fn query_workflow(&mut self, actor_id: u64, name: &str) -> Option<Value> {
        workflow::query_workflow(self, actor_id, name)
    }

    /// Drain completed background LLM calls and resume the suspended actors
    /// waiting for them.
    #[cfg(feature = "ai-runtime")]
    pub(crate) fn poll_llm_completions(&mut self) {
        llm::poll_llm_completions(self)
    }

    /// Record a completed background LLM call on its actor and resume the
    /// actor's suspended behavior, if any. Errors trigger the retry/fallback
    /// pipeline when the actor has a configured agent retry or fallback.
    #[cfg(feature = "ai-runtime")]
    pub(crate) fn store_llm_completion(
        &mut self,
        actor_id: u64,
        result: Result<LlmResponse, LlmError>,
    ) {
        llm::store_llm_completion(self, actor_id, result)
    }

    /// Re-dispatch an in-flight LLM request on retry timer fire.
    #[cfg(feature = "ai-runtime")]
    pub(crate) fn handle_llm_retry_timer(&mut self, actor_id: u64) {
        llm::handle_llm_retry_timer(self, actor_id)
    }

    /// Send an LLM request to the persistent worker thread for execution.
    /// Returns true if the request was dispatched, false if the worker
    /// channel is unavailable (caller should roll back in-flight state).
    #[cfg(feature = "ai-runtime")]
    pub(crate) fn dispatch_llm_request(
        &mut self,
        actor_id: u64,
        request: LlmRequest,
        prompt: &str,
    ) -> bool {
        llm::dispatch_llm_request(self, actor_id, request, prompt)
    }

    /// Re-enqueue an actor whose suspension has resolved if messages queued
    /// up while it was suspended.  step_actor refuses to run new messages
    /// while a suspension is live, so without this the queued mail would
    /// sit until an unrelated send happened to re-enqueue the actor.
    fn requeue_if_mail_pending(&mut self, actor_id: u64) {
        let needs_requeue = self
            .actors
            .get(&actor_id)
            .map(|a| a.suspended_execution.is_none() && !a.mailbox.is_empty())
            .unwrap_or(false);
        if needs_requeue {
            self.enqueue_actor(actor_id);
        }
    }

    /// Resume an actor that yielded at a JIT safepoint.
    ///
    /// Mirrors the structure of `resume_suspended_llm_step` but without
    /// LLM-specific logic: re-installs callbacks, restores VM state,
    /// resets the safepoint counter, and resumes execution.
    fn resume_suspended_jit_yield(&mut self, actor_id: u64) {
        let suspended = match self.actors.get_mut(&actor_id) {
            Some(actor) => actor.suspended_execution.take(),
            None => return,
        };
        let Some(suspended) = suspended else { return };

        if self.vm.is_none() {
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                actor.suspended_execution = Some(suspended);
            }
            return;
        }

        let self_ptr: *mut Runtime = self;
        unsafe {
            let vm = (*self_ptr).vm.as_mut().unwrap();
            vm.set_actor_callbacks(Box::new(BytecodeRuntimeCallbacks::new(self_ptr, actor_id)));
            vm.set_distributed_callbacks(Box::new(BytecodeDistributedCallbacks {
                runtime: self_ptr,
            }));
            vm.restore_suspended_state(suspended.vm_state);

            // Reset the safepoint budget and wire the pointer for JIT code.
            if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                actor.jit_safepoint_counter = crate::jit::runtime::JIT_SAFEPOINT_BUDGET;
                crate::jit::runtime::set_jit_safepoint_ptr(&mut actor.jit_safepoint_counter);
            }

            (*self_ptr).vm_exec_begin();
            let result = vm.resume();
            crate::jit::runtime::clear_jit_safepoint_ptr();

            match result {
                Ok(_) if vm.yield_pending => {
                    // JIT safepoint yield: re-capture VM state.
                    if let Some(vm_state) = vm.take_suspended_state() {
                        if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                            actor.suspended_execution =
                                Some(crate::runtime::actor::SuspendedExecution {
                                    vm_state,
                                    behavior_idx: suspended.behavior_idx,
                                    step_name: suspended.step_name.clone(),
                                });
                            actor.jit_yield_pending = true;
                        }
                    }
                }
                Ok(_) => {
                    // Behavior completed: clear suspension.
                    if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                        actor.jit_yield_pending = false;
                    }
                }
                Err(crate::types::NuError::Suspended(_)) => {
                    // Re-suspended (e.g. signal wait or receive-wait):
                    // re-capture VM state.
                    if let Some(vm_state) = vm.take_suspended_state() {
                        let signal_name = vm.suspended_signal_name.take();
                        let receive_timeout = vm.suspended_receive_timeout.take();
                        if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                            let marker = suspension_marker(actor, signal_name);
                            actor.waiting_signal = marker;
                            actor.suspended_execution =
                                Some(crate::runtime::actor::SuspendedExecution {
                                    vm_state,
                                    behavior_idx: suspended.behavior_idx,
                                    step_name: suspended.step_name,
                                });
                        }
                        (*self_ptr).maybe_schedule_receive_wait(actor_id, receive_timeout);
                    }
                }
                Err(_) => {
                    // Other error: clear suspension.
                    if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                        actor.jit_yield_pending = false;
                    }
                }
            }
            (*self_ptr).vm_exec_end();
        }
        self.requeue_if_mail_pending(actor_id);
    }

    /// Enqueue an actor on the scheduler at its current priority. All
    /// scheduler enqueue paths go through here so a priority set via
    /// `perform Actor.set_priority` takes effect on the next (re)queue;
    /// unknown actors (e.g. already exited) enqueue at the Normal default.
    pub(crate) fn enqueue_actor(&self, actor_id: u64) {
        // Cross-shard routing: if the actor lives on another shard, send
        // an EnqueueActor message. The receiving shard's drain loop enqueues
        // it locally.
        if self.shard_count > 1 {
            let target_shard = (actor_id % self.shard_count as u64) as u16;
            if target_shard != self.shard_idx {
                let priority = ActorPriority::Normal;
                let tx = self.cross_shard_tx.as_ref().unwrap();
                let _ = tx[target_shard as usize]
                    .try_send(CrossShardMsg::EnqueueActor { actor_id, priority });
                return;
            }
        }
        let priority = self
            .actors
            .get(&actor_id)
            .map(|a| a.priority)
            .unwrap_or_default();
        self.scheduler.enqueue_with_priority(actor_id, priority);
    }

    // -- Cross-shard message handling --

    /// Deliver a message that arrived from another shard.  No ORCA
    /// reference-counting is performed because cross-shard payloads are
    /// restricted to value types (ints, strings, bools, unit, nil).
    fn deliver_cross_shard_message(
        &mut self,
        target_id: u64,
        behavior_id: u16,
        payload: Vec<Value>,
        sender: u64,
        trace_id: Option<String>,
        grain_id: Option<GrainId>,
    ) {
        // If the target is a known grain identity but not currently resident,
        // hydrate it before delivering the message. The `grain_id` carried on
        // grain cross-shard messages handles first-time hydration; the local
        // index handles re-hydration after eviction.
        if !self.actors.contains_key(&target_id) {
            let grain_id_to_hydrate =
                grain_id.or_else(|| self.grain_actor_ids.get(&target_id).cloned());
            if let Some(grain_id) = grain_id_to_hydrate {
                if let Err(e) = self.resolve_or_hydrate_grain(grain_id) {
                    warn!(
                        "nulang-grain: failed to hydrate grain actor {} on cross-shard delivery: {}",
                        target_id, e
                    );
                    self.route_to_dlq(
                        &Message {
                            behavior_id,
                            payload: Arc::new(Vec::new()),
                            sender,
                            priority: MessagePriority::System,
                            trace_id: None,
                        },
                        "grain hydration failed (cross-shard)",
                    );
                    return;
                }
            }
        }

        let msg = Message {
            behavior_id,
            payload: Arc::new(payload),
            sender,
            priority: MessagePriority::Normal,
            trace_id: trace_id.clone(),
        };
        if let Some(actor) = self.actors.get_mut(&target_id) {
            if let Err(_dropped) = actor.mailbox.push_local(msg) {
                self.route_to_dlq(
                    &Message {
                        behavior_id,
                        payload: Arc::new(Vec::new()),
                        sender,
                        priority: MessagePriority::System,
                        trace_id: None,
                    },
                    "mailbox full (cross-shard)",
                );
            }
        } else {
            self.route_to_dlq(
                &Message {
                    behavior_id,
                    payload: Arc::new(Vec::new()),
                    sender,
                    priority: MessagePriority::System,
                    trace_id: None,
                },
                "target actor not found (cross-shard)",
            );
        }
        self.enqueue_actor(target_id);
        // Wake an actor suspended in a timed selective receive (same as
        // local path, but without the deferred-wake machinery since
        // cross-shard messages arrive between steps, never mid-VM-exec).
        let wake_for_receive = self
            .actors
            .get(&target_id)
            .map(|a| {
                a.suspended_execution.is_some()
                    && a.receive_wait.map(|w| !w.timed_out).unwrap_or(false)
            })
            .unwrap_or(false);
        if wake_for_receive {
            self.resume_suspended_receive_wait(target_id);
        }
    }

    /// Drain all pending cross-shard messages into local mailboxes. Called
    /// at the top of every scheduler-loop iteration before dequeuing work.
    fn drain_cross_shard_messages(&mut self) {
        let mut pending: Vec<CrossShardMsg> = Vec::new();
        {
            let rx = match self.cross_shard_rx.as_ref() {
                Some(rx) => rx,
                None => return,
            };
            loop {
                match rx.try_recv() {
                    Ok(msg) => pending.push(msg),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }
        for msg in pending {
            match msg {
                CrossShardMsg::DeliverMessage {
                    target_id,
                    behavior_id,
                    payload,
                    sender,
                    trace_id,
                    grain_id,
                } => {
                    self.deliver_cross_shard_message(
                        target_id,
                        behavior_id,
                        payload,
                        sender,
                        trace_id,
                        grain_id,
                    );
                }
                CrossShardMsg::DeliverMessageWithObjects {
                    target_id,
                    behavior_id,
                    mut payload,
                    objects,
                    sender,
                    trace_id,
                    grain_id,
                } => {
                    // Insert each object into the local store and rewrite the
                    // payload so that object ids refer to local entries.
                    let mut id_map: std::collections::HashMap<
                        crate::runtime::object_store::ObjectId,
                        crate::runtime::object_store::ObjectId,
                    > = std::collections::HashMap::with_capacity(objects.len());
                    for (original_id, bytes) in objects {
                        let local_id = self.object_store.put(bytes.into_boxed_slice());
                        id_map.insert(original_id, local_id);
                    }
                    for value in &mut payload {
                        if let Some(id) = value.as_object_id() {
                            if let Some(&local_id) = id_map.get(&id) {
                                *value = Value::object(local_id);
                            }
                        }
                    }
                    self.deliver_cross_shard_message(
                        target_id,
                        behavior_id,
                        payload,
                        sender,
                        trace_id,
                        grain_id,
                    );
                }
                CrossShardMsg::EnqueueActor { actor_id, priority } => {
                    self.scheduler.enqueue_with_priority(actor_id, priority);
                }
            }
        }
    }

    /// Mark the start of a call into the shared runtime VM. While the
    /// depth is non-zero, receive-wait wakes are deferred onto
    /// `pending_receive_wakes` (see `send_message_by_id`).
    fn vm_exec_begin(&mut self) {
        self.vm_execution_depth += 1;
    }

    /// Mark the end of a call into the shared runtime VM. When the
    /// outermost call returns, drain the deferred receive-wait wakes: a
    /// resumed behavior can itself send and re-queue a wake, so loop until
    /// the backlog is empty. The drain flag keeps this iterative - a
    /// nested `vm_exec_end` (from a resume issued by the drain) returns
    /// without draining again.
    fn vm_exec_end(&mut self) {
        self.vm_execution_depth = self.vm_execution_depth.saturating_sub(1);
        if self.vm_execution_depth > 0 || self.draining_receive_wakes {
            return;
        }
        self.draining_receive_wakes = true;
        while let Some(target_id) = self.pending_receive_wakes.pop() {
            // The drain can run inside another actor's step: attribute
            // sends by the resumed behavior to the resumed actor, not to
            // the interrupted one.
            let prev_current_actor = self.current_actor;
            self.current_actor = Some(target_id);
            self.resume_suspended_receive_wait(target_id);
            self.current_actor = prev_current_actor;
        }
        self.draining_receive_wakes = false;
    }

    /// Resume a workflow actor that is suspended waiting for a signal.
    pub(crate) fn resume_suspended_workflow_step(&mut self, actor_id: u64) {
        let suspended = match self.actors.get_mut(&actor_id) {
            Some(actor) => actor.suspended_execution.take(),
            None => return,
        };
        let Some(suspended) = suspended else { return };

        if self.vm.is_none() {
            // No VM available; put the suspension back so a later message
            // can re-trigger the step.
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                actor.suspended_execution = Some(suspended);
            }
            return;
        }

        let behavior_idx = suspended.behavior_idx;
        let step_name = suspended.step_name;
        let self_ptr: *mut Runtime = self;
        let result = unsafe {
            let vm = (*self_ptr).vm.as_mut().unwrap();
            // Re-install callbacks bound to THIS actor: other actors may have
            // run on the shared VM while this one was suspended, and a resumed
            // `LLM.ask` must record its in-flight call (and later completion)
            // on this actor - same as resume_suspended_llm_step.
            vm.set_distributed_callbacks(Box::new(BytecodeDistributedCallbacks {
                runtime: self_ptr,
            }));
            vm.set_actor_callbacks(Box::new(BytecodeRuntimeCallbacks::new(self_ptr, actor_id)));
            vm.restore_suspended_state(suspended.vm_state);
            // A signal-resumed step is still scheduler-context execution: a
            // `perform LLM.ask` after the wait must suspend (non-blocking)
            // instead of blocking the caller thread on the HTTP call.
            let saved_suspend = (*self_ptr).suspend_enabled;
            (*self_ptr).suspend_enabled = true;
            (*self_ptr).vm_exec_begin();
            let result = vm.resume();
            (*self_ptr).suspend_enabled = saved_suspend;
            result
        };

        if let Some(actor) = self.actors.get_mut(&actor_id) {
            actor.waiting_signal = None;
        }

        match result {
            Ok(_) => {
                if self.actor_is_workflow(actor_id) {
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        if let Some(n) =
                            actor.get_state_field("step_index").and_then(|v| v.as_int())
                        {
                            actor.set_state_field("step_index", Value::int(n + 1));
                        }
                    }
                    let seq = self.next_sequence(actor_id);
                    let _ = self.persistence.append_workflow_event(
                        actor_id,
                        WorkflowEvent::StepCompleted {
                            sequence: seq,
                            step_name,
                        },
                    );
                    self.checkpoint_actor(actor_id);
                }
            }
            Err(crate::types::NuError::Suspended(_)) => {
                // Suspended again - waiting for another signal OR on a
                // background LLM call (`perform LLM.ask` after the wait).
                // Re-capture the VM state so the next matching signal or the
                // pumped LLM completion can resume the step.  The marker is
                // the awaited signal's name for a signal wait, or the
                // reserved LLM marker (via suspension_marker) for an LLM
                // suspend, whose completion flows through
                // resume_suspended_llm_step - that path performs the workflow
                // completion bookkeeping.  BytecodeRuntimeCallbacks::
                // suspend_for_signal is a no-op, so the capture must happen
                // here - same as in run_bytecode_at_offset and
                // resume_suspended_llm_step.
                let recaptured = match self.vm.as_mut() {
                    Some(vm) => vm.take_suspended_state().map(|vm_state| {
                        let signal_name = vm.suspended_signal_name.take();
                        let receive_timeout = vm.suspended_receive_timeout.take();
                        (vm_state, signal_name, receive_timeout)
                    }),
                    None => None,
                };
                if let Some((vm_state, signal_name, receive_timeout)) = recaptured {
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        let marker = suspension_marker(actor, signal_name);
                        actor.waiting_signal = marker;
                        actor.suspended_execution =
                            Some(crate::runtime::actor::SuspendedExecution {
                                vm_state,
                                behavior_idx,
                                step_name,
                            });
                    }
                    // A chained receive-after suspend arms its timeout
                    // here; a no-op for the other sentinels.
                    self.maybe_schedule_receive_wait(actor_id, receive_timeout);
                }
            }
            Err(_) => {
                // Step failed after resumption: run saga compensations.
                if self.actor_is_workflow(actor_id) {
                    self.run_saga_compensation(actor_id, behavior_idx);
                }
            }
        }
        // End the VM-execution window only after the match above: the
        // re-capture arm reads the shared VM's frames, which draining
        // deferred wakes would clobber; the compensation arm runs nested
        // bytecode whose own begin/end must stay inside this window. Runs
        // on every path so wakes of other actors are not lost.
        self.vm_exec_end();
        // The suspension resolved (completed or failed): drain any mail
        // that queued up while the step was suspended.
        self.requeue_if_mail_pending(actor_id);
    }

    /// Send a message to `target_id`'s `behavior` mailbox by name.
    ///
    /// KNOWN SURPRISING BEHAVIOR (verified 2026-08-02, not fixed --
    /// see the comment in `flush_actor_mailbox` for why): a `behavior`
    /// name that doesn't match any of the target's registered
    /// behaviors resolves to behavior id 0 via `unwrap_or(0)` below,
    /// NOT a dropped/no-op message -- a typo'd or undeclared behavior
    /// name silently runs the actor's FIRST declared behavior instead
    /// of erroring or being ignored. See SPEC2.md Chapter 8 (message
    /// passing) and `conformance/behavior/lifecycle_03/04_*.nula`.
    pub fn send_message(&mut self, target_id: u64, behavior: &str, args: &[Value]) {
        // Name-based sends already carry the wire behavior name, so route
        // remote refs directly (same local-existence guard as
        // `send_message_by_id`; see RFC-0007 note there).
        if !self.actors.contains_key(&target_id) {
            if let Some(node) = self.remote_refs.get(&target_id).copied() {
                self.route_ref_send(target_id, node, behavior, args);
                return;
            }
        }
        let behavior_id = self.behavior_id_for(target_id, behavior).unwrap_or(0);
        self.send_message_by_id(target_id, behavior_id, args);
    }

    /// Route a message to a KNOWN remote ref (hosting node already
    /// resolved via `remote_refs`): queue while the spawn placeholder is
    /// still pending, otherwise translate the placeholder to the real
    /// actor id (if applicable) and send over the wire.
    fn route_ref_send(&mut self, target_id: u64, node: NodeId, behavior: &str, args: &[Value]) {
        if self.spawn_placeholders.contains(&target_id)
            && !self.pending_spawn_responses.contains_key(&target_id)
        {
            // SpawnResponse still in flight: queue the pre-resolved wire
            // form; it flushes when the real actor id arrives.
            distribution::queue_spawn_message(self, target_id, node, behavior, args);
        } else {
            let real_id = self
                .spawn_translations
                .get(&target_id)
                .copied()
                .unwrap_or(target_id);
            self.send_distributed(ActorAddress::remote(node, real_id), behavior, args);
        }
    }

    /// Full (wire) behavior name for a behavior index in the given actor's
    /// module — the name the receiving node resolves against ITS behavior
    /// table. Unlike `step_name_for` this does NOT strip the dotted
    /// workflow prefix: remote resolution matches the full entry name.
    fn behavior_wire_name_for(&self, actor: Option<u64>, behavior_id: u16) -> Option<String> {
        let actor = self.actors.get(&actor?)?;
        if let Some(entry) = actor.behavior_table.get(behavior_id as usize) {
            if !entry.name.is_empty() {
                return Some(entry.name.clone());
            }
        }
        let module = actor.bytecode_module.as_ref()?;
        module
            .behaviors
            .get(behavior_id as usize)
            .map(|b| b.name.clone())
    }

    /// Synchronously run a single behavior on an actor and return its result.
    /// Used by the VM's `Ask` opcode when a real runtime is attached.
    pub fn ask_actor_sync(
        &mut self,
        actor_id: u64,
        behavior_id: u16,
        args: &[Value],
    ) -> crate::types::NuResult<Value> {
        // Synchronous asks (pipelines, supervisors, debates, nested `Ask`)
        // always block on LLM calls; only scheduler-driven behaviors
        // suspend. Force suspension off for the whole body.
        let saved_suspend = self.suspend_enabled;
        self.suspend_enabled = false;
        // Run the asked behavior under a child of the caller's current trace
        // context so sends it performs continue the chain. The call is fully
        // synchronous (LLM suspension forced off), so the transient context
        // cannot leak into a concurrent resume path.
        let saved_trace = self.current_trace;
        self.current_trace = saved_trace.as_ref().map(|t| t.child());
        let result = self.ask_actor_sync_inner(actor_id, behavior_id, args);
        self.current_trace = saved_trace;
        self.suspend_enabled = saved_suspend;
        result
    }

    fn ask_actor_sync_inner(
        &mut self,
        actor_id: u64,
        behavior_id: u16,
        args: &[Value],
    ) -> crate::types::NuResult<Value> {
        let behavior_idx = behavior_id as usize;

        // Intercept semantic-memory behaviors generated by compile_agent.  These
        // are bytecode behaviors at compile time, but their semantics are
        // implemented directly by the runtime so they can mutate and read the
        // durable `semantic_memory` JSON field.
        #[cfg(feature = "ai-runtime")]
        let behavior_name = self.step_name_for(actor_id, behavior_idx);
        #[cfg(feature = "ai-runtime")]
        if self.actor_is_agent(actor_id) && self.is_semantic_memory_behavior(&behavior_name) {
            self.current_actor = Some(actor_id);
            let result = if behavior_name == "store_fact" {
                let content = args
                    .get(0)
                    .and_then(|v| {
                        self.actors
                            .get(&actor_id)
                            .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                    })
                    .unwrap_or_default();
                self.semantic_memory_store(actor_id, &content)
            } else {
                let query = args
                    .get(0)
                    .and_then(|v| {
                        self.actors
                            .get(&actor_id)
                            .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                    })
                    .unwrap_or_default();
                let top_k = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
                self.semantic_memory_recall(actor_id, &query, top_k)
            };
            self.checkpoint_actor(actor_id);
            self.current_actor = None;
            return Ok(result);
        }

        // Intercept procedural-memory behaviors generated by compile_agent.
        #[cfg(feature = "ai-runtime")]
        if self.actor_is_agent(actor_id) && self.is_procedural_memory_behavior(&behavior_name) {
            self.current_actor = Some(actor_id);
            let result = match behavior_name.as_str() {
                "store_pattern" => {
                    let key = args
                        .get(0)
                        .and_then(|v| {
                            self.actors
                                .get(&actor_id)
                                .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                        })
                        .unwrap_or_default();
                    let input_pattern = args
                        .get(1)
                        .and_then(|v| {
                            self.actors
                                .get(&actor_id)
                                .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                        })
                        .unwrap_or_default();
                    let output_template = args
                        .get(2)
                        .and_then(|v| {
                            self.actors
                                .get(&actor_id)
                                .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                        })
                        .unwrap_or_default();
                    self.procedural_memory_store_pattern(
                        actor_id,
                        &key,
                        &input_pattern,
                        &output_template,
                    )
                }
                "get_pattern" => {
                    let key = args
                        .get(0)
                        .and_then(|v| {
                            self.actors
                                .get(&actor_id)
                                .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                        })
                        .unwrap_or_default();
                    self.procedural_memory_get_pattern(actor_id, &key)
                }
                "add_example" => {
                    let task = args
                        .get(0)
                        .and_then(|v| {
                            self.actors
                                .get(&actor_id)
                                .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                        })
                        .unwrap_or_default();
                    let input = args
                        .get(1)
                        .and_then(|v| {
                            self.actors
                                .get(&actor_id)
                                .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                        })
                        .unwrap_or_default();
                    let output = args
                        .get(2)
                        .and_then(|v| {
                            self.actors
                                .get(&actor_id)
                                .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                        })
                        .unwrap_or_default();
                    self.procedural_memory_add_example(actor_id, &task, &input, &output)
                }
                "get_examples" => {
                    let task = args
                        .get(0)
                        .and_then(|v| {
                            self.actors
                                .get(&actor_id)
                                .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                        })
                        .unwrap_or_default();
                    let query = args
                        .get(1)
                        .and_then(|v| {
                            self.actors
                                .get(&actor_id)
                                .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                        })
                        .unwrap_or_default();
                    let top_k = args.get(2).and_then(|v| v.as_int()).unwrap_or(1) as usize;
                    self.procedural_memory_get_examples(actor_id, &task, &query, top_k)
                }
                _ => crate::vm::Value::nil(),
            };
            self.checkpoint_actor(actor_id);
            self.current_actor = None;
            return Ok(result);
        }

        // Flush any pending messages in the target's mailbox before executing
        // the asked behavior.  This ensures that `send c inc(); ask c get()`
        // works: the `inc` message gets processed before `get` runs, so the
        // ask sees the updated state.  Without this, sends sit in the mailbox
        // until the scheduler runs (which happens after the top-level program
        // completes).
        self.flush_actor_mailbox(actor_id);

        let is_native = self
            .actors
            .get(&actor_id)
            .and_then(|a| a.behavior_table.get(behavior_idx))
            .map(|e| !e.name.is_empty())
            .unwrap_or(false);
        if is_native {
            let handler =
                self.actors.get(&actor_id).unwrap().behavior_table[behavior_idx].handler_fn;
            self.current_actor = Some(actor_id);
            if self.actor_is_persistent(actor_id) {
                let seq = self.next_sequence(actor_id);
                let payload = args.iter().map(PersistedValue::from_value).collect();
                let _ = self.persistence.append_journal(
                    actor_id,
                    JournalEntry {
                        sequence: seq,
                        behavior_id,
                        payload,
                    },
                );
            }
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                handler(actor, args);
            }
            self.checkpoint_actor(actor_id);
            self.current_actor = None;
            return Ok(Value::nil());
        }
        if self.has_bytecode_handler(actor_id, behavior_idx) {
            let result = self.run_bytecode_behavior(actor_id, behavior_idx, args);
            self.checkpoint_actor(actor_id);
            self.current_actor = None;
            return result;
        }
        self.current_actor = None;
        Ok(Value::nil())
    }

    /// Process all pending messages in an actor's mailbox synchronously.
    /// Used by `ask_actor_sync_inner` so that `send`-then-`ask` works:
    /// messages queued by `send` before the `ask` get delivered before the
    /// asked behavior executes.
    fn flush_actor_mailbox(&mut self, actor_id: u64) {
        // We must not recurse into ask_actor_sync_inner (which calls us).
        // Guard against re-entrant flushes by tracking depth per actor.
        // In practice this shouldn't happen because behaviors triggered
        // by flush don't issue nested asks, but be safe.
        const MAX_FLUSH_DEPTH: usize = 32;
        let mut depth = 0;
        loop {
            let msg = match self.actors.get_mut(&actor_id) {
                Some(actor) => actor.receive(),
                None => return,
            };
            let msg = match msg {
                Some(m) => m,
                None => return,
            };
            let behavior_idx = msg.behavior_id as usize;

            // Hold heap pointers from the payload.
            self.hold_payload_refs(actor_id, &msg.payload);

            if self.has_bytecode_handler(actor_id, behavior_idx) {
                let prev = self.current_actor;
                self.current_actor = Some(actor_id);
                let _ = self.run_bytecode_behavior(actor_id, behavior_idx, &msg.payload);
                self.checkpoint_actor(actor_id);
                self.current_actor = prev;
            } else if let Some(handler) = self
                .actors
                .get(&actor_id)
                .and_then(|a| a.behavior_table.get(behavior_idx))
                .map(|e| e.handler_fn)
            {
                let prev = self.current_actor;
                self.current_actor = Some(actor_id);
                if let Some(actor) = self.actors.get_mut(&actor_id) {
                    handler(actor, &msg.payload);
                }
                self.checkpoint_actor(actor_id);
                self.current_actor = prev;
            }
            // A behavior_idx with neither a bytecode nor native handler
            // falls through here silently. In practice this branch is
            // unreachable for messages sent via `send_message`/
            // `send_message_by_id` today: `send_message` resolves an
            // unknown behavior NAME to id 0 via
            // `behavior_id_for(..).unwrap_or(0)` (see its doc comment) --
            // NOT a genuinely unknown numeric id -- so a typo'd or
            // undeclared behavior name silently runs behavior 0 well
            // before reaching this point, rather than being skipped as
            // this comment used to claim. Tracked as a known surprising
            // behavior in SPEC2.md (Chapter 8, message passing), not
            // fixed here -- `send_message` is called pervasively and
            // AGENTS.md documents the remote-message path as
            // deliberately mirroring this same fallback, so correcting
            // it needs a wider, carefully-audited change, not a
            // single-site patch.

            depth += 1;
            if depth >= MAX_FLUSH_DEPTH {
                // Safety valve: if an actor keeps sending itself messages
                // that trigger more sends, don't loop forever.
                break;
            }
        }
    }

    pub fn behavior_id_for(&self, target_id: u64, behavior: &str) -> Option<u16> {
        let actor = self.actors.get(&target_id)?;
        // Allocation-free match: `entry.name == behavior`, or
        // `entry.name` ends with `.<behavior>` (qualified name).
        let matches = |name: &str| {
            name == behavior
                || name
                    .strip_suffix(behavior)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        };
        // Search the per-actor behavior table first (native handlers).
        if let Some(idx) = actor
            .behavior_table
            .iter()
            .position(|entry| matches(&entry.name))
        {
            return Some(idx as u16);
        }
        // Fall back to the module-level behavior table (bytecode handlers).
        // Returns the GLOBAL index into module.behaviors, which matches
        // what bytecode_offsets expects.
        let module = actor.bytecode_module.as_ref()?;
        module
            .behaviors
            .iter()
            .position(|b| matches(&b.name))
            .map(|idx| idx as u16)
    }

    /// Resolve a behavior name to a numeric id using the registered grain
    /// type's module. This lets `send_to_grain` route across shards before the
    /// target actor has been hydrated on the local shard.
    fn resolve_grain_behavior_id(&self, grain_id: &GrainId, behavior_name: &str) -> Option<u16> {
        let grain_type = self.grain_registry.get(&grain_id.grain_type)?;
        let suffix = format!(".{}", behavior_name);
        grain_type
            .module
            .behaviors
            .iter()
            .position(|b| b.name == behavior_name || b.name.ends_with(&suffix))
            .map(|idx| idx as u16)
    }

    /// Send a message to an actor owned by another shard. Validates that the
    /// payload contains only value types (object-store refs are copied to the
    /// target shard's store). Returns `true` if the message was accepted for
    /// delivery, `false` if it was dropped because the payload contained a heap
    /// pointer, actor ref, or closure.
    fn send_cross_shard_message(
        &mut self,
        target_id: u64,
        behavior_id: u16,
        args: Vec<Value>,
        out_trace: Option<String>,
        grain_id: Option<GrainId>,
    ) -> bool {
        let target_shard = (target_id % self.shard_count as u64) as u16;
        for arg in &args {
            if arg.is_ptr() || arg.is_actor_ref() || arg.is_closure() {
                tracing::warn!(
                    "nulang-shard: dropping cross-shard message to actor {}: \
                     payload contains heap pointer / actor ref / closure",
                    target_id
                );
                return false;
            }
        }
        let tx = self.cross_shard_tx.as_ref().unwrap();
        let object_refs: Vec<crate::runtime::object_store::ObjectId> =
            args.iter().filter_map(|v| v.as_object_id()).collect();
        if object_refs.is_empty() {
            let _ = tx[target_shard as usize].try_send(CrossShardMsg::DeliverMessage {
                target_id,
                behavior_id,
                payload: args,
                sender: self.current_actor.unwrap_or(0),
                trace_id: out_trace,
                grain_id,
            });
        } else {
            let mut objects = Vec::with_capacity(object_refs.len());
            for id in object_refs {
                if let Some(entry) = self.object_store.get(id) {
                    objects.push((id, entry.as_bytes().to_vec()));
                }
            }
            let _ = tx[target_shard as usize].try_send(CrossShardMsg::DeliverMessageWithObjects {
                target_id,
                behavior_id,
                payload: args,
                objects,
                sender: self.current_actor.unwrap_or(0),
                trace_id: out_trace,
                grain_id,
            });
        }
        true
    }

    /// Send a message to a virtual actor (grain) identified by its stable
    /// `(grain_type, key)`. Hydrates the grain if it is not currently resident.
    /// In a sharded runtime, the grain's stable actor id determines the owning
    /// shard (`stable_id % shard_count`); messages are routed to that shard
    /// without hydrating locally.
    pub fn send_to_grain(
        &mut self,
        grain_id: GrainId,
        behavior_name: &str,
        args: Vec<Value>,
        sender: u64,
    ) {
        let stable_id = grain_actor_id(&grain_id);

        // Cross-shard routing: if the grain's stable id belongs to another
        // shard, resolve the behavior id from the grain type metadata and
        // forward the message without hydrating on this shard.
        if self.shard_count > 1 {
            let owner_shard = (stable_id % self.shard_count as u64) as u16;
            if owner_shard != self.shard_idx {
                let Some(behavior_id) = self.resolve_grain_behavior_id(&grain_id, behavior_name)
                else {
                    warn!(
                        "nulang-grain: unknown behavior {} for grain {}",
                        behavior_name,
                        grain_id.actor_name()
                    );
                    return;
                };
                let prev = self.current_actor;
                if sender != 0 {
                    self.current_actor = Some(sender);
                }
                let out_trace = self.current_trace.as_ref().map(|t| t.to_traceparent());
                self.send_cross_shard_message(
                    stable_id,
                    behavior_id,
                    args,
                    out_trace,
                    Some(grain_id.clone()),
                );
                self.current_actor = prev;
                return;
            }
        }

        let actor_id = match self.resolve_or_hydrate_grain(grain_id.clone()) {
            Ok(id) => id,
            Err(e) => {
                warn!(
                    "nulang-grain: failed to resolve grain {}: {}",
                    grain_id.actor_name(),
                    e
                );
                return;
            }
        };
        let Some(behavior_id) = self.behavior_id_for(actor_id, behavior_name) else {
            warn!(
                "nulang-grain: unknown behavior {} for grain {}",
                behavior_name,
                grain_id.actor_name()
            );
            return;
        };
        let prev = self.current_actor;
        if sender != 0 {
            self.current_actor = Some(sender);
        }
        self.send_message_by_id(actor_id, behavior_id, &args);
        self.current_actor = prev;
    }

    /// Send a message to a grain that is known to live on a specific remote
    /// node. This is the explicit cross-node routing path: the caller supplies
    /// the target node because the local runtime has no directory mapping from
    /// grain identity to hosting node in the MVP.
    ///
    /// If the grain happens to be resident locally (e.g. in single-node tests),
    /// delivery falls back to the local path.
    pub fn send_to_grain_on_node(
        &mut self,
        grain_id: GrainId,
        target_node: NodeId,
        behavior_name: &str,
        args: Vec<Value>,
        sender: u64,
    ) {
        let stable_id = grain_actor_id(&grain_id);
        let prev = self.current_actor;
        if sender != 0 {
            self.current_actor = Some(sender);
        }

        if self.actors.contains_key(&stable_id) {
            let Some(behavior_id) = self.behavior_id_for(stable_id, behavior_name) else {
                warn!(
                    "nulang-grain: unknown behavior {} for local grain {}",
                    behavior_name,
                    grain_id.actor_name()
                );
                self.current_actor = prev;
                return;
            };
            self.send_message_by_id(stable_id, behavior_id, &args);
            self.current_actor = prev;
            return;
        }

        let target = ActorAddress::remote(target_node, stable_id);
        self.send_distributed(target, behavior_name, &args);
        self.current_actor = prev;
    }

    #[tracing::instrument(level = "trace", skip(self, args))]
    pub fn send_message_by_id(&mut self, target_id: u64, behavior_id: u16, args: &[Value]) {
        // Stamp the outgoing message with the current handler's trace span (if
        // any), so the receiver's child span links directly to it and causal
        // chains continue across actor, shard, and node boundaries. The W3C
        // `traceparent` format carries only trace-id + span-id (no parent), so
        // the current span — not a synthetic child — must cross the wire; the
        // receiving side derives its own child. When no message is being
        // handled, the outgoing message starts a fresh trace on its own.
        let out_trace = self.current_trace.as_ref().map(|t| t.to_traceparent());
        // Cross-node routing by bare actor-ref value (RFC-0007 gap): a
        // spawn@node placeholder or reply-by-ref id whose hosting node we
        // know routes over the wire instead of the local mailbox. The
        // local-existence guard prefers local actors on id collision —
        // `fresh_actor_id` starts at 1 on EVERY node, so a remote id
        // numerically equal to a local actor must never hijack local
        // sends (a colliding remote ref is unreachable by bare value, an
        // inherent limit of the 48-bit actor-ref payload; explicit
        // `ActorAddress::remote` still works).
        if !self.actors.contains_key(&target_id) {
            if let Some(node) = self.remote_refs.get(&target_id).copied() {
                // The behavior name must cross the wire (the receiver
                // resolves it against ITS behavior table). Recover it from
                // the sender's module — the same table the VM's `Send`
                // opcode indexed.
                let Some(behavior_name) =
                    self.behavior_wire_name_for(self.current_actor, behavior_id)
                else {
                    warn!(
                        "nulang-net: dropping message to remote actor {} on node {:?}: cannot resolve behavior name (no sender module context)",
                        target_id, node
                    );
                    return;
                };
                self.route_ref_send(target_id, node, &behavior_name, args);
                return;
            }
        }
        // Forwarding for migrated actors: if this actor has been relocated
        // to another node, route the message there instead of bouncing it.
        if let Some(&(target_node, _migrated_at)) = self.migrated_actors.get(&target_id) {
            // Look up the behavior name from the recovery module.
            let behavior_name = self
                .recovery_modules
                .get(&target_id)
                .and_then(|(module, _, _)| {
                    module
                        .behaviors
                        .get(behavior_id as usize)
                        .map(|b| b.name.clone())
                })
                .unwrap_or_else(|| format!("behavior_{}", behavior_id));
            let target = ActorAddress::remote(target_node, target_id);
            self.send_distributed(target, &behavior_name, args);
            return;
        }
        // Cross-shard routing: if the target actor lives on another shard,
        // forward via the cross-shard channel. The receiving shard delivers it
        // through `deliver_cross_shard_message`.
        if self.shard_count > 1 {
            let target_shard = (target_id % self.shard_count as u64) as u16;
            if target_shard != self.shard_idx {
                self.send_cross_shard_message(
                    target_id,
                    behavior_id,
                    args.to_vec(),
                    out_trace.clone(),
                    None,
                );
                return;
            }
        }
        // Grain hydration: a resident grain actor that is hibernated should be
        // woken before the new message is delivered.
        if self.actor_grain_id.contains_key(&target_id) {
            if let Some(actor) = self.actors.get_mut(&target_id) {
                if actor.is_hibernated() {
                    if let Some(vm) = self.vm.as_mut() {
                        if let Err(e) = actor.wake_from_hibernation(vm) {
                            warn!(
                                "nulang-grain: failed to wake hibernated grain actor {}: {}",
                                target_id, e
                            );
                        }
                    } else {
                        warn!(
                            "nulang-grain: cannot wake hibernated grain actor {}: no VM",
                            target_id
                        );
                    }
                }
            }
        }

        // If the target is a known grain identity but is not currently
        // resident, hydrate it and retry local delivery once.
        if !self.actors.contains_key(&target_id) {
            if let Some(grain_id) = self.grain_actor_ids.get(&target_id).cloned() {
                if let Err(e) = self.resolve_or_hydrate_grain(grain_id) {
                    warn!(
                        "nulang-grain: failed to hydrate grain actor {}: {}",
                        target_id, e
                    );
                    self.route_to_dlq(
                        &Message {
                            behavior_id,
                            payload: Arc::new(args.to_vec()),
                            sender: self.current_actor.unwrap_or(0),
                            priority: MessagePriority::System,
                            trace_id: out_trace.clone(),
                        },
                        "grain hydration failed",
                    );
                    return;
                }
                if self.actors.contains_key(&target_id) {
                    self.deliver_local_message(target_id, behavior_id, args, out_trace);
                } else {
                    self.route_to_dlq(
                        &Message {
                            behavior_id,
                            payload: Arc::new(args.to_vec()),
                            sender: self.current_actor.unwrap_or(0),
                            priority: MessagePriority::System,
                            trace_id: out_trace.clone(),
                        },
                        "grain hydration failed",
                    );
                }
                return;
            }
        }

        self.deliver_local_message(target_id, behavior_id, args, out_trace);
    }

    /// Deliver a message to a local actor's mailbox, track cross-actor
    /// references, and wake a receive-wait if necessary.
    #[tracing::instrument(level = "trace", skip(self, args))]
    fn deliver_local_message(
        &mut self,
        target_id: u64,
        behavior_id: u16,
        args: &[Value],
        out_trace: Option<String>,
    ) {
        let msg = Message {
            behavior_id,
            payload: Arc::new(args.to_vec()),
            sender: self.current_actor.unwrap_or(0),
            priority: MessagePriority::Normal,
            trace_id: out_trace.clone(),
        };
        if let Some(actor) = self.actors.get_mut(&target_id) {
            actor
                .flight_recorder
                .record(self.current_actor.unwrap_or(0), behavior_id, args);
            if actor.mailbox.push_local(msg).is_ok() {
                // Activity resets the dehydration idle timer.
                actor.idle_ms = 0;
            } else {
                // Mailbox is full (capacity > 0). Route to DLQ with a simple notification.
                self.route_to_dlq(
                    &Message {
                        behavior_id,
                        payload: Arc::new(args.to_vec()),
                        sender: self.current_actor.unwrap_or(0),
                        priority: MessagePriority::System,
                        trace_id: out_trace.clone(),
                    },
                    "mailbox full",
                );
            }
        } else {
            self.route_to_dlq(
                &Message {
                    behavior_id,
                    payload: Arc::new(args.to_vec()),
                    sender: self.current_actor.unwrap_or(0),
                    priority: MessagePriority::System,
                    trace_id: out_trace.clone(),
                },
                "target actor not found",
            );
        }
        for arg in args {
            if let Some(ptr) = arg.as_ptr() {
                if ptr.is_null() {
                    continue;
                }
                if self.current_actor.is_some() {
                    // The true owner is recorded in the object's header: an
                    // actor forwarding a reference it received from a third
                    // actor must not be mistaken for the owner (that tripped
                    // the ownership assert in `send_ref_to` and registered
                    // the cycle-detector edge under the wrong actor).
                    // SAFETY: TAG_PTR values carry ActorHeap payload pointers
                    // with a uniform OrcaHeader layout; the sender holds a
                    // counted reference (a local ref or a receiver hold), so
                    // the heap is live - or retired - and the header valid.
                    let source_header = unsafe { crate::runtime::heap::ActorHeap::header_of(ptr) };
                    let owner_id = unsafe { (*source_header).actor_id };

                    if let Some(owner) = self.actors.get_mut(&owner_id) {
                        let op = unsafe { owner.orca_gc.send_ref_to(&owner.heap, ptr, target_id) };
                        self.coordinator.submit_op(op);
                    } else {
                        // The owner has exited: its heap is retired (kept
                        // alive by the sender's hold), so the header is
                        // still valid.  Bump the in-flight count directly
                        // and queue the decrement op; `process_gc_ops`
                        // applies it on the retired heap.
                        // SAFETY: as above; the single scheduler thread is
                        // the only mutator of any header.
                        unsafe { (*source_header).foreign_count += 1 };
                        self.coordinator.submit_op(ForeignRefOp {
                            target_actor: target_id,
                            owner_actor: owner_id,
                            object_header: source_header,
                            delta: -1,
                        });
                    }
                    // Register the cross-actor reference with the cycle detector.
                    // The receiving actor is represented by its pinned sentinel;
                    // the edge target_sentinel -> source_object records that the
                    // target actor holds a reference to the source object.
                    if self.actors.contains_key(&owner_id) && self.actors.contains_key(&target_id) {
                        if let Some(target_actor) = self.actors.get_mut(&target_id) {
                            if let Some(sentinel) = target_actor.cycle_sentinel() {
                                self.cycle_detector.register_foreign_ref(
                                    target_id,
                                    sentinel,
                                    owner_id,
                                    source_header,
                                );
                            }
                        }
                    }
                }
            }
        }
        self.enqueue_actor(target_id);
        // Wake an actor suspended in a timed selective receive: resume it
        // so the VM re-executes the ReceiveWait scan. A match resolves the
        // wait; otherwise the behavior re-suspends on its original deadline.
        // (An already-fired timeout is resolved by the timer-fire path.)
        let wake_for_receive = self
            .actors
            .get(&target_id)
            .map(|a| {
                a.suspended_execution.is_some()
                    && a.receive_wait.map(|w| !w.timed_out).unwrap_or(false)
            })
            .unwrap_or(false);
        if wake_for_receive {
            if self.vm_execution_depth > 0 {
                // A behavior is mid-flight on the shared runtime VM:
                // resuming the target now would nest a second
                // `vm.resume()` inside the running one and clobber the
                // shared frames. Defer the wake; `vm_exec_end` drains it
                // once the outermost VM call returns.
                if !self.pending_receive_wakes.contains(&target_id) {
                    self.pending_receive_wakes.push(target_id);
                }
            } else {
                self.resume_suspended_receive_wait(target_id);
            }
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub fn process_gc_ops(&mut self) {
        let ops = std::mem::take(&mut self.coordinator.pending_ops);
        for op in ops {
            // The owning actor is recorded on the op at send time.  Never
            // dereference `object_header` to discover the owner: if the
            // owner has exited its actor entry is gone, and reading the
            // header first would be a use-after-free once its heap drops.
            let source_header = op.object_header;
            let source_actor = op.owner_actor;

            // Remove the edge from the cycle detector graph before applying the
            // ORCA decrement so the graph stays consistent with the ref count.
            if let Some(target_actor) = self.actors.get_mut(&op.target_actor) {
                if let Some(sentinel) = target_actor.cycle_sentinel() {
                    self.cycle_detector.remove_foreign_ref(
                        op.target_actor,
                        sentinel,
                        source_actor,
                        source_header,
                    );
                }
            }

            // The ORCA operation must be applied on the *owning* actor's heap,
            // because that is where the object (and its header) lives.  Freeing
            // on the target actor's heap would corrupt the wrong allocator.
            if let Some(source_actor_ref) = self.actors.get_mut(&source_actor) {
                source_actor_ref
                    .orca_gc
                    .process_foreign_op(&mut source_actor_ref.heap, op);
            } else {
                // The owner has exited.  Its heap was retired (not freed)
                // precisely because this in-flight op kept the object's
                // foreign_count positive, so the header is still valid and
                // the decrement can be applied directly.  Individual objects
                // are not freed here; the whole retired heap is reclaimed by
                // `reclaim_retired_heaps` once all foreign refs drain.
                // SAFETY: retired heap memory stays mapped until every
                // foreign reference drains; the single scheduler thread is
                // the only mutator of any header.
                unsafe {
                    let header = &mut *source_header;
                    if op.delta >= 0 {
                        header.foreign_count += op.delta as u32;
                    } else {
                        header.foreign_count -= (-op.delta) as u32;
                    }
                }
            }
        }
        self.reclaim_retired_heaps();
        let should_detect = self.cycle_detector.should_detect();
        if should_detect {
            let local_ids: std::collections::HashSet<u64> = self.actors.keys().copied().collect();
            self.cycle_detector.set_local_actors(local_ids);
            // Take the detector out of `self` so it and the runtime are two
            // disjoint &mut borrows; `incremental_detect` only touches
            // `self.actors` via the CycleRuntime impl.
            let mut detector = std::mem::take(&mut self.cycle_detector);
            detector.incremental_detect(self);
            self.cycle_detector = detector;
        }
    }

    /// Return a snapshot of scheduler profiling statistics.
    pub fn scheduler_stats(&self) -> SchedulerStats {
        self.scheduler.stats()
    }

    /// Reset scheduler profiling statistics to zero.
    pub fn reset_scheduler_stats(&self) {
        self.scheduler.reset_stats()
    }

    pub fn gc_stats(&self) -> GcStats {
        let mut total = GcStats::default();
        for actor in self.actors.values() {
            let stats = actor.orca_gc.stats();
            total.objects_allocated += stats.objects_allocated;
            total.objects_freed += stats.objects_freed;
            total.local_refs_created += stats.local_refs_created;
            total.local_refs_dropped += stats.local_refs_dropped;
            total.foreign_refs_sent += stats.foreign_refs_sent;
            total.foreign_refs_received += stats.foreign_refs_received;
            total.cycles_detected += stats.cycles_detected;
            total.bytes_allocated += stats.bytes_allocated;
            total.bytes_freed += stats.bytes_freed;
        }
        total
    }
    /// Return the DLQ actor id, creating the DLQ actor if needed.
    /// The DLQ actor is intentionally never scheduled - messages accumulate
    /// in its mailbox for inspection via `dlq_depth()`.
    pub fn ensure_dlq_actor(&mut self) -> u64 {
        if let Some(id) = self.dlq_actor_id {
            if self.actors.contains_key(&id) {
                return id;
            }
        }
        let id = fresh_actor_id();
        let mut actor = Actor::new(id, "__dlq", 0);
        actor.set_state_field("count", Value::int(0));
        self.actors.insert(id, actor);
        self.dlq_actor_id = Some(id);
        id
    }

    /// Route an undeliverable message to the DLQ.
    /// The DLQ actor is never scheduled, so messages accumulate in its mailbox.
    pub fn route_to_dlq(&mut self, _msg: &Message, _reason: &str) {
        let dlq_id = self.ensure_dlq_actor();
        // Push a simple notification to the DLQ's mailbox directly.
        // We don't use send_message_by_id because it would try to ORCA-track args.
        if let Some(actor) = self.actors.get_mut(&dlq_id) {
            let _ = actor.mailbox.push_local(Message {
                behavior_id: 0,
                payload: Arc::new(vec![Value::int(1)]),
                sender: 0, // DLQ system message has no sender
                priority: MessagePriority::System,
                trace_id: None,
            });
        }
    }

    /// Number of messages currently queued in the DLQ actor's mailbox.
    pub fn dlq_depth(&self) -> usize {
        self.dlq_actor_id
            .and_then(|id| self.actors.get(&id))
            .map(|actor| actor.mailbox.len())
            .unwrap_or(0)
    }

    /// Number of living actors currently registered.
    pub fn actor_count(&self) -> usize {
        self.actors.len()
    }

    /// Snapshot of (actor_id, mailbox_depth) for each living actor.
    pub fn mailbox_depths(&self) -> Vec<(u64, usize)> {
        self.actors
            .iter()
            .map(|(id, actor)| (*id, actor.mailbox.len()))
            .collect()
    }

    /// Total tasks dequeued by all scheduler workers (lifetime counter).
    pub fn scheduler_processed_count(&self) -> usize {
        self.scheduler.processed_count()
    }

    /// Number of LLM calls currently in flight; always 0 without `ai-runtime`.
    #[cfg(feature = "ai-runtime")]
    fn llm_inflight_count(&self) -> usize {
        self.llm.inflight_count
    }
    #[cfg(not(feature = "ai-runtime"))]
    fn llm_inflight_count(&self) -> usize {
        0
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub fn run_scheduler(&mut self) {
        let mut ticks: u64 = 0;
        loop {
            // Drain any cross-shard messages before checking the local
            // scheduler queue. In-flight messages from other shards inject
            // actors into the local scheduler.
            self.drain_cross_shard_messages();
            let actor_id = match self.scheduler.dequeue() {
                Some(actor_id) => actor_id,
                None => {
                    if self.llm_inflight_count() == 0 && self.timer_wheel.is_empty() {
                        if let Some(ref mut cb) = self.idle_callback {
                            cb();
                        }
                        break;
                    }
                    // The run queue is drained but background LLM calls are
                    // still in flight or timers are pending: block briefly
                    // for the next completion or timer deadline so
                    // run_scheduler keeps its "run until quiescent"
                    // semantics - an actor whose last turn armed a timer
                    // must still receive the fired message.
                    let wait = match self.timer_wheel.next_deadline() {
                        Some(deadline) => deadline
                            .saturating_duration_since(self.now())
                            .min(std::time::Duration::from_millis(10)),
                        None => std::time::Duration::from_millis(10),
                    };
                    #[cfg(feature = "ai-runtime")]
                    match self.llm.rx.recv_timeout(wait) {
                        Ok((actor_id, result)) => {
                            self.store_llm_completion(actor_id, result);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    #[cfg(not(feature = "ai-runtime"))]
                    std::thread::sleep(wait);
                    // Deliver any timers that matured while waiting; fired
                    // messages re-enqueue their target actors, so the next
                    // dequeue resumes work.
                    self.tick_timers();
                    continue;
                }
            };
            #[cfg(feature = "ai-runtime")]
            self.poll_llm_completions();
            self.tick_timers();
            self.step_actor(actor_id);
            // Micro-batch: continue processing the same actor for a few more
            // messages to maximize L1 instruction-cache retention.  The
            // per-turn reduction budget (checked by should_yield) acts as
            // the safety limit - a hot actor that exhausts its budget will
            // be requeued behind other actors.
            const BATCH_SIZE: usize = 16;
            for _ in 1..BATCH_SIZE {
                let should_continue = self
                    .actors
                    .get(&actor_id)
                    .map(|a| {
                        !a.mailbox.is_empty()
                            && !a.should_yield()
                            && a.suspended_execution.is_none()
                    })
                    .unwrap_or(false);
                if !should_continue {
                    break;
                }
                self.step_actor(actor_id);
            }
            ticks += 1;
            if ticks % GC_PUMP_INTERVAL == 0 {
                // Safe at any cadence: process_deferred only frees objects
                // whose local and foreign counts have already reached zero.
                self.process_deferred_all();
            }
            if ticks % DEHYDRATE_CHECK_INTERVAL == 0 {
                self.dehydrate_idle_grains();
            }
        }
        // Deliver pending foreign-ref decrements and run cycle detection only
        // once the run queue has drained. Receiver-side holds now keep
        // `foreign_count` elevated for as long as a receiving actor holds a
        // pointer, so the -1 ops only release the *in-flight* count; applying
        // them mid-run is still deferred to keep mailbox pointers counted by
        // the in-flight bump until they are received (and held). Note: an
        // actor that yielded with a non-empty mailbox is re-enqueued, so a
        // drained queue implies drained mailboxes for terminating programs.
        self.process_gc_ops();
        self.process_deferred_all();
    }

    /// Run a distributed node: process network packets and execute the
    /// local scheduler in an infinite loop.  This method never returns
    /// under normal circumstances; the node terminates on SIGINT/SIGTERM.
    pub fn run_distributed_node(&mut self) {
        loop {
            self.process_network();
            self.run_scheduler();
            // run_scheduler returns when the local queue is quiescent.
            // Network packets may have arrived while we ran, so loop
            // again after a brief pause to avoid busy-waiting when
            // truly idle.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Pick the next actor to step, deterministically: the SORTED set of
    /// ready (non-empty-mailbox) actor ids, indexed by `rng`. `None` if no
    /// actor currently has a pending message.
    ///
    /// Split out from `run_scheduler_deterministic` so the selection
    /// policy itself is unit-testable against a fixed `Runtime` snapshot
    /// (same-seed same-sequence), independent of actually stepping
    /// actors -- which matters because `Actor::register_behavior` takes
    /// a bare `fn` pointer with no closure capture, so hand-registered
    /// test behaviors can't record an externally observable step
    /// sequence themselves.
    pub fn pick_ready_actor_deterministic(
        &self,
        rng: &mut crate::dst::DeterministicRng,
    ) -> Option<u64> {
        let mut ready: Vec<u64> = self
            .actors
            .iter()
            .filter(|(_, a)| !a.mailbox.is_empty())
            .map(|(id, _)| *id)
            .collect();
        if ready.is_empty() {
            return None;
        }
        // Sort first: HashMap iteration order is randomized per-process,
        // so the CANDIDATE list itself must be made deterministic before
        // the seeded pick, not just the pick's own randomness source.
        ready.sort_unstable();
        rng.pick(&ready).copied()
    }

    /// Run the scheduler deterministically: actor selection at each step
    /// goes through `pick_ready_actor_deterministic` instead of the real
    /// crossbeam work-stealing `Scheduler`. Reuses `step_actor` unchanged
    /// -- the SAME VM execution, GC, and persistence machinery the
    /// production scheduler drives -- so a bug caught here is a bug in
    /// the real actor runtime, not a simulated stand-in.
    ///
    /// Scope (PLAN.md Phase 1 bullet 2): pure message-passing + timer
    /// + cross-shard determinism. Does NOT drive `self.scheduler` (the
    /// crossbeam queue) or LLM completions -- a program that
    /// spawns/sends/asks/links/monitors between actors, arms timers
    /// (`send_after`), or uses timed receive-waits executes
    /// byte-identically for the same seed, provided a virtual clock is
    /// installed. Cross-shard messages (sharded `Runtime`s) are drained
    /// at the top of every loop iteration exactly like the production
    /// scheduler (`drain_cross_shard_messages`), so `new_sharded`
    /// runtimes are deterministic too. Timer determinism (2026-08-13):
    /// when no actor is ready but the timer wheel is non-empty, the
    /// scheduler advances the virtual clock to the next deadline and
    /// re-ticks, so a timer-armed program's timers actually fire instead
    /// of the run Quiescing forever with them pending (the
    /// pre-extension behavior). Clock advances count toward `max_steps`
    /// so a program that keeps re-arming timers cannot spin past the
    /// bound. WITHOUT a virtual clock the old contract holds: a
    /// timer-armed program stops making progress once its
    /// timer-waiting actor's mailbox empties and surfaces as `Quiescent`
    /// (timers are wall-clock-based and cannot be driven
    /// deterministically). Network-driven determinism (multi-node
    /// clusters over the in-memory transport) lives in
    /// `cluster_dst::DeterministicCluster` (test-gated), which pumps
    /// per-node deterministic scheduler runs interleaved with transport
    /// delivery.
    pub fn run_scheduler_deterministic(
        &mut self,
        seed: u64,
        max_steps: u64,
    ) -> DeterministicRunResult {
        let mut rng = crate::dst::DeterministicRng::new(seed);
        self.run_scheduler_deterministic_with_rng(&mut rng, max_steps)
    }

    /// Like [`Runtime::run_scheduler_deterministic`], but draws actor
    /// selection from a caller-owned RNG so a multi-node harness can
    /// interleave nodes and actors from one seeded sequence.
    pub fn run_scheduler_deterministic_with_rng(
        &mut self,
        rng: &mut crate::dst::DeterministicRng,
        max_steps: u64,
    ) -> DeterministicRunResult {
        let mut steps: u64 = 0;
        loop {
            if steps >= max_steps {
                return DeterministicRunResult::StepLimitExceeded { steps };
            }
            // Drain cross-shard messages first (mirrors the production
            // scheduler's `run_scheduler`); enqueued actors then show up
            // in `pick_ready_actor_deterministic`'s ready set.
            self.drain_cross_shard_messages();
            // Tick timers first (respects virtual clock via self.now())
            self.tick_timers();
            match self.pick_ready_actor_deterministic(rng) {
                Some(actor_id) => {
                    self.step_actor(actor_id);
                    steps += 1;
                    if steps % GC_PUMP_INTERVAL == 0 {
                        // Safe at any cadence: process_deferred only frees
                        // objects whose local and foreign counts have already
                        // reached zero. Mirrors the production
                        // `run_scheduler`'s per-batch pump so heap-churn DST
                        // scenarios exercise the same GC cadence.
                        self.process_deferred_all();
                    }
                    if steps % DEHYDRATE_CHECK_INTERVAL == 0 {
                        self.dehydrate_idle_grains();
                    }
                }
                None => {
                    // No actor is ready. With a virtual clock installed and
                    // timers pending, advance the clock to the next
                    // deadline and re-tick — the fired timer re-enqueues
                    // its target, so the next iteration makes progress.
                    // Without a virtual clock (or with an empty wheel)
                    // there is nothing deterministic left to do: Quiesce.
                    if self.virtual_clock.is_none() || self.timer_wheel.is_empty() {
                        // Deliver pending foreign-ref decrements and run
                        // cycle detection only once the run queue has
                        // drained — mirrors the production
                        // `run_scheduler`'s end-of-run drain. Receiver-side
                        // holds keep `foreign_count` elevated for as long as
                        // a receiving actor holds a pointer, so the -1 ops
                        // only release the *in-flight* count; applying them
                        // mid-run is still deferred to keep mailbox pointers
                        // counted by the in-flight bump until they are
                        // received (and held).
                        self.process_gc_ops();
                        self.process_deferred_all();
                        return DeterministicRunResult::Quiescent { steps };
                    }
                    let deadline = self
                        .timer_wheel
                        .next_deadline()
                        .expect("timer wheel non-empty implies a deadline");
                    let delta = deadline.saturating_duration_since(self.now());
                    if delta.is_zero() {
                        // Deadline already at/behind the clock but the tick
                        // at loop top didn't fire it (deadline raced the
                        // advance): nudge a minimal quantum to force the
                        // next tick to see it. Bounded by max_steps.
                        self.advance_time(std::time::Duration::from_millis(1));
                    } else {
                        self.advance_time(delta);
                    }
                    steps += 1;
                    if steps % DEHYDRATE_CHECK_INTERVAL == 0 {
                        self.dehydrate_idle_grains();
                    }
                }
            }
        }
    }

    /// Best-effort module hash for an actor, used to tag hibernation state.
    /// Falls back to an all-zero hash when no type hash is available.
    fn actor_module_hash(&self, actor_id: u64) -> [u8; 32] {
        self.actors
            .get(&actor_id)
            .and_then(|a| a.bytecode_module.as_ref())
            .and_then(|m| m.actor_metadata.iter().find_map(|m| m.type_hash))
            .unwrap_or([0u8; 32])
    }

    /// Build a serializable snapshot of an actor's durable state without
    /// updating the actor's own sequence/dirty tracking.
    fn build_actor_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        let mut state = std::collections::HashMap::new();
        let waiting_signal = {
            let actor = self.actors.get(&actor_id)?;
            for (name, value) in &actor.state_data {
                let model = actor
                    .state_models
                    .get(name)
                    .copied()
                    .unwrap_or(StateModel::Local);
                if model == StateModel::Durable || model.is_crdt() {
                    let persisted = if name == "semantic_memory" || name == "procedural_memory" {
                        #[cfg(feature = "ai-runtime")]
                        {
                            self.vm_value_to_string_in_actor(value, actor)
                                .map(PersistedValue::String)
                                .unwrap_or_else(|| {
                                    PersistedValue::from_value_resolved(
                                        value,
                                        actor.bytecode_module.as_ref(),
                                    )
                                })
                        }
                        #[cfg(not(feature = "ai-runtime"))]
                        {
                            PersistedValue::from_value_resolved(
                                value,
                                actor.bytecode_module.as_ref(),
                            )
                        }
                    } else {
                        PersistedValue::from_value_resolved(value, actor.bytecode_module.as_ref())
                    };
                    state.insert(name.clone(), persisted);
                }
            }
            actor.waiting_signal.clone()
        };
        let sequence = self.next_sequence(actor_id);
        let crdt_snapshot = self.crdt_manager.as_ref().map(|m| {
            m.snapshot()
                .into_iter()
                .map(|(id, (ty, bytes))| (id.0, ty.to_u8(), bytes))
                .collect()
        });
        Some(ActorSnapshot {
            actor_id,
            sequence,
            state,
            waiting_signal,
            crdt_snapshot,
        })
    }

    /// Scan resident grain actors and hibernate any that have been idle long
    /// enough according to their grain type's DehydratePolicy.
    pub(crate) fn dehydrate_idle_grains(&mut self) {
        let candidates: Vec<(u64, GrainId)> = self
            .actor_grain_id
            .iter()
            .map(|(&id, g)| (id, g.clone()))
            .collect();
        for (actor_id, grain_id) in candidates {
            let (may_dehydrate, policy_idle_ms) = {
                let Some(actor) = self.actors.get(&actor_id) else {
                    continue;
                };
                if actor.pinned || actor.is_hibernated() || actor.is_mid_execution() {
                    continue;
                }
                if !actor.mailbox.is_empty() {
                    continue;
                }
                let Some(grain_type) = self.grain_registry.get(&grain_id.grain_type) else {
                    continue;
                };
                let policy = grain_type.dehydrate_policy;
                if !policy.allow_dehydrate {
                    continue;
                }
                (true, policy.idle_ms)
            };
            if !may_dehydrate {
                continue;
            }

            {
                let Some(actor) = self.actors.get_mut(&actor_id) else {
                    continue;
                };
                actor.idle_ms += DEHYDRATE_CHECK_INTERVAL;
                if actor.idle_ms < policy_idle_ms {
                    continue;
                }
            }

            // Persist a snapshot of durable state before hibernating.
            let Some(snapshot) = self.build_actor_snapshot(actor_id) else {
                continue;
            };
            let sequence = snapshot.sequence;
            if let Err(e) = self.persistence.save_snapshot(snapshot) {
                warn!(
                    "nulang-grain: failed to save snapshot before dehydrating actor {}: {}",
                    actor_id, e
                );
                continue;
            }

            // Keep actor.sequence/dirty_fields consistent with the persisted snapshot.
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                actor.sequence = sequence;
                actor.dirty_fields.clear();
            }

            // Hibernate the actor. Its entry stays in `self.actors` so it
            // remains addressable and will be woken on the next send.
            let module_hash = self.actor_module_hash(actor_id);
            if self.vm.is_none() {
                self.vm = Some(crate::vm::VM::new());
            }
            let vm = self.vm.as_mut().unwrap();
            let hibernated = if let Some(actor) = self.actors.get_mut(&actor_id) {
                match actor.hibernate(vm, &module_hash) {
                    Ok(_) => true,
                    Err(ref e) if e == "No active frame" => {
                        // Idle grain with no in-flight behavior: record a
                        // state-only hibernation marker so the next send wakes
                        // it and starts a fresh behavior.
                        actor.hibernation_state = Some(crate::runtime::actor::HibernationState {
                            continuation_bytes: Vec::new(),
                            module_hash,
                            hibernated_at_ms: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64,
                            state_fields: actor.state_data.clone(),
                        });
                        true
                    }
                    Err(e) => {
                        warn!(
                            "nulang-grain: failed to hibernate actor {}: {}",
                            actor_id, e
                        );
                        false
                    }
                }
            } else {
                false
            };
            if hibernated {
                if let Some(actor) = self.actors.get_mut(&actor_id) {
                    actor.idle_ms = 0;
                }
            }
        }
    }

    /// Evict hibernated grain actors that have empty mailboxes and are not pinned.
    ///
    /// Eviction removes the actor from `self.actors` to reclaim heap memory while
    /// keeping the stable identity mapping in `grain_actor_ids`/`actor_grain_id`.
    /// The `grain_residents` entry is removed so that the next send re-hydrates
    /// the grain from its snapshot (or fresh type metadata). Pinned grains and
    /// grains with non-empty mailboxes are never evicted.
    ///
    /// Returns the number of actors evicted.
    pub fn evict_hibernated_grains(&mut self, max_evict: Option<usize>) -> usize {
        let candidates: Vec<(u64, GrainId)> = self
            .actor_grain_id
            .iter()
            .filter_map(|(&actor_id, grain_id)| {
                let actor = self.actors.get(&actor_id)?;
                if actor.is_hibernated() && !actor.pinned && actor.mailbox.is_empty() {
                    Some((actor_id, grain_id.clone()))
                } else {
                    None
                }
            })
            .collect();

        let mut evicted = 0;
        for (actor_id, grain_id) in candidates {
            if let Some(max) = max_evict {
                if evicted >= max {
                    break;
                }
            }
            // Remove the resident actor. Keep grain_actor_ids/actor_grain_id so
            // subsequent sends addressed by stable id still recognize the grain.
            self.actors.remove(&actor_id);
            self.grain_residents.remove(&grain_id);
            evicted += 1;
            tracing::debug!("nulang-grain: evicted hibernated actor {}", actor_id);
        }
        evicted
    }

    /// Heuristic memory-pressure hook: evict hibernated grains when the number
    /// of hibernated grain actors exceeds a threshold.
    ///
    /// This is a coarse-grained operator safety valve; production deployments
    /// will typically want to feed real RSS metrics into this decision.
    pub fn maybe_evict_under_pressure(&mut self) -> usize {
        const HIBERNATED_THRESHOLD: usize = 1000;
        let hibernated_count = self.actors.values().filter(|a| a.is_hibernated()).count();
        if hibernated_count > HIBERNATED_THRESHOLD {
            self.evict_hibernated_grains(None)
        } else {
            0
        }
    }

    /// Retry deferred local decrements on every actor's heap. Objects whose
    /// `foreign_count` has since dropped to zero are freed.
    fn process_deferred_all(&mut self) {
        for actor in self.actors.values_mut() {
            actor.orca_gc.process_deferred(&mut actor.heap);
        }
    }

    // -- ORCA receiver holds & retired heaps --

    /// Take a receiver-side ORCA hold for every heap pointer in a message
    /// payload that `receiver_id` has just popped from its mailbox.
    ///
    /// Each hold increments the owning object's `foreign_count`, so the
    /// object survives until the receiver exits - even if the sender drops
    /// its local references or exits first.  Holds are recorded on the
    /// receiver's `OrcaGc` and released by [`release_held_foreign_refs`].
    fn hold_payload_refs(&mut self, receiver_id: u64, payload: &[Value]) {
        for value in payload {
            if let Some(id) = value.as_object_id() {
                // Object-store ref: increment the node-local refcount and
                // record the hold on the receiving actor.
                self.object_store.clone_ref(id);
                if let Some(receiver) = self.actors.get_mut(&receiver_id) {
                    receiver.held_objects.insert(id);
                }
                continue;
            }
            let Some(ptr) = value.as_ptr() else { continue };
            if ptr.is_null() {
                continue;
            }
            // SAFETY: TAG_PTR values carry ActorHeap payload pointers with a
            // uniform OrcaHeader layout.  The pointer is valid because the
            // in-flight send bump (or the sender's local ref) keeps the
            // owning heap live - heaps with outstanding foreign refs are
            // retired, never freed.
            let header = unsafe { crate::runtime::heap::ActorHeap::header_of(ptr) };
            let owner_id = unsafe { (*header).actor_id };
            if let Some(owner) = self.actors.get_mut(&owner_id) {
                // SAFETY: `ptr` points to a live object owned by `owner_id`.
                unsafe { owner.orca_gc.inc_foreign_hold(&owner.heap, ptr) };
            } else {
                // The owner has exited: its heap is retired (kept alive by
                // the in-flight send bump), so bump the header directly.
                // SAFETY: as above; single scheduler thread.
                unsafe { (*header).foreign_count += 1 };
            }
            if let Some(receiver) = self.actors.get_mut(&receiver_id) {
                receiver.orca_gc.record_held_ref(owner_id, header);
            }
        }
    }

    /// Release every receiver-side foreign hold taken by `actor_id`.
    ///
    /// Called when the actor exits.  For a live owner the release goes
    /// through the owner's `OrcaGc` (which may free the object); for an
    /// exited owner the decrement is applied directly against its retired
    /// heap.  Idempotent: the hold list is drained on the first call.
    pub(crate) fn release_held_foreign_refs(&mut self, actor_id: u64) {
        let holds = match self.actors.get_mut(&actor_id) {
            Some(actor) => actor.orca_gc.take_held_refs(),
            None => return,
        };
        let object_ids: std::collections::HashSet<crate::runtime::object_store::ObjectId> =
            match self.actors.get_mut(&actor_id) {
                Some(actor) => std::mem::take(&mut actor.held_objects),
                None => std::collections::HashSet::new(),
            };
        for (owner_id, header) in holds {
            if let Some(owner) = self.actors.get_mut(&owner_id) {
                owner.orca_gc.process_foreign_op(
                    &mut owner.heap,
                    ForeignRefOp {
                        target_actor: actor_id,
                        owner_actor: owner_id,
                        object_header: header,
                        delta: -1,
                    },
                );
            } else {
                // SAFETY: the hold kept foreign_count > 0, so the owner's
                // heap was retired (not freed) and the header is valid.
                unsafe { (*header).foreign_count -= 1 };
            }
        }
        self.object_store.drop_refs(&object_ids);
        self.reclaim_retired_heaps();
    }

    /// True if any live object on `heap` still has foreign references.
    fn heap_has_outstanding_foreign_refs(heap: &ActorHeap) -> bool {
        let mut outstanding = false;
        heap.iter_live_objects(|header, _, _| {
            // SAFETY: iter_live_objects yields live headers on the scheduler
            // thread; no mutation happens during the scan.
            if unsafe { (*header).foreign_count } > 0 {
                outstanding = true;
            }
        });
        outstanding
    }

    /// Remove an actor from the runtime, releasing its receiver holds and
    /// deferring heap destruction while other actors still reference its
    /// objects.  A heap with outstanding foreign refs is moved into
    /// `retired_heaps` instead of being dropped, so in-flight ops and
    /// receiver holds held elsewhere never dangle.
    pub(crate) fn remove_actor_reaping(&mut self, actor_id: u64) {
        self.release_held_foreign_refs(actor_id);
        if let Some(mut actor) = self.actors.remove(&actor_id) {
            if Self::heap_has_outstanding_foreign_refs(&actor.heap) {
                // Swap in a fresh empty heap so `actor` drops cleanly; the
                // real heap moves to the retired list.
                let heap = std::mem::replace(&mut actor.heap, ActorHeap::new(64));
                self.retired_heaps.push(heap);
            }
        }
    }

    /// Drop retired heaps whose foreign references have all drained.
    ///
    /// Every foreign-count mutation on a retired heap goes through the
    /// runtime (direct bumps/decrements in `send_message_by_id`,
    /// `hold_payload_refs`, `release_held_foreign_refs`, and
    /// `process_gc_ops`), so scanning at those points is exact.
    fn reclaim_retired_heaps(&mut self) {
        if self.retired_heaps.is_empty() {
            return;
        }
        self.retired_heaps
            .retain(|heap| Self::heap_has_outstanding_foreign_refs(heap));
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub fn step_actor(&mut self, actor_id: u64) {
        self.current_actor = Some(actor_id);

        // If the actor yielded at a JIT safepoint, resume it inline before
        // attempting to receive the next message. The resume may complete the
        // behavior (clearing suspended_execution) or re-suspend (setting it
        // again), after which the normal suspended_execution guard below
        // prevents processing new messages while the behavior is live.
        let jit_yield = self
            .actors
            .get(&actor_id)
            .map(|a| a.jit_yield_pending)
            .unwrap_or(false);
        if jit_yield {
            self.resume_suspended_jit_yield(actor_id);
        }

        let msg_opt = {
            let actor = match self.actors.get_mut(&actor_id) {
                Some(a) => a,
                None => {
                    self.current_actor = None;
                    return;
                }
            };
            match actor.state {
                ActorState::Running | ActorState::Created | ActorState::Waiting => {
                    // A behavior suspended on a signal wait or a background
                    // LLM call owns the actor until it resumes: leave queued
                    // messages in the mailbox instead of running them over
                    // the suspension.  A second suspending behavior would
                    // overwrite `suspended_execution`, hijack the first
                    // call's completion (a single `llm_completed` slot), and
                    // lose the first behavior forever.  The resume paths
                    // re-enqueue the actor once the suspension resolves.
                    if actor.suspended_execution.is_some() {
                        None
                    } else {
                        actor.receive()
                    }
                }
                _ => {
                    self.current_actor = None;
                    return;
                }
            }
        };
        let should_requeue = if let Some(msg) = msg_opt {
            // Message delivery counts as activity for dehydration.
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                actor.idle_ms = 0;
            }
            let behavior_idx = msg.behavior_id as usize;

            // ORCA receiver protocol: hold every heap pointer in the
            // received payload so the owning objects (and any retired
            // owner heap) stay alive until this actor exits.
            self.hold_payload_refs(actor_id, &msg.payload);

            // Establish the W3C trace context for this message: a child of
            // the sender's span when the message carries a traceparent (so
            // causal chains continue), otherwise a fresh root. The context is
            // recorded on the runtime so sends performed by the handler below
            // stamp their outgoing traceparent as children of it. `_span_guard`
            // keeps the `tracing` span alive for the rest of this dispatch.
            let trace_ctx = match &msg.trace_id {
                Some(tp) => match TraceContext::from_traceparent(tp) {
                    Some(incoming) => incoming.child(),
                    None => TraceContext::root(),
                },
                None => TraceContext::root(),
            };
            self.current_trace = Some(trace_ctx);
            let _span_guard = trace_ctx.enter_dispatch_span(actor_id, behavior_idx);

            // Intercept semantic-memory behaviors generated by compile_agent.
            // They are bytecode behaviors but are implemented directly by the
            // runtime against the durable `semantic_memory` state field.
            #[cfg(feature = "ai-runtime")]
            let behavior_name = self.step_name_for(actor_id, behavior_idx);
            #[cfg(feature = "ai-runtime")]
            if self.actor_is_agent(actor_id) && self.is_semantic_memory_behavior(&behavior_name) {
                if self.actor_is_persistent(actor_id) {
                    let seq = self.next_sequence(actor_id);
                    let payload = msg.payload.iter().map(PersistedValue::from_value).collect();
                    let _ = self.persistence.append_journal(
                        actor_id,
                        JournalEntry {
                            sequence: seq,
                            behavior_id: msg.behavior_id,
                            payload,
                        },
                    );
                }
                let content = msg
                    .payload
                    .get(0)
                    .and_then(|v| {
                        self.actors
                            .get(&actor_id)
                            .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                    })
                    .unwrap_or_default();
                if behavior_name == "store_fact" {
                    self.semantic_memory_store(actor_id, &content);
                } else {
                    let query = content;
                    let top_k = msg.payload.get(1).and_then(|v| v.as_int()).unwrap_or(1) as usize;
                    self.semantic_memory_recall(actor_id, &query, top_k);
                }
                self.checkpoint_actor(actor_id);
                self.current_actor = None;
                return;
            }

            // Intercept procedural-memory behaviors generated by compile_agent.
            #[cfg(feature = "ai-runtime")]
            if self.actor_is_agent(actor_id) && self.is_procedural_memory_behavior(&behavior_name) {
                if self.actor_is_persistent(actor_id) {
                    let seq = self.next_sequence(actor_id);
                    let payload = msg.payload.iter().map(PersistedValue::from_value).collect();
                    let _ = self.persistence.append_journal(
                        actor_id,
                        JournalEntry {
                            sequence: seq,
                            behavior_id: msg.behavior_id,
                            payload,
                        },
                    );
                }
                match behavior_name.as_str() {
                    "store_pattern" => {
                        let key = msg
                            .payload
                            .get(0)
                            .and_then(|v| {
                                self.actors
                                    .get(&actor_id)
                                    .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                            })
                            .unwrap_or_default();
                        let input_pattern = msg
                            .payload
                            .get(1)
                            .and_then(|v| {
                                self.actors
                                    .get(&actor_id)
                                    .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                            })
                            .unwrap_or_default();
                        let output_template = msg
                            .payload
                            .get(2)
                            .and_then(|v| {
                                self.actors
                                    .get(&actor_id)
                                    .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                            })
                            .unwrap_or_default();
                        self.procedural_memory_store_pattern(
                            actor_id,
                            &key,
                            &input_pattern,
                            &output_template,
                        );
                    }
                    "get_pattern" => {
                        let key = msg
                            .payload
                            .get(0)
                            .and_then(|v| {
                                self.actors
                                    .get(&actor_id)
                                    .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                            })
                            .unwrap_or_default();
                        self.procedural_memory_get_pattern(actor_id, &key);
                    }
                    "add_example" => {
                        let task = msg
                            .payload
                            .get(0)
                            .and_then(|v| {
                                self.actors
                                    .get(&actor_id)
                                    .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                            })
                            .unwrap_or_default();
                        let input = msg
                            .payload
                            .get(1)
                            .and_then(|v| {
                                self.actors
                                    .get(&actor_id)
                                    .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                            })
                            .unwrap_or_default();
                        let output = msg
                            .payload
                            .get(2)
                            .and_then(|v| {
                                self.actors
                                    .get(&actor_id)
                                    .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                            })
                            .unwrap_or_default();
                        self.procedural_memory_add_example(actor_id, &task, &input, &output);
                    }
                    "get_examples" => {
                        let task = msg
                            .payload
                            .get(0)
                            .and_then(|v| {
                                self.actors
                                    .get(&actor_id)
                                    .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                            })
                            .unwrap_or_default();
                        let query = msg
                            .payload
                            .get(1)
                            .and_then(|v| {
                                self.actors
                                    .get(&actor_id)
                                    .and_then(|actor| self.vm_value_to_string_in_actor(v, actor))
                            })
                            .unwrap_or_default();
                        let top_k =
                            msg.payload.get(2).and_then(|v| v.as_int()).unwrap_or(1) as usize;
                        self.procedural_memory_get_examples(actor_id, &task, &query, top_k);
                    }
                    _ => {}
                }
                self.checkpoint_actor(actor_id);
                self.current_actor = None;
                return;
            }

            // Backend dispatch: WASM component actors are handled by the
            // component runtime, not the native VM.
            {
                let actor = match self.actors.get(&actor_id) {
                    Some(a) => a,
                    None => {
                        self.current_actor = None;
                        return;
                    }
                };
                if let crate::runtime::actor::ActorBackend::WasmComponent { .. } = &actor.backend {
                    self.current_actor = None;
                    return; // stub: WASM component runtime not yet integrated
                }
            }

            let handler_fn: Option<fn(&mut Actor, &[Value])> = {
                let actor = match self.actors.get(&actor_id) {
                    Some(a) => a,
                    None => {
                        self.current_actor = None;
                        return;
                    }
                };
                if behavior_idx < actor.behavior_table.len() {
                    Some(actor.behavior_table[behavior_idx].handler_fn)
                } else {
                    None
                }
            };
            // AOT target to arm around the handler (None for bytecode/native
            // handlers or behaviors without an AOT-compiled version).
            let aot_target = self
                .actors
                .get(&actor_id)
                .and_then(|a| a.aot_targets.get(behavior_idx))
                .and_then(|t| *t);
            let mut processed = false;
            let is_placeholder = self
                .actors
                .get(&actor_id)
                .and_then(|a| a.behavior_table.get(behavior_idx))
                .map(|e| e.name.is_empty())
                .unwrap_or(false);
            if let Some(handler) = handler_fn {
                if !is_placeholder {
                    // Journal the message before handling so recovery can replay it.
                    if self.actor_is_persistent(actor_id) {
                        let seq = self.next_sequence(actor_id);
                        let payload = msg.payload.iter().map(PersistedValue::from_value).collect();
                        let _ = self.persistence.append_journal(
                            actor_id,
                            JournalEntry {
                                sequence: seq,
                                behavior_id: msg.behavior_id,
                                payload,
                            },
                        );
                    }
                    let actor = match self.actors.get_mut(&actor_id) {
                        Some(a) => a,
                        None => {
                            self.current_actor = None;
                            return;
                        }
                    };
                    // Arm the AOT native target so `aot_behavior_adapter` (the
                    // behavior's handler) dispatches through AOT code.
                    if let Some(target) = aot_target {
                        crate::aot::set_aot_dispatch(Some(target));
                    }
                    handler(actor, &msg.payload);
                    if aot_target.is_some() {
                        crate::aot::clear_aot_dispatch();
                    }
                    // Snapshot durable state after the message is processed.
                    self.checkpoint_actor(actor_id);
                    processed = true;
                }
            }
            if !processed && self.has_bytecode_handler(actor_id, behavior_idx) {
                // Journal before executing bytecode as well.
                if self.actor_is_persistent(actor_id) {
                    let seq = self.next_sequence(actor_id);
                    let payload = msg.payload.iter().map(PersistedValue::from_value).collect();
                    let _ = self.persistence.append_journal(
                        actor_id,
                        JournalEntry {
                            sequence: seq,
                            behavior_id: msg.behavior_id,
                            payload,
                        },
                    );
                }
                let payload = msg.payload.clone();
                // Enable non-blocking LLM suspension for this
                // scheduler-driven behavior invocation. Nested synchronous
                // entry points (ask_actor_sync) force it back off.
                let saved_suspend = self.suspend_enabled;
                self.suspend_enabled = true;
                let result = self.run_bytecode_behavior(actor_id, behavior_idx, &payload);
                self.suspend_enabled = saved_suspend;
                match result {
                    Ok(_) => {
                        self.checkpoint_actor(actor_id);
                        processed = true;
                    }
                    Err(crate::types::NuError::Suspended(_)) => {
                        // The step yielded waiting for a signal or a
                        // background LLM call. Do not mark it completed, do
                        // not run compensations, and do not checkpoint the
                        // partially-mutated durable state: persist only the
                        // suspension marker so recovery can re-drive the
                        // step from its last pre-suspend checkpoint.
                        self.persist_suspension_marker(actor_id);
                        processed = false;
                    }
                    Err(e) => {
                        self.checkpoint_actor(actor_id);
                        // A workflow step failed: record the failure (durable
                        // StepFailed event — SPEC2 §10 known-issue #5: step
                        // failures were silent, exit 0, no diagnostic), then
                        // run saga compensations for previously completed
                        // steps in reverse order.
                        if self.actor_is_workflow(actor_id) {
                            let seq = self.next_sequence(actor_id);
                            let step_name = self.step_name_for(actor_id, behavior_idx);
                            let _ = self.persistence.append_workflow_event(
                                actor_id,
                                WorkflowEvent::StepFailed {
                                    sequence: seq,
                                    step_name,
                                    error: format!("{}", e),
                                },
                            );
                            self.run_saga_compensation(actor_id, behavior_idx);
                        }
                        processed = false;
                    }
                }
            }
            if processed
                && self.actor_is_workflow(actor_id)
                && !self.is_internal_behavior(actor_id, behavior_idx)
            {
                let seq = self.next_sequence(actor_id);
                let step_name = self.step_name_for(actor_id, behavior_idx);
                let _ = self.persistence.append_workflow_event(
                    actor_id,
                    WorkflowEvent::StepCompleted {
                        sequence: seq,
                        step_name,
                    },
                );
                // Synthetic parallel steps do not increment step_index in their
                // bytecode (so signal-waiting branches do not double-increment);
                // advance it here when the step completes.
                if self.is_parallel_step(actor_id, behavior_idx) {
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        if let Some(n) =
                            actor.get_state_field("step_index").and_then(|v| v.as_int())
                        {
                            actor.set_state_field("step_index", Value::int(n + 1));
                        }
                    }
                }
                self.checkpoint_actor(actor_id);
            }
            let actor = match self.actors.get_mut(&actor_id) {
                Some(a) => a,
                None => {
                    self.current_actor = None;
                    return;
                }
            };
            actor.increment_reductions(1);
            // Flush the selective-receive skip-buffer back to the normal
            // queue so the next turn starts clean and is_empty() correctly
            // reflects pending messages.
            actor.mailbox.flush_skip_buffer();
            if actor.mailbox.is_empty() {
                // Turn over: next scheduling starts with a fresh budget.
                actor.reset_reductions();
                false
            } else if actor.should_yield() {
                // Reduction budget exhausted with mail pending: yield -
                // reset the counter and requeue at the back of the
                // scheduler queue so other actors get a turn first.
                actor.reset_reductions();
                true
            } else {
                true
            }
        } else {
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                if actor.state == ActorState::Running {
                    actor.state = ActorState::Waiting;
                }
                // Waiting actors start their next turn with a fresh budget.
                actor.reset_reductions();
            }
            false
        };
        if should_requeue {
            self.enqueue_actor(actor_id);
        }
        self.current_actor = None;
    }

    fn actor_is_persistent(&self, actor_id: u64) -> bool {
        self.actors
            .get(&actor_id)
            .map(|a| a.persistent)
            .unwrap_or(false)
    }

    fn actor_is_workflow(&self, actor_id: u64) -> bool {
        workflow::actor_is_workflow(self, actor_id)
    }

    #[cfg(feature = "ai-runtime")]
    fn actor_is_agent(&self, actor_id: u64) -> bool {
        self.actors
            .get(&actor_id)
            .map(|a| a.is_agent)
            .unwrap_or(false)
    }

    /// Return true if the behavior name is a semantic-memory behavior generated
    /// by `compile_agent` for agents configured with `semantic_memory`.
    #[cfg(feature = "ai-runtime")]
    fn is_semantic_memory_behavior(&self, name: &str) -> bool {
        name == "store_fact" || name == "recall"
    }

    /// Read an agent's durable `semantic_memory` state field as a `SemanticMemory`.
    #[cfg(feature = "ai-runtime")]
    fn read_semantic_memory(&self, actor: &Actor) -> Option<nulang_ai::SemanticMemory> {
        let value = actor.get_state_field("semantic_memory")?;
        let ptr = value.as_ptr()?;
        if ptr.is_null() {
            return None;
        }
        let json = unsafe {
            std::ffi::CStr::from_ptr(ptr as *const std::ffi::c_char)
                .to_string_lossy()
                .into_owned()
        };
        serde_json::from_str(&json).ok()
    }

    /// Write a `SemanticMemory` back to an agent's durable `semantic_memory` state field.
    #[cfg(feature = "ai-runtime")]
    fn write_semantic_memory(actor: &mut Actor, memory: &nulang_ai::SemanticMemory) {
        if let Ok(json) = serde_json::to_string(memory) {
            let ptr = actor.allocate_string(&json);
            actor.set_state_field("semantic_memory", ptr);
        }
    }

    /// Convert a VM value into a Rust string, reading pointer payloads as
    /// null-terminated UTF-8 and string-id values via the actor's bytecode module.
    #[cfg(feature = "ai-runtime")]
    fn vm_value_to_string_in_actor(
        &self,
        value: &crate::vm::Value,
        actor: &Actor,
    ) -> Option<String> {
        if let Some(id) = value.as_string_id() {
            actor
                .bytecode_module
                .as_ref()
                .and_then(|m| m.constants.get(id as usize))
                .and_then(|c| match c {
                    crate::bytecode::Constant::String(s) => Some(s.clone()),
                    _ => None,
                })
        } else if let Some(ptr) = value.as_ptr() {
            if ptr.is_null() {
                Some(String::new())
            } else {
                Some(unsafe {
                    std::ffi::CStr::from_ptr(ptr as *const std::ffi::c_char)
                        .to_string_lossy()
                        .into_owned()
                })
            }
        } else {
            None
        }
    }

    /// Store a fact in an agent's semantic memory and return the document id.
    #[cfg(feature = "ai-runtime")]
    fn semantic_memory_store(&mut self, actor_id: u64, content: &str) -> crate::vm::Value {
        self.semantic_memory_store_with_metadata(
            actor_id,
            content,
            std::collections::HashMap::new(),
        )
    }

    /// Store a fact with metadata in an agent's semantic memory and return the document id.
    #[cfg(feature = "ai-runtime")]
    fn semantic_memory_store_with_metadata(
        &mut self,
        actor_id: u64,
        content: &str,
        metadata: std::collections::HashMap<String, String>,
    ) -> crate::vm::Value {
        let memory_opt = if let Some(actor) = self.actors.get(&actor_id) {
            self.read_semantic_memory(actor)
        } else {
            None
        };
        let mut memory = memory_opt.unwrap_or_else(|| nulang_ai::SemanticMemory::new(64, None));
        let id = memory.store(content, metadata);
        if let Some(actor) = self.actors.get_mut(&actor_id) {
            Self::write_semantic_memory(actor, &memory);
            return actor.allocate_string(&id);
        }
        crate::vm::Value::nil()
    }

    /// Search an agent's semantic memory and return the top result's content.
    #[cfg(feature = "ai-runtime")]
    fn semantic_memory_recall(
        &mut self,
        actor_id: u64,
        query: &str,
        top_k: usize,
    ) -> crate::vm::Value {
        let content = if let Some(actor) = self.actors.get(&actor_id) {
            self.read_semantic_memory(actor).and_then(|memory| {
                let results = memory.search(query, top_k);
                results.first().map(|(doc, _)| doc.content.clone())
            })
        } else {
            None
        };
        if let Some(content) = content {
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                return actor.allocate_string(&content);
            }
        }
        crate::vm::Value::nil()
    }

    // -------------------------------------------------------------------------
    // Procedural memory helpers
    // -------------------------------------------------------------------------

    /// Return true if the behavior name is a procedural-memory behavior generated
    /// by `compile_agent` for agents configured with `procedural_memory`.
    #[cfg(feature = "ai-runtime")]
    fn is_procedural_memory_behavior(&self, name: &str) -> bool {
        matches!(
            name,
            "store_pattern" | "get_pattern" | "add_example" | "get_examples"
        )
    }

    /// Read an agent's durable `procedural_memory` state field as a `ProceduralMemory`.
    #[cfg(feature = "ai-runtime")]
    fn read_procedural_memory(&self, actor: &Actor) -> Option<nulang_ai::ProceduralMemory> {
        let value = actor.get_state_field("procedural_memory")?;
        let ptr = value.as_ptr()?;
        if ptr.is_null() {
            return None;
        }
        let json = unsafe {
            std::ffi::CStr::from_ptr(ptr as *const std::ffi::c_char)
                .to_string_lossy()
                .into_owned()
        };
        serde_json::from_str(&json).ok()
    }

    /// Write a `ProceduralMemory` back to an agent's durable `procedural_memory` state field.
    #[cfg(feature = "ai-runtime")]
    fn write_procedural_memory(actor: &mut Actor, memory: &nulang_ai::ProceduralMemory) {
        if let Ok(json) = serde_json::to_string(memory) {
            let ptr = actor.allocate_string(&json);
            actor.set_state_field("procedural_memory", ptr);
        }
    }

    /// Store a pattern in an agent's procedural memory and return the key.
    #[cfg(feature = "ai-runtime")]
    fn procedural_memory_store_pattern(
        &mut self,
        actor_id: u64,
        key: &str,
        input_pattern: &str,
        output_template: &str,
    ) -> crate::vm::Value {
        let memory_opt = self
            .actors
            .get(&actor_id)
            .and_then(|actor| self.read_procedural_memory(actor));
        let mut memory = memory_opt.unwrap_or_else(|| nulang_ai::ProceduralMemory::new("default"));
        let key = memory.store_pattern(key, input_pattern, output_template);
        if let Some(actor) = self.actors.get_mut(&actor_id) {
            Self::write_procedural_memory(actor, &memory);
            return actor.allocate_string(&key);
        }
        crate::vm::Value::nil()
    }

    /// Retrieve a pattern by key from an agent's procedural memory.
    #[cfg(feature = "ai-runtime")]
    fn procedural_memory_get_pattern(&mut self, actor_id: u64, key: &str) -> crate::vm::Value {
        let content = self
            .actors
            .get(&actor_id)
            .and_then(|actor| self.read_procedural_memory(actor))
            .and_then(|memory| memory.get_pattern(key).map(|p| p.output_template.clone()));
        if let Some(content) = content {
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                return actor.allocate_string(&content);
            }
        }
        crate::vm::Value::nil()
    }

    /// Add a few-shot example to an agent's procedural memory.
    #[cfg(feature = "ai-runtime")]
    fn procedural_memory_add_example(
        &mut self,
        actor_id: u64,
        task: &str,
        input: &str,
        output: &str,
    ) -> crate::vm::Value {
        let memory_opt = self
            .actors
            .get(&actor_id)
            .and_then(|actor| self.read_procedural_memory(actor));
        let mut memory = memory_opt.unwrap_or_else(|| nulang_ai::ProceduralMemory::new("default"));
        memory.add_example(task, input, output);
        if let Some(actor) = self.actors.get_mut(&actor_id) {
            Self::write_procedural_memory(actor, &memory);
        }
        crate::vm::Value::nil()
    }

    /// Retrieve the top-k examples for a task/query from an agent's procedural memory.
    #[cfg(feature = "ai-runtime")]
    fn procedural_memory_get_examples(
        &mut self,
        actor_id: u64,
        task: &str,
        query: &str,
        top_k: usize,
    ) -> crate::vm::Value {
        let examples = self
            .actors
            .get(&actor_id)
            .and_then(|actor| self.read_procedural_memory(actor))
            .map(|memory| memory.get_examples(task, query, top_k));
        if let Some(examples) = examples {
            let formatted = examples
                .iter()
                .map(|example| format!("IN: {}\nOUT: {}", example.input, example.output))
                .collect::<Vec<_>>()
                .join("\n---\n");
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                return actor.allocate_string(&formatted);
            }
        }
        crate::vm::Value::nil()
    }

    fn has_bytecode_handler(&self, actor_id: u64, behavior_idx: usize) -> bool {
        self.actors
            .get(&actor_id)
            .map(|a| a.bytecode_module.is_some() && behavior_idx < a.bytecode_offsets.len())
            .unwrap_or(false)
    }

    fn next_sequence(&self, actor_id: u64) -> u64 {
        workflow::next_sequence(self, actor_id)
    }

    /// Schedule a durable timer for a workflow actor.
    ///
    /// Appends a `TimerSet` event, checkpoints state, and arms the runtime's
    /// timer wheel. When the timer fires the runtime will append a
    /// `TimerFired` event and deliver a `__timer_fired` message to the actor.
    pub fn schedule_workflow_timer(&mut self, actor_id: u64, name: &str, duration_ms: u64) {
        workflow::schedule_workflow_timer(self, actor_id, name, duration_ms)
    }

    /// Re-arm a timer from the durable journal without appending a new event.
    /// Used during recovery to restore timers that have not yet fired.
    pub(crate) fn rearm_timer(&mut self, actor_id: u64, name: &str, duration_ms: u64) {
        let behavior_id = self.behavior_id_for(actor_id, "__timer_fired").unwrap_or(0);
        self.timer_wheel.send_after_with_context(
            std::time::Duration::from_millis(duration_ms),
            actor_id,
            behavior_id,
            vec![],
            name.to_string(),
        );
    }

    /// Return the current logical time: the virtual clock's view if one is
    /// installed, otherwise real wall-clock time.
    pub fn now(&self) -> std::time::Instant {
        match &self.virtual_clock {
            Some(vc) => vc.now(),
            None => std::time::Instant::now(),
        }
    }

    /// Install a virtual clock, freezing time at the current wall-clock
    /// moment. All subsequent timer expiry and deadline calculations use
    /// this clock. Call `advance_time` to move time forward.
    pub fn install_virtual_clock(&mut self) {
        self.virtual_clock = Some(VirtualClock::new());
    }

    /// Advance the virtual clock by `duration`. Timers whose fire time lies
    /// at or before the new virtual time will fire on the next scheduler
    /// iteration. Panics if no virtual clock is installed.
    pub fn advance_time(&mut self, duration: std::time::Duration) {
        match &mut self.virtual_clock {
            Some(vc) => {
                vc.advance(duration);
                // Also advance the cluster's clock for deterministic membership/gossip
                if let Some(cluster) = &mut self.distributed.cluster {
                    cluster.set_clock(vc.clone());
                }
            }
            None => warn!("advance_time called without a virtual clock installed; ignoring"),
        }
    }

    /// Remove the virtual clock, returning to real wall-clock time.
    pub fn remove_virtual_clock(&mut self) {
        self.virtual_clock = None;
    }

    /// Tick the timer wheel and deliver any fired timers.
    pub fn tick_timers(&mut self) {
        self.tick_timers_at(self.now());
    }

    // -- Timed selective receive (receive-after) --

    /// Arm the timeout for an actor's first receive-wait suspension.
    ///
    /// Called at every suspend-capture site with the timeout the VM staged
    /// in `suspended_receive_timeout`. A re-suspension of the SAME wait
    /// (a wake found no matching message) must not restart the clock, so
    /// the timer is scheduled only when the actor has no live receive-wait
    /// state; the original deadline stands.
    fn maybe_schedule_receive_wait(&mut self, actor_id: u64, timeout_ms: Option<i64>) {
        let Some(ms) = timeout_ms else { return };
        if ms <= 0 {
            return;
        }
        let already_waiting = self
            .actors
            .get(&actor_id)
            .map(|a| a.receive_wait.is_some())
            .unwrap_or(false);
        if already_waiting {
            return;
        }
        let timer_id = self
            .timer_wheel
            .receive_wait_timeout(std::time::Duration::from_millis(ms as u64), actor_id);
        if let Some(actor) = self.actors.get_mut(&actor_id) {
            actor.receive_wait = Some(crate::runtime::actor::ReceiveWaitState {
                timer_id,
                timed_out: false,
            });
        }
    }

    /// Drop an actor's receive-wait state once the wait has resolved,
    /// cancelling the timeout timer if it is still pending. Called on every
    /// terminal outcome of a resumed receive-suspended behavior (the match
    /// path cancels earlier, via `receive_wait_matched`).
    fn clear_receive_wait(&mut self, actor_id: u64) {
        let wait = self
            .actors
            .get_mut(&actor_id)
            .and_then(|a| a.receive_wait.take());
        if let Some(wait) = wait {
            self.timer_wheel.cancel(wait.timer_id);
        }
    }

    /// A receive-wait timeout timer fired: mark the actor's wait as timed
    /// out and resume its suspended behavior. The re-executed `ReceiveWait`
    /// consumes the marker, writes the no-match sentinel, and continues
    /// into the after body.
    fn fire_receive_wait_timeout(&mut self, actor_id: u64) {
        let has_suspension = self
            .actors
            .get(&actor_id)
            .map(|a| a.suspended_execution.is_some())
            .unwrap_or(false);
        if !has_suspension {
            // Nothing to wake (actor exited or the wait already resolved):
            // drop any stale wait state instead of poisoning a later wait.
            self.clear_receive_wait(actor_id);
            return;
        }
        if let Some(actor) = self.actors.get_mut(&actor_id) {
            if let Some(wait) = actor.receive_wait.as_mut() {
                wait.timed_out = true;
            }
        }
        self.resume_suspended_receive_wait(actor_id);
    }

    /// A `Timer.sleep` timer fired: mark the actor's sleep flag and resume
    /// its suspended PerformAsync behavior.  On re-execution the PerformAsync
    /// callback sees `timer_sleep_fired == true` and returns Ready.
    ///
    /// This does a full VM resume (not just a flag + enqueue) so that
    /// Timer.sleep works without the AI runtime feature.  Previously it only
    /// set a flag and relied on `poll_llm_completions` (ai-runtime only) to
    /// resume - the single-arg form permanently hung without that feature.
    fn fire_timer_sleep_wake(&mut self, actor_id: u64) {
        if let Some(actor) = self.actors.get_mut(&actor_id) {
            actor.timer_sleep_fired = true;
        }
        // Resume the suspended PerformAsync execution so the re-executed
        // opcode sees the flag and completes the sleep.  Modeled on
        // resume_suspended_jit_yield without the JIT safepoint logic.
        let suspended = match self.actors.get_mut(&actor_id) {
            Some(actor) => actor.suspended_execution.take(),
            None => return,
        };
        let Some(suspended) = suspended else {
            // Not currently suspended - nothing to resume.
            return;
        };
        if self.vm.is_none() {
            // VM not available; restore suspension and requeue.
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                actor.suspended_execution = Some(suspended);
            }
            self.scheduler.enqueue(actor_id);
            return;
        }
        let self_ptr: *mut Runtime = self;
        unsafe {
            let vm = (*self_ptr).vm.as_mut().unwrap();
            vm.set_actor_callbacks(Box::new(BytecodeRuntimeCallbacks::new(self_ptr, actor_id)));
            vm.set_distributed_callbacks(Box::new(BytecodeDistributedCallbacks {
                runtime: self_ptr,
            }));
            vm.restore_suspended_state(suspended.vm_state);
            (*self_ptr).vm_exec_begin();
            let result = vm.resume();
            // Distinguish completion from re-suspension by the resume
            // RESULT, not `take_suspended_state`: after a normal
            // completion the frame is still live, so take_suspended_state
            // returns the completed state and a blind re-capture would
            // re-install it as a fresh suspension (permanent stall).
            // Mirrors resume_suspended_llm_step.
            match result {
                Ok(_) => {
                    // The sleeping step ran to completion. For workflow
                    // actors record the completion the same way the other
                    // resume paths do: advance step_index, append
                    // StepCompleted, and checkpoint. Without this the
                    // step's body finishes but the workflow never advances
                    // — SPEC2 known-issue #4's permanent stall.
                    if (*self_ptr).actor_is_workflow(actor_id) {
                        if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                            if let Some(n) =
                                actor.get_state_field("step_index").and_then(|v| v.as_int())
                            {
                                actor.set_state_field("step_index", Value::int(n + 1));
                            }
                        }
                        let seq = (*self_ptr).next_sequence(actor_id);
                        let _ = (*self_ptr).persistence.append_workflow_event(
                            actor_id,
                            crate::runtime::WorkflowEvent::StepCompleted {
                                sequence: seq,
                                step_name: suspended.step_name.clone(),
                            },
                        );
                        (*self_ptr).checkpoint_actor(actor_id);
                    }
                }
                Err(crate::types::NuError::Suspended(_)) => {
                    // Re-suspended (e.g. a chained Timer.sleep): re-capture
                    // the VM state so the next timer fire can resume it.
                    if let Some(vm_state) = vm.take_suspended_state() {
                        if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                            actor.suspended_execution =
                                Some(crate::runtime::actor::SuspendedExecution {
                                    vm_state,
                                    behavior_idx: suspended.behavior_idx,
                                    step_name: suspended.step_name.clone(),
                                });
                        }
                    }
                }
                Err(e) => {
                    // VM error during resume - log and clean up.
                    tracing::warn!("Timer.sleep resume error for actor {}: {:?}", actor_id, e);
                    if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                        actor.suspended_execution = None;
                    }
                }
            }
            (*self_ptr).vm_exec_end();
        }
        // Re-enqueue so the scheduler can continue processing the actor.
        self.scheduler.enqueue(actor_id);
    }

    /// Resume an actor whose bytecode behavior suspended on a timed
    /// selective receive (`receive ... after ms =>`). Called when a message
    /// was pushed to the actor's mailbox (the re-scan may match) or when
    /// the wait's timer fired (the wait resolves with the no-match
    /// sentinel). Mirrors `resume_suspended_llm_step`: the actor's
    /// callbacks are re-installed on the shared VM before `vm.resume()`.
    fn resume_suspended_receive_wait(&mut self, actor_id: u64) {
        let suspended = match self.actors.get_mut(&actor_id) {
            Some(actor) => actor.suspended_execution.take(),
            None => return,
        };
        let Some(suspended) = suspended else { return };

        if self.vm.is_none() {
            // No VM available; put the suspension back so a later wake can
            // re-trigger it.
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                actor.suspended_execution = Some(suspended);
            }
            return;
        }

        let self_ptr: *mut Runtime = self;
        unsafe {
            let vm = (*self_ptr).vm.as_mut().unwrap();
            // Re-install callbacks bound to THIS actor: other actors may have
            // run on the shared VM while this one was suspended.
            vm.set_distributed_callbacks(Box::new(BytecodeDistributedCallbacks {
                runtime: self_ptr,
            }));
            vm.set_actor_callbacks(Box::new(BytecodeRuntimeCallbacks::new(self_ptr, actor_id)));
            vm.restore_suspended_state(suspended.vm_state);
            // A resumed behavior is still scheduler-context execution: a
            // `perform LLM.ask` after the wait must suspend (non-blocking).
            let saved_suspend = (*self_ptr).suspend_enabled;
            (*self_ptr).suspend_enabled = true;
            (*self_ptr).vm_exec_begin();
            let result = vm.resume();
            (*self_ptr).suspend_enabled = saved_suspend;
            match result {
                Ok(_) => {
                    // The wait resolved (match or timeout) and the behavior
                    // ran to completion: drop any leftover wait state and
                    // record workflow completion like the LLM resume path.
                    (*self_ptr).clear_receive_wait(actor_id);
                    if (*self_ptr).actor_is_workflow(actor_id) {
                        if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                            actor.waiting_signal = None;
                            if let Some(n) =
                                actor.get_state_field("step_index").and_then(|v| v.as_int())
                            {
                                actor.set_state_field("step_index", Value::int(n + 1));
                            }
                        }
                        let seq = (*self_ptr).next_sequence(actor_id);
                        let _ = (*self_ptr).persistence.append_workflow_event(
                            actor_id,
                            WorkflowEvent::StepCompleted {
                                sequence: seq,
                                step_name: suspended.step_name,
                            },
                        );
                        (*self_ptr).checkpoint_actor(actor_id);
                    }
                }
                Err(crate::types::NuError::Suspended(VmSuspension::ReceiveWait)) => {
                    // Re-suspended on the same wait (the waking message did
                    // not match): keep the original timer and re-capture the
                    // VM state so the next message or the timeout can resume
                    // it. maybe_schedule_receive_wait is a no-op while the
                    // wait state is live, so the deadline is not restarted.
                    if let Some(vm_state) = vm.take_suspended_state() {
                        let timeout = vm.suspended_receive_timeout.take();
                        if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                            actor.suspended_execution =
                                Some(crate::runtime::actor::SuspendedExecution {
                                    vm_state,
                                    behavior_idx: suspended.behavior_idx,
                                    step_name: suspended.step_name,
                                });
                        }
                        (*self_ptr).maybe_schedule_receive_wait(actor_id, timeout);
                    }
                }
                Err(crate::types::NuError::Suspended(_)) => {
                    // Suspended on something else (a signal wait or a
                    // background LLM call) past the receive: the wait is
                    // over. Re-capture so the matching signal or pumped
                    // completion can resume the behavior.
                    (*self_ptr).clear_receive_wait(actor_id);
                    if let Some(vm_state) = vm.take_suspended_state() {
                        let signal_name = vm.suspended_signal_name.take();
                        if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                            let marker = suspension_marker(actor, signal_name);
                            actor.waiting_signal = marker;
                            actor.suspended_execution =
                                Some(crate::runtime::actor::SuspendedExecution {
                                    vm_state,
                                    behavior_idx: suspended.behavior_idx,
                                    step_name: suspended.step_name,
                                });
                        }
                    }
                }
                // Other errors: the wait is over; the send-path result is
                // discarded anyway, matching step_actor semantics.
                Err(_) => (*self_ptr).clear_receive_wait(actor_id),
            }
            // End the VM-execution window only after any suspend-state
            // re-capture above: draining deferred wakes runs other actors
            // on the shared VM, which would clobber the frames an
            // un-captured suspend still needs. Runs on every path, so
            // wakes of other actors are not lost when THIS one suspends.
            (*self_ptr).vm_exec_end();
        }
        // The suspension resolved (completed or failed): if messages queued
        // up while the behavior was suspended, schedule the actor to drain
        // them - step_actor leaves mail untouched while a suspension is live.
        self.requeue_if_mail_pending(actor_id);
    }

    fn tick_timers_at(&mut self, now: std::time::Instant) {
        let fired = self.timer_wheel.tick(now);
        for (target_actor, message) in fired {
            match message {
                TimerMessage::SendWithContext {
                    behavior_id,
                    payload,
                    context,
                } => {
                    if self.actor_is_workflow(target_actor) {
                        let _ = self.append_timer_fired(target_actor, &context);
                    }
                    self.send_message_by_id(target_actor, behavior_id, &payload);
                }
                TimerMessage::Send {
                    behavior_id,
                    payload,
                } => {
                    self.send_message_by_id(target_actor, behavior_id, &payload);
                }
                TimerMessage::Exit { reason } => {
                    self.exit_actor(target_actor, ExitReason::Error(reason));
                }
                TimerMessage::Kill => {
                    self.kill_actor(target_actor);
                }
                TimerMessage::ReceiveWaitTimeout => {
                    self.fire_receive_wait_timeout(target_actor);
                }
                TimerMessage::TimerSleepWake => {
                    self.fire_timer_sleep_wake(target_actor);
                }
                TimerMessage::LlmRetry => {
                    #[cfg(feature = "ai-runtime")]
                    self.handle_llm_retry_timer(target_actor);
                }
            }
        }
    }

    /// Snapshot durable fields of an actor to the persistence store.
    /// The snapshot is skipped entirely when no fields have changed since
    /// the last checkpoint (dirty-bit optimization).
    pub fn checkpoint_actor(&mut self, actor_id: u64) {
        workflow::checkpoint_actor(self, actor_id)
    }

    /// Persist only the suspension marker of a persistent actor whose
    /// bytecode behavior has just suspended (signal wait or background LLM
    /// call), without snapshotting the step's partially-mutated durable
    /// state.  Recovery reads the marker (`waiting_signal`, or the
    /// `LLM_SUSPEND_MARKER` sentinel for LLM suspends) to decide that the
    /// in-flight step must be re-driven; the state it re-runs from is the
    /// last pre-step checkpoint.  A no-op when the actor has no snapshot
    /// yet - without one there is nothing to recover anyway.
    fn persist_suspension_marker(&mut self, actor_id: u64) {
        let waiting_signal = match self.actors.get(&actor_id) {
            Some(actor) if actor.persistent => actor.waiting_signal.clone(),
            _ => return,
        };
        if let Some(mut snapshot) = self.persistence.load_snapshot(actor_id) {
            if snapshot.waiting_signal == waiting_signal {
                return;
            }
            snapshot.waiting_signal = waiting_signal;
            let _ = self.persistence.save_snapshot(snapshot);
        }
    }

    /// Lay out a workflow actor's native behavior table so that bytecode step
    /// ids (0..n-1) do not collide with internal runtime behaviors such as
    /// `__timer_fired`.
    pub(crate) fn layout_workflow_behavior_table(&mut self, actor_id: u64) {
        spawn::layout_workflow_behavior_table(self, actor_id)
    }

    /// Execute a bytecode behavior for an actor.
    fn run_bytecode_behavior(
        &mut self,
        actor_id: u64,
        behavior_idx: usize,
        args: &[Value],
    ) -> crate::types::NuResult<Value> {
        let code_offset = {
            let actor = match self.actors.get(&actor_id) {
                Some(a) => a,
                None => return Ok(Value::nil()),
            };
            actor
                .bytecode_offsets
                .get(behavior_idx)
                .copied()
                .unwrap_or(0)
        };
        let result = self.run_bytecode_at_offset(actor_id, code_offset, args);
        // If the step suspended waiting for a signal or a background LLM
        // call, record which behavior and step name it was executing so
        // recovery/resumption can continue.
        if let Err(crate::types::NuError::Suspended(_)) = result {
            let step_name = self.step_name_for(actor_id, behavior_idx);
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                if let Some(ref mut suspended) = actor.suspended_execution {
                    suspended.behavior_idx = behavior_idx;
                    suspended.step_name = step_name;
                }
            }
        }
        result
    }

    /// Execute a saga compensation expression for a completed workflow step.
    fn run_compensation(
        &mut self,
        actor_id: u64,
        behavior_idx: usize,
    ) -> crate::types::NuResult<Value> {
        let code_offset = {
            let actor = match self.actors.get(&actor_id) {
                Some(a) => a,
                None => return Ok(Value::nil()),
            };
            match actor
                .compensation_offsets
                .get(behavior_idx)
                .copied()
                .flatten()
            {
                Some(offset) => offset,
                None => return Ok(Value::nil()),
            }
        };
        self.run_bytecode_at_offset(actor_id, code_offset, &[])
    }

    /// Execute bytecode at a specific code offset for an actor.
    fn run_bytecode_at_offset(
        &mut self,
        actor_id: u64,
        code_offset: usize,
        args: &[Value],
    ) -> crate::types::NuResult<Value> {
        let module = match self.actors.get(&actor_id) {
            Some(a) => match a.bytecode_module.clone() {
                Some(m) => m,
                None => return Ok(Value::nil()),
            },
            None => return Ok(Value::nil()),
        };

        let self_ptr: *mut Runtime = self;
        unsafe {
            if (*self_ptr).vm.is_none() {
                (*self_ptr).vm = Some(crate::vm::VM::new());
            }
            let vm = (*self_ptr).vm.as_mut().unwrap();

            let module_idx = if let Some(idx) = (*self_ptr)
                .actors
                .get(&actor_id)
                .unwrap()
                .bytecode_module_idx
            {
                idx
            } else {
                let idx = vm.modules.len();
                vm.load_module(module.clone());
                (*self_ptr).register_module_grains(&module);
                if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                    actor.bytecode_module_idx = Some(idx);
                }
                idx
            };

            vm.set_actor_callbacks(Box::new(BytecodeRuntimeCallbacks::new(self_ptr, actor_id)));
            vm.set_distributed_callbacks(Box::new(BytecodeDistributedCallbacks {
                runtime: self_ptr,
            }));

            let mut frame = crate::vm::Frame::new(None, module_idx);
            frame.pc = code_offset;
            for (i, arg) in args.iter().enumerate().take(256) {
                frame.regs[i] = *arg;
            }
            vm.set_current_frame(frame);

            (*self_ptr).vm_exec_begin();

            // Reset JIT safepoint counter for this behavior invocation.
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                actor.jit_safepoint_counter = crate::jit::runtime::JIT_SAFEPOINT_BUDGET;
                crate::jit::runtime::set_jit_safepoint_ptr(&mut actor.jit_safepoint_counter);
            }

            let result = vm.run_from(module_idx, code_offset);

            // JIT safepoint yield: capture state for inline resume on next turn.
            if vm.yield_pending {
                if let Some(vm_state) = vm.take_suspended_state() {
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        actor.suspended_execution =
                            Some(crate::runtime::actor::SuspendedExecution {
                                vm_state,
                                behavior_idx: 0,
                                step_name: String::new(),
                            });
                        actor.jit_yield_pending = true;
                    }
                }
                crate::jit::runtime::clear_jit_safepoint_ptr();
                (*self_ptr).vm_exec_end();
                return Ok(Value::nil());
            }
            // Capture VM state for a workflow signal wait, a non-blocking
            // LLM call, or a timed selective receive. Doing this here avoids
            // aliasing the Runtime through the callback while the VM borrow
            // is active.
            if let Err(crate::types::NuError::Suspended(_)) = &result {
                if let Some(vm_state) = vm.take_suspended_state() {
                    let signal_name = vm.suspended_signal_name.take();
                    let receive_timeout = vm.suspended_receive_timeout.take();
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        let marker = suspension_marker(actor, signal_name);
                        actor.waiting_signal = marker;
                        actor.suspended_execution =
                            Some(crate::runtime::actor::SuspendedExecution {
                                vm_state,
                                behavior_idx: 0,
                                step_name: String::new(),
                            });
                    }
                    self.maybe_schedule_receive_wait(actor_id, receive_timeout);
                }
            }
            // End the VM-execution window only after the suspend-state
            // capture above: draining deferred wakes runs other actors on
            // the shared VM, which would clobber the frames an un-captured
            // suspend still needs. Runs on every path, so wakes of other
            // actors are not lost when THIS actor suspends.
            (*self_ptr).vm_exec_end();
            crate::jit::runtime::clear_jit_safepoint_ptr();
            // String-id values index into this runtime VM's constant pool. When
            // the result is returned to a different VM (e.g. the top-level VM
            // that invoked `ask`), the id is meaningless there. Convert string
            // results to heap-allocated pointers so they remain valid.
            match result {
                Ok(value) => {
                    if let Some(id) = value.as_string_id() {
                        if let Some(s) = vm.constant_string(module_idx, id) {
                            Ok(vm.allocate_string(&s))
                        } else {
                            Ok(value)
                        }
                    } else {
                        Ok(value)
                    }
                }
                Err(e) => Err(e),
            }
        }
    }

    /// Run saga compensations for a workflow step that failed.
    /// Walks backwards through completed steps and executes each compensation
    /// expression in reverse order, skipping steps already marked compensated.
    fn run_saga_compensation(&mut self, actor_id: u64, _failed_behavior_idx: usize) {
        let step_index = self
            .actors
            .get(&actor_id)
            .and_then(|a| a.get_state_field("step_index").and_then(|v| v.as_int()))
            .unwrap_or(0) as usize;

        for behavior_idx in (0..step_index).rev() {
            let already_compensated = {
                let actor = match self.actors.get(&actor_id) {
                    Some(a) => a,
                    None => return,
                };
                let step_name = self.step_name_for(actor_id, behavior_idx);
                actor.compensated_steps.contains(&step_name)
            };
            if already_compensated {
                continue;
            }

            let result = self.run_compensation(actor_id, behavior_idx);
            let step_name = self.step_name_for(actor_id, behavior_idx);
            if result.is_err() {
                // Compensation failed: do not record it as completed.
                continue;
            }
            let _ = self.append_saga_compensated(actor_id, &step_name);
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                if !actor.compensated_steps.contains(&step_name) {
                    actor.compensated_steps.push(step_name);
                }
            }
        }
    }

    /// Workflow step failures recorded since the runtime started, as
    /// `(step_name, error)` — surfaced by the CLI so a failing step is no
    /// longer silent (SPEC2 §10 known-issue #5). Reads the durable
    /// `StepFailed` events for every workflow actor.
    pub fn workflow_failures(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let actor_ids: Vec<u64> = self
            .actors
            .iter()
            .filter(|(_, a)| a.is_workflow)
            .map(|(id, _)| *id)
            .collect();
        for actor_id in actor_ids {
            for event in self.persistence.read_workflow_events(actor_id) {
                if let WorkflowEvent::StepFailed {
                    step_name, error, ..
                } = event
                {
                    out.push((step_name, error));
                }
            }
        }
        out
    }

    /// Return the step name for a workflow behavior index.
    fn step_name_for(&self, actor_id: u64, behavior_idx: usize) -> String {
        if let Some(actor) = self.actors.get(&actor_id) {
            // Prefer real behavior names; skip placeholder entries used to
            // reserve step ids in workflow actors.
            if let Some(entry) = actor.behavior_table.get(behavior_idx) {
                if !entry.name.is_empty() {
                    if let Some(pos) = entry.name.rfind('.') {
                        return entry.name[pos + 1..].to_string();
                    }
                    return entry.name.clone();
                }
            }
            if let Some(module) = &actor.bytecode_module {
                if let Some(entry) = module.behaviors.get(behavior_idx) {
                    if let Some(pos) = entry.name.rfind('.') {
                        return entry.name[pos + 1..].to_string();
                    }
                    return entry.name.clone();
                }
            }
        }
        format!("step_{}", behavior_idx)
    }

    /// Return true if the behavior index belongs to an internal runtime behavior
    /// (not a user-defined workflow step). Internal behaviors do not generate
    /// `StepCompleted` events.
    fn is_internal_behavior(&self, actor_id: u64, behavior_idx: usize) -> bool {
        self.actors
            .get(&actor_id)
            .and_then(|a| a.behavior_table.get(behavior_idx))
            .map(|entry| entry.name == "__timer_fired")
            .unwrap_or(false)
    }

    /// Return true if the workflow behavior at `behavior_idx` is a synthetic
    /// parallel step.  Parallel steps advance step_index in the runtime rather
    /// than in their bytecode.
    fn is_parallel_step(&self, actor_id: u64, behavior_idx: usize) -> bool {
        self.actors
            .get(&actor_id)
            .and_then(|a| a.bytecode_module.as_ref())
            .and_then(|m| m.behaviors.get(behavior_idx))
            .map(|entry| entry.parallel_branches.is_some())
            .unwrap_or(false)
    }

    /// Recover a persistent actor from the latest snapshot and replay the journal.
    ///
    /// For workflow actors the durable workflow event journal is replayed
    /// instead of the message journal, restoring the current step index and
    /// any other state captured in workflow events.
    pub fn recover_actor(&mut self, actor_id: u64) -> Option<u64> {
        let snapshot = self.persistence.load_snapshot(actor_id)?;
        let workflow_events = self.persistence.read_workflow_events(actor_id);
        let is_workflow = self
            .recovery_modules
            .get(&actor_id)
            .map(|(m, _, _)| m.actor_metadata.iter().any(|meta| meta.is_workflow))
            .unwrap_or(!workflow_events.is_empty());
        let is_agent = self
            .recovery_modules
            .get(&actor_id)
            .map(|(m, _, _)| m.actor_metadata.iter().any(|meta| meta.is_agent))
            .unwrap_or(false);

        let mut actor = Actor::new(actor_id, format!("actor_{}", actor_id), 0);
        actor.persistent = true;
        actor.is_workflow = is_workflow;
        actor.is_agent = is_agent;
        actor.sequence = snapshot.sequence;
        actor.waiting_signal = snapshot.waiting_signal;
        // Restore CRDT state if present in the snapshot.
        if let Some(crdt_snap) = &snapshot.crdt_snapshot {
            if let Some(manager) = &mut self.crdt_manager {
                let snapshot: HashMap<CrdtId, (CrdtType, Vec<u8>)> = crdt_snap
                    .iter()
                    .filter_map(|(id, ty, bytes)| {
                        CrdtType::from_u8(*ty).map(|t| (CrdtId(*id), (t, bytes.clone())))
                    })
                    .collect();
                manager.restore(snapshot);
            }
        }
        for (name, value) in snapshot.state {
            // Rehydrate the semantic_memory and procedural_memory JSON strings
            // by allocating them on the actor heap so runtime helpers can read
            // them as pointer values.
            if name == "semantic_memory" || name == "procedural_memory" {
                if let PersistedValue::String(json) = &value {
                    let ptr = actor.allocate_string(json);
                    actor.set_state_field(name, ptr);
                    continue;
                }
            }
            let v = value.to_value_on_heap(&mut actor);
            actor.set_state_field(name, v);
        }
        // Parse cached retry/fallback configs from restored state for agents.
        if is_agent {
            if let Some(module) = actor
                .bytecode_module
                .as_ref()
                .or_else(|| self.recovery_modules.get(&actor_id).map(|(m, _, _)| m))
            {
                for (name, c) in module.actor_metadata.iter().flat_map(|m| &m.state_defaults) {
                    if let crate::bytecode::Constant::String(json) = c {
                        if name == "retry_config" {
                            actor.retry_config = serde_json::from_str(&json).ok();
                        } else if name == "fallback_config" {
                            actor.fallback_config = serde_json::from_str(&json).unwrap_or_default();
                        }
                    }
                }
            }
        }
        // Replay event-sourced events to reconstruct EventSourced fields.
        // The stored snapshot value (captured after the apply handler ran
        // during live execution) correctly reconstructs fields with
        // non-trivial apply handlers.  See
        // `integration_tests::test_event_sourced_apply_handler_recovery`
        // which validates this behavior.
        let events = self.persistence.read_events(actor_id);
        if !events.is_empty() {
            for entry in &events {
                let v = entry.value.to_value_on_heap(&mut actor);
                actor.set_state_field(&entry.field_name, v);
                let current_seq = actor
                    .event_sourced_sequences
                    .get(&entry.field_name)
                    .copied()
                    .unwrap_or(0);
                if entry.sequence > current_seq {
                    actor
                        .event_sourced_sequences
                        .insert(entry.field_name.clone(), entry.sequence);
                }
            }
        }
        // Fill in declared initial values for fields not touched by any
        // restoration path above (snapshot state, event-sourced
        // replay). This is specifically for `local` fields:
        // checkpoint_actor never includes them in the snapshot (by
        // design -- they're "Ephemeral, reset on restart" per
        // SPEC2.md §9.3), and nothing else re-runs the spawn-time
        // default-value initialization recover_actor's bare
        // `Actor::new` skipped. Without this, a `local` field comes
        // back as nil/unset instead of its declared initial value.
        // Placed after event-sourced replay above (not before) so it
        // only fills genuine gaps -- an EventSourced field with zero
        // persisted events legitimately has no value yet either, but
        // that's a separate, pre-existing question this fix doesn't
        // change.
        if let Some(module) = self.recovery_modules.get(&actor_id).map(|(m, _, _)| m) {
            for (name, c) in module.actor_metadata.iter().flat_map(|m| &m.state_defaults) {
                if actor.get_state_field(name).is_some() {
                    continue;
                }
                let v = match c {
                    crate::bytecode::Constant::String(s) => actor.allocate_string(s),
                    other => crate::vm::constant_to_value(other),
                };
                actor.set_state_field(name, v);
            }
        }
        // Restore bytecode metadata registered for recovery.
        if let Some((module, offsets, comp_offsets)) = self.recovery_modules.get(&actor_id) {
            actor.bytecode_module = Some(module.clone());
            actor.bytecode_offsets = offsets.clone();
            actor.compensation_offsets = comp_offsets.clone();
            // Restore per-field state-model tracking (Local/Durable/
            // EventSourced/Crdt), lost when `Actor::new` built a bare
            // actor above. Without this, `checkpoint_actor`'s
            // Durable/Crdt snapshot filter and `emit_event`'s
            // EventSourced "+1" bump both silently fall back to
            // treating every field as `Local` (via their
            // `unwrap_or(StateModel::Local)`), breaking persistence for
            // any field mutated after this recovery: a second crash
            // would drop Durable fields from the snapshot entirely, and
            // EventSourced fields would stop accumulating via emitted
            // events.
            actor.state_models = module
                .actor_metadata
                .iter()
                .flat_map(|m| &m.state_models)
                .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
                .collect();
        }
        if is_workflow {
            self.actors.insert(actor_id, actor);
            self.layout_workflow_behavior_table(actor_id);
        } else {
            self.actors.insert(actor_id, actor);
        }

        if is_workflow {
            // Replay workflow events that arrived after the snapshot.
            let events_to_replay: Vec<_> = workflow_events
                .iter()
                .filter(|e| e.sequence() > snapshot.sequence)
                .cloned()
                .collect();
            let mut fired_timer_names: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for event in &events_to_replay {
                if let WorkflowEvent::TimerFired { name, .. } = event {
                    fired_timer_names.insert(name.clone());
                }
            }
            for event in &events_to_replay {
                if let Some(actor) = self.actors.get_mut(&actor_id) {
                    Self::apply_workflow_event(actor, event);
                    actor.sequence = event.sequence();
                }
            }
            // Re-arm timers that were set before the snapshot/replay but have
            // not yet fired. Timers are reconstructed from the full durable
            // journal, not just events after the snapshot, because snapshots do
            // not capture pending timers.
            let all_timer_events = self.persistence.read_timer_events(actor_id);
            let mut fired_timer_names: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for event in &all_timer_events {
                if let WorkflowEvent::TimerFired { name, .. } = event {
                    fired_timer_names.insert(name.clone());
                }
            }
            for event in &all_timer_events {
                if let WorkflowEvent::TimerSet {
                    name, duration_ms, ..
                } = event
                {
                    if !fired_timer_names.contains(name) {
                        self.rearm_timer(actor_id, name, *duration_ms);
                    }
                }
            }
            // If the workflow was in the middle of a step waiting on a signal,
            // re-trigger that step so it can resume from replayed events. We
            // use step_index as the behavior id because each step is compiled
            // to a behavior at the same index.
            let should_resume = self
                .actors
                .get(&actor_id)
                .map(|a| a.waiting_signal.is_some() || a.suspended_execution.is_some())
                .unwrap_or(false);
            if should_resume {
                let current_step = self
                    .actors
                    .get(&actor_id)
                    .and_then(|a| a.get_state_field("step_index"))
                    .and_then(|v| v.as_int())
                    .unwrap_or(0) as u16;
                let has_behavior = self
                    .actors
                    .get(&actor_id)
                    .and_then(|a| a.bytecode_module.as_ref())
                    .map(|m| (current_step as usize) < m.behaviors.len())
                    .unwrap_or(false);
                if has_behavior {
                    self.send_message_by_id(actor_id, current_step, &[]);
                }
            }
        } else {
            // Replay journal entries that arrived after the snapshot.
            let journal = self.persistence.read_journal(actor_id);
            let entries_to_replay: Vec<_> = journal
                .iter()
                .filter(|e| e.sequence > snapshot.sequence)
                .cloned()
                .collect();
            for entry in entries_to_replay {
                let behavior_idx = entry.behavior_id as usize;
                let payload: Vec<Value> = entry.payload.iter().map(|p| p.to_value()).collect();
                if self.has_native_handler(actor_id, behavior_idx) {
                    let handler = self
                        .actors
                        .get(&actor_id)
                        .and_then(|a| a.behavior_table.get(behavior_idx))
                        .map(|b| b.handler_fn)?;
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        handler(actor, &payload);
                        actor.sequence = entry.sequence;
                    }
                } else if self.has_bytecode_handler(actor_id, behavior_idx) {
                    self.current_actor = Some(actor_id);
                    let _ = self.run_bytecode_behavior(actor_id, behavior_idx, &payload);
                    self.current_actor = None;
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        actor.sequence = entry.sequence;
                    }
                }
            }
        }
        self.enqueue_actor(actor_id);
        Some(actor_id)
    }

    /// Build an actor from a snapshot and bytecode module - the common core
    /// shared by [`recover_actor`](Runtime::recover_actor) and
    /// [`receive_migrated_actor`](Runtime::receive_migrated_actor).
    ///
    /// Restores persistent flags, state models, durable fields, and default
    /// values.  Does NOT register the recovery module, restore CRDT state,
    /// insert into `self.actors`, or enqueue - callers do those.
    fn restore_actor_from_snapshot(
        actor_id: u64,
        module: &crate::bytecode::CodeModule,
        snapshot: &ActorSnapshot,
        is_workflow: bool,
        is_agent: bool,
    ) -> Actor {
        let offsets: Vec<usize> = crate::runtime::spawn::bytecode_offsets_for(module, is_workflow);
        let compensation_offsets: Vec<Option<usize>> = if is_workflow {
            module
                .actor_metadata
                .iter()
                .find(|m| m.is_workflow)
                .map(|meta| {
                    meta.behavior_indices
                        .iter()
                        .map(|&i| module.behaviors[i].compensate_offset.map(|o| o as usize))
                        .collect()
                })
                .unwrap_or_else(|| {
                    module
                        .behaviors
                        .iter()
                        .map(|b| b.compensate_offset.map(|o| o as usize))
                        .collect()
                })
        } else {
            module
                .behaviors
                .iter()
                .map(|b| b.compensate_offset.map(|o| o as usize))
                .collect()
        };

        let mut actor = Actor::new(actor_id, format!("actor_{}", actor_id), 0);
        actor.persistent = true;
        actor.is_workflow = is_workflow;
        actor.is_agent = is_agent;
        actor.sequence = snapshot.sequence;
        actor.waiting_signal = snapshot.waiting_signal.clone();
        actor.bytecode_module = Some(module.clone());
        actor.bytecode_offsets = offsets;
        actor.compensation_offsets = compensation_offsets;

        // Restore per-field state-model tracking.
        actor.state_models = module
            .actor_metadata
            .iter()
            .flat_map(|m| &m.state_models)
            .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
            .collect();

        // Restore durable state fields from the snapshot.
        for (name, value) in &snapshot.state {
            if name == "semantic_memory" || name == "procedural_memory" {
                if let PersistedValue::String(json) = value {
                    let ptr = actor.allocate_string(json);
                    actor.set_state_field(name, ptr);
                    continue;
                }
            }
            let v = value.to_value_on_heap(&mut actor);
            actor.set_state_field(name, v);
        }

        // Fill in declared initial values for fields not touched above.
        for (name, c) in module.actor_metadata.iter().flat_map(|m| &m.state_defaults) {
            if actor.get_state_field(name).is_some() {
                continue;
            }
            let v = match c {
                crate::bytecode::Constant::String(s) => actor.allocate_string(s),
                other => crate::vm::constant_to_value(other),
            };
            actor.set_state_field(name, v);
        }

        actor
    }

    /// Resolve a virtual actor (grain) identity to a resident actor id,
    /// hydrating it from persistence if necessary.
    ///
    /// Returns the actor id on success.  If the grain type is unknown or the
    /// snapshot exists but cannot be restored, returns an error.
    pub fn resolve_or_hydrate_grain(&mut self, grain_id: GrainId) -> Result<u64, NuError> {
        if let Some(&id) = self.grain_residents.get(&grain_id) {
            return Ok(id);
        }

        let grain_type = self
            .grain_registry
            .get(&grain_id.grain_type)
            .ok_or_else(|| NuError::RuntimeError {
                msg: format!("unknown virtual actor type: {}", grain_id.grain_type),
                span: Span::new(0, 0),
            })?
            .clone();

        let stable_actor_id = grain_actor_id(&grain_id);

        // Register the recovery module so checkpoint/replay know the bytecode.
        self.register_recovery_module(
            stable_actor_id,
            grain_type.module.clone(),
            grain_type.bytecode_offsets.clone(),
            grain_type.compensation_offsets.clone(),
        );

        let snapshot = self.persistence.load_snapshot(stable_actor_id);

        let actor = if let Some(ref snap) = snapshot {
            Self::restore_actor_from_snapshot(
                stable_actor_id,
                &grain_type.module,
                snap,
                false,
                false,
            )
        } else {
            let mut actor = Actor::new(stable_actor_id, grain_id.actor_name(), 0);
            actor.persistent = true;
            actor.bytecode_module = Some(grain_type.module.clone());
            actor.bytecode_offsets = grain_type.bytecode_offsets.clone();
            actor.compensation_offsets = grain_type.compensation_offsets.clone();
            actor.state_models = grain_type
                .default_models
                .iter()
                .map(|(name, model)| (name.clone(), *model))
                .collect();
            // Fill declared initial values.
            for (name, c) in grain_type
                .module
                .actor_metadata
                .iter()
                .flat_map(|m| &m.state_defaults)
            {
                let v = match c {
                    crate::bytecode::Constant::String(s) => actor.allocate_string(&s),
                    other => crate::vm::constant_to_value(&other),
                };
                actor.set_state_field(name, v);
            }
            actor
        };

        // Track the grain identity.
        self.actors.insert(stable_actor_id, actor);
        self.grain_residents
            .insert(grain_id.clone(), stable_actor_id);
        self.actor_grain_id
            .insert(stable_actor_id, grain_id.clone());
        self.grain_actor_ids.insert(stable_actor_id, grain_id);

        // Replay message journal entries that arrived after the snapshot.
        if let Some(ref snap) = snapshot {
            let journal = self.persistence.read_journal(stable_actor_id);
            for entry in journal.iter().filter(|e| e.sequence > snap.sequence) {
                let behavior_idx = entry.behavior_id as usize;
                let payload: Vec<Value> = entry.payload.iter().map(|p| p.to_value()).collect();
                if self.has_native_handler(stable_actor_id, behavior_idx) {
                    // Native handlers cannot be resolved until the actor is in
                    // `self.actors`, so we only support bytecode grains here.
                    continue;
                }
                if self.has_bytecode_handler(stable_actor_id, behavior_idx) {
                    self.current_actor = Some(stable_actor_id);
                    let _ = self.run_bytecode_behavior(stable_actor_id, behavior_idx, &payload);
                    self.current_actor = None;
                    if let Some(actor) = self.actors.get_mut(&stable_actor_id) {
                        actor.sequence = entry.sequence;
                    }
                }
            }
        }

        self.enqueue_actor(stable_actor_id);

        Ok(stable_actor_id)
    }

    /// Receive a migrated actor from another node.
    ///
    /// Deserializes the NBC bytecode module and durable state snapshot sent
    /// by the source node, creates a local actor with the same id, restores
    /// its state and behavior table, and enqueues it for scheduling.
    ///
    /// Returns `true` on success, `false` if the NBC module or snapshot is
    /// malformed.
    pub fn receive_migrated_actor(
        &mut self,
        actor_id: u64,
        nbc_bytes: Vec<u8>,
        snapshot_json: Vec<u8>,
    ) -> bool {
        use crate::bytecode::CodeModule;
        use crate::runtime::persistence::ActorSnapshot;

        // Parse the bytecode module.
        let module = match CodeModule::from_nbc(&nbc_bytes) {
            Ok(artifact) => artifact.module,
            Err(e) => {
                tracing::warn!(
                    "nulang-migrate: bad NBC module for actor {}: {}",
                    actor_id,
                    e
                );
                return false;
            }
        };

        // Parse the durable state snapshot.
        let snapshot: ActorSnapshot = match serde_json::from_slice(&snapshot_json) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "nulang-migrate: bad snapshot JSON for actor {}: {}",
                    actor_id,
                    e
                );
                return false;
            }
        };

        let is_workflow = module.actor_metadata.iter().any(|m| m.is_workflow);
        let is_agent = module.actor_metadata.iter().any(|m| m.is_agent);

        let actor =
            Self::restore_actor_from_snapshot(actor_id, &module, &snapshot, is_workflow, is_agent);

        // Register the recovery module.
        let offsets: Vec<usize> = module
            .behaviors
            .iter()
            .map(|b| b.code_offset as usize)
            .collect();
        // Filter compensation_offsets to this actor's own behaviors using
        // behavior_indices from the first workflow ActorMeta (a migrated
        // actor module carries its own metadata).
        let compensation_offsets: Vec<Option<usize>> = module
            .actor_metadata
            .iter()
            .find(|m| m.is_workflow)
            .map(|meta| {
                meta.behavior_indices
                    .iter()
                    .map(|&i| module.behaviors[i].compensate_offset.map(|o| o as usize))
                    .collect()
            })
            .unwrap_or_else(|| {
                module
                    .behaviors
                    .iter()
                    .map(|b| b.compensate_offset.map(|o| o as usize))
                    .collect()
            });
        self.recovery_modules
            .insert(actor_id, (module, offsets, compensation_offsets));

        // Restore CRDT state if present.
        if let Some(crdt_snap) = &snapshot.crdt_snapshot {
            if let Some(manager) = &mut self.crdt_manager {
                let crdt_map: HashMap<CrdtId, (CrdtType, Vec<u8>)> = crdt_snap
                    .iter()
                    .filter_map(|(id, ty, bytes)| {
                        CrdtType::from_u8(*ty).map(|t| (CrdtId(*id), (t, bytes.clone())))
                    })
                    .collect();
                manager.restore(crdt_map);
            }
        }

        if is_workflow {
            self.layout_workflow_behavior_table(actor_id);
        }
        self.actors.insert(actor_id, actor);
        self.enqueue_actor(actor_id);

        tracing::info!("nulang-migrate: actor {} received and enqueued", actor_id);
        true
    }

    /// Apply a single workflow event to an actor's state.  Used during recovery
    /// replay to restore step index and accumulated event-sourced state.
    fn apply_workflow_event(actor: &mut Actor, event: &WorkflowEvent) {
        match event {
            WorkflowEvent::WorkflowStarted { .. } => {
                if actor.get_state_field("step_index").is_some() {
                    actor.set_state_field("step_index", Value::int(0));
                }
            }
            WorkflowEvent::StepCompleted { .. } => {
                if let Some(n) = actor.get_state_field("step_index").and_then(|v| v.as_int()) {
                    actor.set_state_field("step_index", Value::int(n + 1));
                }
                // A completed step (sequential or parallel) clears any stale
                // parallel-progress counter.
                actor.set_state_field("parallel_progress", Value::int(0));
            }
            WorkflowEvent::SagaCompensated { step_name, .. } => {
                // Replay marks the step as already compensated so the runtime
                // does not run its compensation expression again.
                if !actor.compensated_steps.contains(step_name) {
                    actor.compensated_steps.push(step_name.clone());
                }
            }
            // Foundation: timer events are persisted but their runtime
            // scheduling is handled by the timer feature scope.
            WorkflowEvent::TimerSet { .. } | WorkflowEvent::TimerFired { .. } => {}
            // A failed step is a terminal marker; replay does not re-run
            // it (the failure was already compensated live).
            WorkflowEvent::StepFailed { .. } => {}
            WorkflowEvent::SignalReceived { name, payload, .. } => {
                actor.received_signals.push((name.clone(), payload.clone()));
            }
            WorkflowEvent::ParallelBranchCompleted { .. } => {
                let current = actor
                    .get_state_field("parallel_progress")
                    .and_then(|v| v.as_int())
                    .unwrap_or(0);
                actor.set_state_field("parallel_progress", Value::int(current + 1));
            }
            WorkflowEvent::Custom { name, args, .. } => {
                let values: Vec<Value> = args.iter().map(|a| a.to_value()).collect();
                actor.event_log.push((name.clone(), values));
            }
        }
    }

    fn has_native_handler(&self, actor_id: u64, behavior_idx: usize) -> bool {
        self.actors
            .get(&actor_id)
            .and_then(|a| a.behavior_table.get(behavior_idx))
            .map(|e| !e.name.is_empty())
            .unwrap_or(false)
    }

    // -- Fault Tolerance: Links --

    pub fn link_actors(&mut self, a: u64, b: u64) {
        if a == b {
            return;
        }
        if let Some(actor_a) = self.actors.get_mut(&a) {
            if !actor_a.links.contains(&b) {
                actor_a.links.push(b);
            }
        }
        if let Some(actor_b) = self.actors.get_mut(&b) {
            if !actor_b.links.contains(&a) {
                actor_b.links.push(a);
            }
        }
    }

    pub fn unlink_actors(&mut self, a: u64, b: u64) {
        if let Some(actor_a) = self.actors.get_mut(&a) {
            actor_a.links.retain(|&id| id != b);
        }
        if let Some(actor_b) = self.actors.get_mut(&b) {
            actor_b.links.retain(|&id| id != a);
        }
    }

    // -- Fault Tolerance: Monitors --

    pub fn monitor(&mut self, watcher: u64, target: u64) {
        if watcher == target {
            return;
        }
        if let Some(actor) = self.actors.get_mut(&target) {
            if !actor.monitors.contains(&watcher) {
                actor.monitors.push(watcher);
            }
        } else {
            self.send_down_message(watcher, target, &ExitReason::Error("noproc".to_string()));
        }
    }

    pub fn demonitor(&mut self, watcher: u64, target: u64) {
        if let Some(actor) = self.actors.get_mut(&target) {
            actor.monitors.retain(|&id| id != watcher);
        }
    }

    // -- Fault Tolerance: Actor Exit --

    pub fn exit_actor(&mut self, actor_id: u64, reason: ExitReason) {
        exit::exit_actor(self, actor_id, reason)
    }

    pub fn kill_actor(&mut self, actor_id: u64) {
        exit::kill_actor(self, actor_id)
    }

    pub fn handle_actor_exit(&mut self, actor_id: u64, reason: ExitReason) {
        exit::handle_actor_exit(self, actor_id, reason)
    }

    /// Exit-protocol cleanup for an actor being removed: mark it terminated,
    /// release its receiver-side ORCA holds, unregister its names, leave its
    /// process groups, send DOWN to its monitors, propagate abnormal exits
    /// to linked actors, then reap it (retiring the heap while foreign
    /// references are outstanding).
    ///
    /// Shared by `handle_actor_exit` and by supervisor mass-removal paths
    /// (`restart_all`/`restart_from`/`shutdown_supervisor`), which remove
    /// LIVING children and therefore must not bypass the protocol.  Does not
    /// dispatch to the actor's supervisor - supervision is handled by the
    /// callers, which is why this is not simply `handle_actor_exit`.
    fn reap_living_actor(&mut self, actor_id: u64, reason: ExitReason) {
        exit::reap_living_actor(self, actor_id, reason)
    }

    // -- Builtin Actor Effects (Actor.*) --

    /// Dispatch a built-in `Actor.*` effect performed by `actor_id` (the
    /// current actor, when any). Every op yields nil except `whereis`,
    /// which yields the actor ref or nil for unknown names. Ops that need
    /// a current actor are no-ops outside one, matching the standalone
    /// VM's nil fallback. Returns `None` for unknown op names so the
    /// caller can fall through to other built-in handlers.
    fn perform_actor_builtin(
        &mut self,
        actor_id: Option<u64>,
        op_name: Option<&str>,
        constants: &[crate::bytecode::Constant],
        regs: &[Value],
    ) -> Option<Value> {
        let string_arg = |idx: usize| -> Option<String> {
            let id = regs.get(idx)?.as_string_id()?;
            match constants.get(id as usize) {
                Some(crate::bytecode::Constant::String(s)) => Some(s.clone()),
                _ => None,
            }
        };
        match op_name {
            Some("link") | Some("unlink") | Some("monitor") | Some("demonitor") => {
                let target = regs.get(0)?.as_actor_id()?;
                let Some(me) = actor_id else {
                    return Some(Value::nil());
                };
                match op_name {
                    Some("link") => self.link_actors(me, target),
                    Some("unlink") => self.unlink_actors(me, target),
                    Some("monitor") => self.monitor(me, target),
                    _ => self.demonitor(me, target),
                }
                Some(Value::nil())
            }
            Some("trap_exit") => {
                let flag = regs.get(0)?.as_bool()?;
                if let Some(me) = actor_id {
                    if let Some(actor) = self.actors.get_mut(&me) {
                        actor.trap_exits = flag;
                    }
                }
                Some(Value::nil())
            }
            Some("set_priority") => {
                // 0 = High, 1 = Normal, 2 = Low; any other value selects
                // Normal. Takes effect on the actor's next (re)queue.
                let level = regs.get(0)?.as_int()?;
                if let Some(me) = actor_id {
                    if let Some(actor) = self.actors.get_mut(&me) {
                        actor.priority = match level {
                            0 => ActorPriority::High,
                            2 => ActorPriority::Low,
                            _ => ActorPriority::Normal,
                        };
                    }
                }
                Some(Value::nil())
            }
            Some("exit") => {
                let reason = actor_exit_reason(regs.get(0), constants);
                if let Some(me) = actor_id {
                    self.exit_actor(me, reason);
                }
                Some(Value::nil())
            }
            Some("register") => {
                let name = string_arg(0)?;
                if let Some(me) = actor_id {
                    let _ = self.registry.register(&name, me);
                }
                Some(Value::nil())
            }
            Some("unregister") => {
                let name = string_arg(0)?;
                let _ = self.registry.unregister(&name);
                Some(Value::nil())
            }
            Some("whereis") => {
                let name = string_arg(0)?;
                Some(match self.registry.whereis(&name) {
                    Some(id) => Value::actor_ref(id),
                    None => Value::nil(),
                })
            }
            _ => None,
        }
    }

    /// Dispatch a built-in `Grain.*` effect performed from bytecode.
    ///
    /// - `ref`: returns the stable actor id for `(grain_type, key)`.
    /// - `prewarm`: hydrates the grain if not resident; returns unit on
    ///   success, nil on failure.
    /// - `pin` / `unpin`: hydrates the grain and sets/clears the pinned flag.
    ///
    /// Returns `None` for unknown op names so the caller can fall through
    /// to other built-in handlers.
    fn perform_grain_builtin(
        &mut self,
        op_name: Option<&str>,
        constants: &[crate::bytecode::Constant],
        regs: &[Value],
    ) -> Option<Value> {
        let string_arg = |idx: usize| -> Option<String> {
            let id = regs.get(idx)?.as_string_id()?;
            match constants.get(id as usize) {
                Some(crate::bytecode::Constant::String(s)) => Some(s.clone()),
                _ => None,
            }
        };
        let grain_type = string_arg(0)?;
        let key_value = regs.get(1)?;
        let key = if let Some(n) = key_value.as_int() {
            n.to_string()
        } else if let Some(s) = string_arg(1) {
            s
        } else {
            return Some(Value::nil());
        };
        let grain_id = GrainId::new(grain_type, key);
        match op_name {
            Some("ref") => {
                let stable_id = grain_actor_id(&grain_id);
                // Register the stable id -> GrainId mapping so sends to this
                // reference can hydrate the grain on first use, even though the
                // actor itself is not materialised yet.
                self.grain_actor_ids.insert(stable_id, grain_id);
                Some(Value::actor_ref(stable_id))
            }
            Some("prewarm") => match self.resolve_or_hydrate_grain(grain_id.clone()) {
                Ok(_) => Some(Value::unit()),
                Err(e) => {
                    warn!(
                        "nulang-grain: prewarm failed for {}: {}",
                        grain_id.actor_name(),
                        e
                    );
                    Some(Value::nil())
                }
            },
            Some("pin") => match self.resolve_or_hydrate_grain(grain_id.clone()) {
                Ok(actor_id) => {
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        actor.pin();
                    }
                    Some(Value::unit())
                }
                Err(e) => {
                    warn!(
                        "nulang-grain: pin failed for {}: {}",
                        grain_id.actor_name(),
                        e
                    );
                    Some(Value::nil())
                }
            },
            Some("unpin") => match self.resolve_or_hydrate_grain(grain_id.clone()) {
                Ok(actor_id) => {
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        actor.unpin();
                    }
                    Some(Value::unit())
                }
                Err(e) => {
                    warn!(
                        "nulang-grain: unpin failed for {}: {}",
                        grain_id.actor_name(),
                        e
                    );
                    Some(Value::nil())
                }
            },
            _ => None,
        }
    }

    /// Dispatch a built-in `Python.*` effect performed from bytecode.
    /// Returns `None` for unknown op names so the caller can fall through
    /// to other built-in handlers.
    #[cfg(feature = "python")]
    fn perform_python_builtin(
        &mut self,
        op_name: Option<&str>,
        constants: &[crate::bytecode::Constant],
        regs: &[Value],
    ) -> Option<Value> {
        let string_arg = |idx: usize| -> Option<String> {
            let id = regs.get(idx)?.as_string_id()?;
            match constants.get(id as usize) {
                Some(crate::bytecode::Constant::String(s)) => Some(s.clone()),
                _ => None,
            }
        };
        let fi = self.foreign_interop.get_or_insert_with(|| {
            match crate::backends::DefaultForeignInterop::new() {
                Ok(f) => Box::new(f) as Box<dyn crate::backends::ForeignInterop>,
                Err(e) => {
                    eprintln!("Python bridge init error: {}", e);
                    Box::new(NoOpForeignInterop) as Box<dyn crate::backends::ForeignInterop>
                }
            }
        });
        match op_name {
            Some("import") => {
                let module = string_arg(0)?;
                match fi.import(&module) {
                    Ok(()) => Some(Value::unit()),
                    Err(e) => {
                        eprintln!("Python import error: {}", e);
                        Some(Value::nil())
                    }
                }
            }
            Some("call") => {
                let module = string_arg(0)?;
                let function = string_arg(1)?;
                let args: Vec<Value> = regs.iter().skip(2).copied().collect();
                match fi.call(&module, &function, &args) {
                    Ok(result) => Some(result),
                    Err(e) => {
                        eprintln!("Python call error: {}", e);
                        Some(Value::nil())
                    }
                }
            }
            Some("get_attr") => {
                let module = string_arg(0)?;
                let attr = string_arg(1)?;
                let args: Vec<Value> = vec![];
                match fi.call(&module, &attr, &args) {
                    Ok(result) => Some(result),
                    Err(e) => {
                        eprintln!("Python get_attr error: {}", e);
                        Some(Value::nil())
                    }
                }
            }
            _ => None,
        }
    }

    // -- Builtin OTP Supervisor Effects (Otp.*) --

    /// Dispatch a built-in `Otp.*` supervisor effect performed from
    /// bytecode. Unlike `Actor.*`, these ops manage supervisors directly
    /// and do not need a current actor; unknown supervisor ids are nil
    /// no-ops (matching the `Actor.*` outside-an-actor contract). Returns
    /// `None` for unknown op names so the caller can fall through to other
    /// built-in handlers. `module` is the performing module: string args
    /// resolve against its constant pool and actor-type templates against
    /// its actor metadata (`find_actor_template`).
    fn perform_otp_builtin(
        &mut self,
        op_name: Option<&str>,
        module: &crate::bytecode::CodeModule,
        regs: &[Value],
    ) -> Option<Value> {
        let string_arg = |idx: usize| -> Option<String> {
            let id = regs.get(idx)?.as_string_id()?;
            match module.constants.get(id as usize) {
                Some(crate::bytecode::Constant::String(s)) => Some(s.clone()),
                _ => None,
            }
        };
        match op_name {
            // Strategy: 0=one_for_one, 1=one_for_all, 2=rest_for_one,
            // 3=simple_one_for_one; any other value is a nil no-op.
            Some("create_supervisor") => {
                let name = string_arg(0)?;
                let strategy = match regs.get(1)?.as_int()? {
                    0 => RestartStrategy::OneForOne,
                    1 => RestartStrategy::OneForAll,
                    2 => RestartStrategy::RestForOne,
                    3 => RestartStrategy::SimpleOneForOne,
                    _ => return Some(Value::nil()),
                };
                let id = self.create_supervisor(&name, strategy);
                Some(Value::int(id as i64))
            }
            // Policy: 0=permanent, 1=temporary, 2=transient,
            // 3=respawn_on_node_loss (RFC 0014); any other value is a nil
            // no-op.
            Some("supervise_child") => {
                let sup = regs.get(0)?.as_int()? as u64;
                let child = regs.get(1)?.as_actor_id()?;
                let policy = match regs.get(2)?.as_int()? {
                    0 => RestartPolicy::Permanent,
                    1 => RestartPolicy::Temporary,
                    2 => RestartPolicy::Transient,
                    3 => RestartPolicy::RespawnOnNodeLoss,
                    _ => return Some(Value::nil()),
                };
                if self.supervisors.contains_key(&sup) {
                    let spec = ChildSpec::new(format!("child_{}", child), policy);
                    self.supervise_child(sup, spec, child);
                }
                Some(Value::nil())
            }
            Some("set_template") => {
                let sup = regs.get(0)?.as_int()? as u64;
                let type_name = string_arg(1)?;
                let _ = self.set_supervisor_template(sup, &type_name, module);
                Some(Value::nil())
            }
            Some("start_child") => {
                let sup = regs.get(0)?.as_int()? as u64;
                Some(match self.start_supervised_child(sup, Vec::new()) {
                    Some(id) => Value::actor_ref(id),
                    None => Value::nil(),
                })
            }
            Some("terminate_child") => {
                let sup = regs.get(0)?.as_int()? as u64;
                let child = regs.get(1)?.as_actor_id()?;
                let _ = self.terminate_supervised_child(sup, child);
                Some(Value::nil())
            }
            Some("child_count") => {
                let sup = regs.get(0)?.as_int()? as u64;
                Some(match self.supervisors.get(&sup) {
                    Some(supervisor) => Value::int(supervisor.child_count() as i64),
                    None => Value::nil(),
                })
            }
            _ => None,
        }
    }

    /// Resolve an actor type by name to the `(module, behavior_idx)` pair

    /// Dispatch a built-in `Crdt.*` effect performed from bytecode.
    ///
    /// Each op targets a CRDT-backed field on the *current* actor and is
    /// validated against that field's per-type operation set by
    /// [`CrdtManager::apply_field_op`] — an op outside the set (e.g.
    /// `decrement` on a `gcounter`) returns `nil` and mutates nothing.
    /// Successful mutations materialize the field's new value back into
    /// `state_data` so `self.field` reads stay consistent with the
    /// replicated CRDT entry.
    ///
    /// Returns `None` only for an unrecognized op name, so the caller can
    /// fall through to other built-in handlers; every recognized op returns
    /// `Some(...)` (`nil` encodes a per-type rejection or missing field).
    fn perform_crdt_builtin(
        &mut self,
        actor_id: Option<u64>,
        op_name: Option<&str>,
        constants: &[crate::bytecode::Constant],
        regs: &[Value],
    ) -> Option<Value> {
        let string_arg = |idx: usize| -> Option<String> {
            let id = regs.get(idx)?.as_string_id()?;
            match constants.get(id as usize) {
                Some(crate::bytecode::Constant::String(s)) => Some(s.clone()),
                _ => None,
            }
        };

        // Unrecognized op: fall through to other built-in/user handlers
        // (returning `None`). Everything below is a recognized `Crdt.*` op.
        let op = op_name?;
        if !matches!(
            op,
            "increment" | "decrement" | "add" | "remove" | "set" | "read"
        ) {
            return None;
        }

        // A recognized op on a missing actor/field/manager, or an op outside
        // the field's type's operation set, is a *silent nil no-op* — never an
        // `Unhandled effect` abort, which would kill the enclosing behavior.
        let Some(actor_id) = actor_id else {
            return Some(Value::nil());
        };
        let Some(field) = string_arg(0) else {
            return Some(Value::nil());
        };
        let arg = string_arg(1);

        // Apply the op against the CrdtManager entry; the borrow is scoped
        // so the actor can be mutated afterwards. `None` from
        // `apply_field_op` means the field is unknown or the op is out of
        // the type's set — a nil no-op, not an abort.
        let outcome = {
            let Some(manager) = self.crdt_manager.as_mut() else {
                return Some(Value::nil());
            };
            match manager.apply_field_op(actor_id, &field, op, arg.as_deref()) {
                Some(v) => v,
                None => return Some(Value::nil()),
            }
        };

        // Materialize the value: intern register strings into the actor heap.
        let value = match outcome {
            crate::runtime::crdt_manager::CrdtValue::Int(i) => Value::int(i),
            crate::runtime::crdt_manager::CrdtValue::Str(s) => {
                match self.actors.get_mut(&actor_id) {
                    Some(actor) => actor.allocate_string(&s),
                    None => return Some(Value::nil()),
                }
            }
        };

        // Push the materialized value back so `self.field` reads are
        // consistent with the replicated entry.
        if let Some(actor) = self.actors.get_mut(&actor_id) {
            actor.set_state_field(field, value);
        }

        Some(if op == "read" { value } else { Value::unit() })
    }

    /// Resolve an actor type by name to the `(module, behavior_idx)` pair
    /// `spawn_from_module` expects. Searches the performing module first,
    /// then the runtime VM's loaded modules, then the recovery modules
    /// registered by previous spawns - so a type declared anywhere in the
    /// running program resolves even before its first spawn.
    fn find_actor_template(
        &self,
        name: &str,
        performing: &crate::bytecode::CodeModule,
    ) -> Option<(crate::bytecode::CodeModule, usize)> {
        fn find_in(module: &crate::bytecode::CodeModule, name: &str) -> Option<usize> {
            module
                .actor_metadata
                .iter()
                .find(|meta| meta.name == name)
                .and_then(|meta| meta.behavior_indices.first().copied())
        }
        if let Some(idx) = find_in(performing, name) {
            return Some((performing.clone(), idx));
        }
        if let Some(vm) = &self.vm {
            for module in &vm.modules {
                if let Some(idx) = find_in(module, name) {
                    return Some((module.clone(), idx));
                }
            }
        }
        for (module, _, _) in self.recovery_modules.values() {
            if let Some(idx) = find_in(module, name) {
                return Some((module.clone(), idx));
            }
        }
        None
    }

    // -- Supervisor Management --

    pub fn create_supervisor(&mut self, name: &str, strategy: RestartStrategy) -> u64 {
        let id = fresh_actor_id();
        let mut actor = Actor::new(id, name.to_string(), 0);
        actor.state = ActorState::Running;
        self.actors.insert(id, actor);
        let supervisor = Supervisor::new(id, name, strategy);
        self.supervisors.insert(id, supervisor);
        self.enqueue_actor(id);
        id
    }

    pub fn supervise_child(&mut self, supervisor_id: u64, spec: ChildSpec, child_id: u64) {
        // Snapshot everything a restart needs to rebuild the child, so a
        // supervised restart restores behaviors/bytecode/state instead of
        // producing a bare actor that silently drops every message.
        let restart = self.actors.get(&child_id).map(|actor| RestartTemplate {
            state_data: actor
                .state_data
                .iter()
                .map(|(name, value)| (name.clone(), *value))
                .collect(),
            state_models: actor.state_models.clone(),
            behaviors: actor
                .behavior_table
                .iter()
                .map(|entry| (entry.name.clone(), entry.handler_fn))
                .collect(),
            bytecode_module: actor.bytecode_module.clone(),
            bytecode_offsets: actor.bytecode_offsets.clone(),
            compensation_offsets: actor.compensation_offsets.clone(),
            persistent: actor.persistent,
            is_workflow: actor.is_workflow,
            is_agent: actor.is_agent,
        });
        let spec = ChildSpec { restart, ..spec };
        if let Some(child) = self.actors.get_mut(&child_id) {
            child.parent = Some(supervisor_id);
        }
        // RFC 0014 §4: `RespawnOnNodeLoss` opts the child into shadow
        // replication + the durable-actor directory (epoch starts at 1).
        // Only durable (`persistent`) actors are opt-able: a non-durable
        // actor has no snapshot to re-spawn from.
        if spec.restart_policy == RestartPolicy::RespawnOnNodeLoss
            && self
                .actors
                .get(&child_id)
                .map(|a| a.persistent)
                .unwrap_or(false)
        {
            self.respawn_opted.entry(child_id).or_insert(1);
            if let Some(cluster) = self.distributed.cluster.as_mut() {
                let node = self.distributed.node_id.unwrap_or(NodeId::LOCAL);
                cluster.announce_directory(DurableDirectoryEntry {
                    actor_id: child_id,
                    node_id: node,
                    epoch: 1,
                });
            }
        }
        if let Some(supervisor) = self.supervisors.get_mut(&supervisor_id) {
            supervisor.add_child(spec, child_id);
        }
    }

    /// Set the child template of a `SimpleOneForOne` supervisor by actor
    /// type name. Returns false when the supervisor does not exist or the
    /// actor type cannot be resolved (see `find_actor_template`).
    pub fn set_supervisor_template(
        &mut self,
        supervisor_id: u64,
        type_name: &str,
        performing: &crate::bytecode::CodeModule,
    ) -> bool {
        let Some((module, behavior_idx)) = self.find_actor_template(type_name, performing) else {
            return false;
        };
        match self.supervisors.get_mut(&supervisor_id) {
            Some(supervisor) => {
                supervisor.template = Some(ChildTemplate {
                    type_name: type_name.to_string(),
                    module,
                    behavior_idx,
                });
                true
            }
            None => false,
        }
    }

    /// Start a dynamic child of a `SimpleOneForOne` supervisor from its
    /// child template. Returns the new child's actor id, or `None` for an
    /// unknown supervisor, a missing template, or a non-dynamic strategy.
    pub fn start_supervised_child(
        &mut self,
        supervisor_id: u64,
        init_args: Vec<(String, Value)>,
    ) -> Option<u64> {
        let mut supervisor = self.supervisors.remove(&supervisor_id)?;
        let result = supervisor.start_child(self, init_args);
        self.supervisors.insert(supervisor_id, supervisor);
        result
    }

    /// Terminate a supervised child WITHOUT restarting it (clean Normal
    /// exit). Returns false when the supervisor or the child is unknown.
    pub fn terminate_supervised_child(&mut self, supervisor_id: u64, actor_id: u64) -> bool {
        let Some(mut supervisor) = self.supervisors.remove(&supervisor_id) else {
            return false;
        };
        let result = supervisor.terminate_child(self, actor_id);
        self.supervisors.insert(supervisor_id, supervisor);
        result
    }

    // -- Internal Helpers --

    fn send_down_message(&mut self, watcher_id: u64, target_id: u64, reason: &ExitReason) {
        exit::send_down_message(self, watcher_id, target_id, reason)
    }

    /// Shut a supervisor down, removing its children and the supervisor
    /// actor itself through the full exit protocol so registered names and
    /// process groups are cleaned up and monitors/links are notified.
    ///
    /// The `supervisor` value is passed in because callers remove it from
    /// `self.supervisors` before deciding to shut it down - looking it up in
    ///
    // -- Distributed Actor System --

    /// Configure the cluster before (or after) `enable_distribution`:
    /// split-brain resolver strategy and probe interval.
    ///
    /// Returns false (and keeps the previous configuration) when the
    /// configuration is invalid, e.g. `static-quorum` with
    /// `expected_nodes == 0`.
    pub fn set_cluster_config(&mut self, config: ClusterConfig) -> bool {
        if !config.is_valid() {
            warn!(
                "set_cluster_config: invalid cluster configuration \
                 (static-quorum expected_nodes must be >= 1)"
            );
            return false;
        }
        if let Some(cluster) = &mut self.distributed.cluster {
            return cluster.apply_config(&config);
        }
        self.cluster_config = config;
        true
    }

    /// Snapshot all runtime metrics into a single serializable struct.
    ///
    /// Used by `--verbose` output and available for external monitoring
    /// tooling.  All counters are lifetime totals; gauges reflect the
    /// current point-in-time value.
    pub fn metrics_snapshot(&self) -> MetricsSnapshot {
        let scheduler = self.scheduler_stats();
        let gc = self.gc_stats();
        let resolver = self
            .distributed
            .resolver
            .as_ref()
            .map(|r| r.stats())
            .unwrap_or_default();
        let dlq = self.dlq_depth();
        let mailboxes: Vec<ActorMailboxMetric> = self
            .mailbox_depths()
            .into_iter()
            .map(|(id, depth)| ActorMailboxMetric {
                actor_id: id,
                depth,
            })
            .collect();

        // Supervision tree topology.
        let supervisors = self
            .supervisors
            .iter()
            .map(|(id, sup)| SupervisorMetric {
                id: *id,
                name: sup.name.clone(),
                strategy: format!("{:?}", sup.strategy),
                parent: sup.parent,
                children: sup
                    .children
                    .iter()
                    .map(|(spec, actor_id)| SupervisorChildMetric {
                        actor_id: *actor_id,
                        spec_id: spec.id.clone(),
                    })
                    .collect(),
            })
            .collect();

        // CRDT replication state. `CrdtEntry` is not PartialEq, so an entry
        // counts as an unsynced delta when its serialized state differs from
        // the sync base (i.e. changes generated since the last delta sync).
        let crdt = match &self.crdt_manager {
            Some(m) => {
                let unsynced_deltas = m
                    .entries
                    .iter()
                    .filter(|(id, e)| {
                        m.sync_base.get(id).map(|b| b.payload_bytes()) != Some(e.payload_bytes())
                    })
                    .count();
                CrdtMetric {
                    node_id: m.node_id,
                    entries: m.entries.len(),
                    ops_synced: m.ops_synced,
                    unsynced_deltas,
                }
            }
            None => CrdtMetric {
                node_id: 0,
                entries: 0,
                ops_synced: 0,
                unsynced_deltas: 0,
            },
        };

        MetricsSnapshot {
            actors_live: self.actor_count() as u64,
            actors_mailboxes: mailboxes,
            dlq_depth: dlq as u64,
            scheduler,
            gc,
            resolver,
            supervisors,
            crdt,
        }
    }

    /// Render the runtime's supervision tree and CRDT state as ASCII text
    /// for a terminal topology view.
    pub fn render_topology(&self) -> String {
        self.metrics_snapshot().render_topology_text()
    }

    /// Start a Prometheus-format metrics server on the given port.
    ///
    /// Spawns a background TCP listener that serves `GET /metrics`.
    /// Call [`publish_metrics`](Runtime::publish_metrics) periodically
    /// (e.g. from the scheduler loop or `sync_crdts`) to push fresh
    /// snapshots.
    pub fn enable_metrics_server(&mut self, port: u16) -> std::io::Result<()> {
        if self.metrics.is_some() {
            return Ok(()); // already running
        }
        self.metrics = Some(metrics::MetricsServer::start(port)?);
        Ok(())
    }

    /// Publish the latest metrics snapshot to the Prometheus server.
    /// No-op if no server is running.
    pub fn publish_metrics(&self) {
        if let Some(server) = &self.metrics {
            let snap = self.metrics_snapshot();
            server.publish(snap.to_prometheus_text());
        }
        #[cfg(feature = "otel")]
        {
            let snap = self.metrics_snapshot();
            crate::observability::publish_otlp_metrics(&snap);
        }
    }
    #[cfg(feature = "tcp")]
    pub fn enable_distribution(
        &mut self,
        bind_addr: std::net::SocketAddr,
        tls_config: crate::runtime::network::TlsConfig,
    ) -> std::io::Result<()> {
        distribution::enable_distribution(self, bind_addr, tls_config)
    }

    /// Enable the distributed actor system over TCP.
    ///
    /// Stub used when the `tcp` feature is disabled: real TCP distribution
    /// is unavailable, so this always fails.
    #[cfg(not(feature = "tcp"))]
    pub fn enable_distribution(
        &mut self,
        bind_addr: std::net::SocketAddr,
    ) -> std::io::Result<()> {
        distribution::enable_distribution(self, bind_addr)
    }

    /// Enable distribution over a caller-supplied transport (DST: the
    /// in-memory `DeterministicNetworkTransport`). The transport's node
    /// id and listen address become this node's identity.
    pub fn enable_distribution_with_transport(
        &mut self,
        transport: Box<dyn NetworkTransport>,
    ) -> std::io::Result<()> {
        distribution::enable_distribution_with_transport(self, transport)
    }

    pub fn join_cluster(&mut self, seed_addr: std::net::SocketAddr) {
        distribution::join_cluster(self, seed_addr)
    }

    /// Register a behavior that remote nodes are allowed to spawn on this
    /// node by name (via `Packet::SpawnRequest`).
    ///
    /// This is the MVP scope of remote spawn: only native behaviors
    /// explicitly registered here can be spawned remotely - a node cannot
    /// make a peer run arbitrary code it never offered. When a spawn
    /// request for `name` arrives, the runtime spawns a fresh actor with
    /// the request's initial state and registers this handler as its sole
    pub fn register_spawnable_behavior(&mut self, name: &str, handler: fn(&mut Actor, &[Value])) {
        distribution::register_spawnable_behavior(self, name, handler)
    }

    /// Register an AOT-compiled module for native actor behavior dispatch.
    ///
    /// The Runtime takes ownership of the module and keys it by the actor
    /// types it declares. Actors of those types spawned afterwards dispatch
    /// their behaviors through AOT native code (bypassing the bytecode VM)
    /// when the behavior is compiled in the module; behaviors absent from the
    /// module keep their bytecode handlers.
    pub fn register_aot_module(&mut self, module: crate::aot::AotModule) {
        // Box the module so its address is stable, then register the raw
        // pointer for every actor type it declares.
        let boxed = Box::new(module);
        let module_ptr: *const crate::aot::AotModule = &*boxed;
        for name in unsafe { &*module_ptr }.actor_type_names() {
            self.aot_modules.entry(name).or_insert(module_ptr);
        }
        self.aot_module_storage.push(boxed);
    }

    /// Take the result of a previously issued remote spawn request.
    ///
    /// Returns `None` while the response has not arrived yet; otherwise
    /// `Some(Some(actor_id))` on success (the real actor id on the remote
    /// node - combine it with the node id into an `ActorAddress::remote`)
    /// or `Some(None)` if the remote node rejected the request (unknown
    /// behavior name).
    pub fn take_spawn_response(&mut self, request_id: u64) -> Option<Option<u64>> {
        distribution::take_spawn_response(self, request_id)
    }

    /// Check whether a packet with the given sequence number has been
    /// acknowledged by the receiver.
    pub fn is_acked(&self, seq: u64) -> bool {
        distribution::is_acked(self, seq)
    }

    /// Drain and return all acknowledged packet sequence numbers.
    ///
    /// Callers should drain periodically to avoid unbounded growth of the
    /// acked-packets set.
    pub fn drain_acked(&mut self) -> HashSet<u64> {
        distribution::drain_acked(self)
    }

    pub fn send_distributed(&mut self, target: ActorAddress, behavior: &str, args: &[Value]) {
        distribution::send_distributed(self, target, behavior, args)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub fn process_network(&mut self) {
        distribution::process_network(self)
    }

    // -- CRDT Synchronization (v0.6) --

    /// Synchronize CRDT state with all healthy cluster members.
    ///
    /// Most rounds ship **delta-state** ops (`Packet::CrdtDeltaSync`) -
    /// only the changes since the previous round, with never-synced
    /// entries sent in full (the join fallback). Every
    /// `CRDT_FULL_SYNC_INTERVAL`-th round (starting with the first) ships
    /// full state (`Packet::CrdtSync`) instead: the sync base advances
    /// when deltas are generated, so a lost delta is never re-sent and
    /// these periodic full syncs are the repair mechanism.
    pub fn sync_crdts(&mut self) {
        distribution::sync_crdts(self)
    }

    /// Full-state CRDT sync: ship every entry to all healthy members.
    pub(crate) fn sync_crdts_full(&mut self) {
        let ops = match &mut self.crdt_manager {
            Some(m) => m.generate_sync_ops(),
            None => return,
        };
        if ops.is_empty() {
            return;
        }
        let packet = Packet::CrdtSync { ops: Arc::new(ops) };
        if let Some(cluster) = &self.distributed.cluster {
            for member in cluster.healthy_members() {
                if let Some(transport) = &mut self.distributed.transport {
                    let net_node_id = NodeId(member.node_id.0);
                    transport.send(net_node_id, member.address, packet.clone());
                }
            }
        }
    }

    /// Garbage-collect forwarding entries for actors that migrated
    /// longer ago than [`MIGRATED_ACTOR_TTL`].  Once the TTL expires
    /// messages for the actor that still reach the old node bounce to
    /// the DLQ instead of being forwarded.
    pub(crate) fn sweep_migrated_actors(&mut self) {
        let now = self.now();
        self.migrated_actors
            .retain(|_actor_id, (_target, migrated_at)| {
                now.duration_since(*migrated_at) < MIGRATED_ACTOR_TTL
            });
    }

    /// Store a shadow replica received over the wire (RFC 0014 §3). The
    /// replica is kept, not instantiated; the re-spawn driver consumes it
    /// when the actor's home node is confirmed removed. A same-epoch replica
    /// (a later checkpoint within one activation) MUST replace the earlier
    /// one, or the shadow would hold the *first* checkpoint and re-spawn
    /// silently lose every later durable write.
    pub(crate) fn store_shadow_replica(
        &mut self,
        actor_id: u64,
        nbc_bytes: Vec<u8>,
        snapshot_json: Vec<u8>,
        epoch: u64,
    ) {
        let replace = match self.shadow_replicas.get(&actor_id) {
            Some(existing) => epoch >= existing.epoch,
            None => true,
        };
        if replace {
            self.shadow_replicas.insert(
                actor_id,
                ShadowReplica {
                    nbc_bytes,
                    snapshot_json,
                    epoch,
                },
            );
        }
    }

    /// Replicate an actor's durable snapshot to its deterministic shadow
    /// (RFC 0014 §3). Called from `checkpoint_actor` after the local
    /// snapshot is saved; only re-spawn-opted actors replicate. The
    /// directory entry is re-announced on success so its presence implies
    /// an acknowledged replica.
    pub(crate) fn maybe_shadow_replicate(
        &mut self,
        actor_id: u64,
        snapshot: &crate::runtime::persistence::ActorSnapshot,
    ) {
        let Some(&epoch) = self.respawn_opted.get(&actor_id) else {
            return;
        };
        let Some(cluster) = self.distributed.cluster.as_ref() else {
            return;
        };
        let home = self.distributed.node_id.unwrap_or(NodeId::LOCAL);
        let Some(shadow) = shadow_for(cluster, home, actor_id) else {
            return;
        };
        if shadow == home {
            return;
        }
        let Ok(snapshot_json) = serde_json::to_vec(snapshot) else {
            return;
        };
        let module = match self
            .actors
            .get(&actor_id)
            .and_then(|a| a.bytecode_module.clone())
            .or_else(|| {
                self.recovery_modules
                    .get(&actor_id)
                    .map(|(m, _, _)| m.clone())
            }) {
            Some(m) => m,
            None => return,
        };
        let Ok(nbc_bytes) = module.to_nbc(None) else {
            return;
        };
        let packet = Packet::ShadowReplicate {
            actor_id,
            nbc_bytes,
            snapshot_json,
            epoch,
        };
        let Some(addr) = cluster.get_node(shadow).map(|info| info.address) else {
            return;
        };
        if let Some(transport) = &mut self.distributed.transport {
            transport.send(shadow, addr, packet);
            if let Some(cluster) = self.distributed.cluster.as_mut() {
                cluster.announce_directory(DurableDirectoryEntry {
                    actor_id,
                    node_id: home,
                    epoch,
                });
            }
        }
    }

    /// Terminate any local actor whose activation epoch has been superseded
    /// by a newer directory entry (RFC 0014 §5 self-demote): a node that was
    /// confirmed gone and re-joins later must not resume writing durable
    /// state for an actor a survivor already re-spawned.
    pub(crate) fn self_demote_superseded(&mut self) {
        let superseded: Vec<(u64, NodeId)> = self
            .distributed
            .cluster
            .as_ref()
            .map(|c| {
                self.respawn_opted
                    .iter()
                    .filter_map(|(actor_id, epoch)| {
                        c.directory_entry(*actor_id)
                            .filter(|entry| entry.epoch > *epoch)
                            .map(|entry| (*actor_id, entry.node_id))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (actor_id, replacement_node) in superseded {
            // Reap the stale local copy; the directory entry (and the
            // survivor's replica) is authoritative for the newer epoch.
            crate::runtime::exit::reap_living_actor(
                self,
                actor_id,
                crate::types::ExitReason::NoConnection,
            );
            self.respawn_opted.remove(&actor_id);
            // The replaced actor lives on the directory entry's node now:
            // forward sends there (NOT self — that would loop).
            self.migrated_actors
                .insert(actor_id, (replacement_node, std::time::Instant::now()));
            tracing::warn!(
                "nulang-respawn: self-demoted stale actor {} (directory epoch newer)",
                actor_id
            );
        }
    }

    /// Make the local node's `NodeGoodbye` declaration true (RFC 0014 §1
    /// path 1): checkpoint every re-spawn-opted durable actor (which also
    /// replicates the final snapshot to its shadow) and then terminate the
    /// local copy. A goodbye that merely lists the manifest without
    /// terminating would leave two live copies once the shadow re-spawns.
    pub(crate) fn goodbye_self(&mut self) {
        let opted: Vec<u64> = self.respawn_opted.keys().copied().collect();
        for actor_id in opted {
            // Skip non-durable entries (they cannot be checkpointed and are
            // not in the directory's re-spawn set anyway).
            if !self
                .actors
                .get(&actor_id)
                .map(|a| a.persistent)
                .unwrap_or(false)
            {
                continue;
            }
            self.checkpoint_actor(actor_id);
            crate::runtime::exit::reap_living_actor(
                self,
                actor_id,
                crate::types::ExitReason::Normal,
            );
        }
    }
}

/// Deterministic shadow for a durable actor (RFC 0014 §3): the healthy
/// member with the smallest node id excluding the actor's home node. Called
/// identically at checkpoint time (home computes its own shadow) and at
/// re-spawn time (each survivor computes whether it is the shadow), so the
/// node that holds the replica is the node that re-spawns — no leader
/// election, exactly one re-spawn per actor.
pub(crate) fn shadow_for(cluster: &ClusterState, home: NodeId, _actor_id: u64) -> Option<NodeId> {
    cluster
        .all_members()
        .iter()
        .filter(|info| info.status == NodeStatus::Healthy && info.node_id != home)
        .map(|info| info.node_id)
        .min()
}

/// A shadow replica of a durable actor's snapshot (RFC 0014 §3).
pub(crate) struct ShadowReplica {
    pub nbc_bytes: Vec<u8>,
    pub snapshot_json: Vec<u8>,
    pub epoch: u64,
}

/// Interval (in `sync_crdts` rounds) between full-state repair syncs.
/// Round 1 is full; rounds 2..=N are delta; round N+1 is full again.
const CRDT_FULL_SYNC_INTERVAL: u64 = 16;
/// How often (in scheduler ticks) deferred local decrements are retried
/// while actors are still running. Used by both the production
/// `run_scheduler` and the deterministic DST scheduler.
const GC_PUMP_INTERVAL: u64 = 256;
/// How long (wall-clock) a migrated-actor forwarding entry is kept
/// before it is garbage-collected.  After this TTL, messages for the
/// actor that still reach the old node are bounced to the DLQ (target
/// actor not found).  In-flight messages should arrive within seconds;
const MIGRATED_ACTOR_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Point-in-time snapshot of all runtime metrics (counters and gauges).
///
/// Serializable for JSON export; consumed by `--verbose` output and
/// available for external monitoring tooling.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricsSnapshot {
    pub actors_live: u64,
    pub actors_mailboxes: Vec<ActorMailboxMetric>,
    pub dlq_depth: u64,
    pub scheduler: SchedulerStats,
    pub gc: GcStats,
    pub resolver: ResolverStats,
    /// Supervision tree topology: one entry per supervisor, with its parent
    /// link and children. Parent links reconstruct the tree.
    pub supervisors: Vec<SupervisorMetric>,
    /// CRDT replication state.
    pub crdt: CrdtMetric,
}

/// Per-actor mailbox depth for the metrics snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActorMailboxMetric {
    pub actor_id: u64,
    pub depth: usize,
}

/// One supervisor in the topology snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SupervisorMetric {
    pub id: u64,
    pub name: String,
    /// `RestartStrategy` variant name (OneForOne, OneForAll, RestForOne,
    /// SimpleOneForOne).
    pub strategy: String,
    /// Parent supervisor actor id, if this supervisor is itself supervised.
    pub parent: Option<u64>,
    pub children: Vec<SupervisorChildMetric>,
}

/// A supervised child actor.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SupervisorChildMetric {
    pub actor_id: u64,
    /// `ChildSpec.id` — stable across restarts (the actor id changes).
    pub spec_id: String,
}

/// CRDT replication state.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CrdtMetric {
    pub node_id: u64,
    /// Number of live CRDT entries (replicas).
    pub entries: usize,
    /// Total CRDT ops shipped.
    pub ops_synced: u64,
    /// Entries whose state differs from the last-synced base — changes not
    /// yet replicated. Computed by serializing and comparing (CrdtEntry is
    /// not PartialEq).
    pub unsynced_deltas: usize,
}

/// True when the given 1-based sync round should ship full state.
///
// ---------------------------------------------------------------------------
// CycleRuntime implementation
// ---------------------------------------------------------------------------

impl crate::runtime::orca_cycle::CycleRuntime for Runtime {
    unsafe fn free_object(&mut self, actor_id: u64, header: *mut crate::runtime::heap::OrcaHeader) {
        if let Some(actor) = self.actors.get_mut(&actor_id) {
            // Remove from deferred-decrement list first so a later
            // `process_deferred` pass does not touch freed memory.
            actor.orca_gc.remove_deferred(header);

            // Compute the payload pointer and free on the owning actor's heap.
            let header_size = std::mem::size_of::<crate::runtime::heap::OrcaHeader>();
            let payload_ptr = (header as *mut u8).add(header_size);
            actor.heap.free(payload_ptr);
        }
    }
}

// VM runtime callbacks
// ---------------------------------------------------------------------------

fn map_ast_state_model(model: crate::ast::StateModel) -> crate::runtime::persistence::StateModel {
    use crate::ast::StateModel as AstModel;
    use crate::runtime::persistence::StateModel as RuntimeModel;
    match model {
        AstModel::Local => RuntimeModel::Local,
        AstModel::Durable => RuntimeModel::Durable,
        AstModel::EventSourced => RuntimeModel::EventSourced,
        AstModel::Crdt(t) => RuntimeModel::Crdt(t),
    }
}

/// Convert a JSON value into a Nulang VM value for tool-call arguments.
#[cfg(feature = "ai-runtime")]
fn json_to_vm_value(
    vm: &mut crate::vm::VM,
    value: serde_json::Value,
) -> Result<crate::vm::Value, String> {
    match value {
        serde_json::Value::Null => Ok(crate::vm::Value::nil()),
        serde_json::Value::Bool(b) => Ok(crate::vm::Value::bool(b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(crate::vm::Value::int(i))
            } else {
                Ok(crate::vm::Value::float(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Ok(vm.allocate_string(&s)),
        _ => Err("Unsupported tool argument type".to_string()),
    }
}

/// Compute the backoff delay in milliseconds for the given retry attempt
/// (0-indexed). Uses actor_id as a seed for ±25% jitter to avoid
/// deterministic thundering-herd on clusters.
#[cfg(feature = "ai-runtime")]
fn compute_backoff(config: &crate::ast::AgentRetryConfig, attempt: u32, actor_id: u64) -> u64 {
    let base_ms = match &config.backoff {
        crate::ast::AgentBackoff::Exponential {
            initial_ms,
            factor,
            max_ms,
        } => {
            let exp = (*factor).powi(attempt as i32);
            let delay = (*initial_ms as f64 * exp).min(*max_ms as f64);
            delay as u64
        }
        crate::ast::AgentBackoff::Fixed { delay_ms } => *delay_ms,
    };
    // ±25% jitter, seeded from actor_id so different actors (or the same
    // actor on different nodes with different ids) get different jitter.
    let seed = actor_id
        .wrapping_mul(6364136223846793005)
        .wrapping_add(attempt as u64);
    let r = (seed >> 33) as f64 / (1u64 << 31) as f64; // [0, 1)
    let jittered = base_ms as f64 + (base_ms as f64 * 0.5 * (r - 0.5));
    jittered.max(0.0) as u64
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}
