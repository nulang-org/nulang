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
mod behavior_ownership;
mod blocking_executor;
pub mod cache;
pub mod cache_cluster;
pub mod cache_dispatch;
pub mod cache_pipeline;
pub mod cache_routing;
#[cfg(feature = "cache-server")]
pub mod cache_server;
mod foreign_call;
mod foreign_executor;
mod gc;
pub mod heap;
pub(crate) mod heap_serialize;
mod mailbox;
mod scheduler;
mod schema_identity;
pub use heap_serialize::*;
mod cluster;
mod distributed;
mod distributed_context;
mod fabric_stream;
mod fabric_stream_cluster;
mod fabric_stream_epoch;
mod grain;
mod network;
mod object_store;
mod orca_cycle;
mod supervision;
mod supervisor;
pub use distributed_context::{
    DistributedContext, FabricAdvertisement, FabricAdvertisementSnapshot, FabricPublishReport,
};
pub use fabric_stream::{
    FabricStreamConfig, FabricStreamInfo, FabricStreamRecord, FileFabricStreamStore,
};
pub use fabric_stream_cluster::{
    FabricStreamCatchUpReport, FabricStreamPlacement, FabricStreamRecoveryReport,
    FabricStreamReplicaAppend, FabricStreamReplicaDispatchReport,
    FabricStreamReplicatedAppendResult, FabricStreamReplicationStatus, FabricStreamRetryReport,
};
pub use fabric_stream_epoch::{
    FabricStreamAutoFailoverReport, FabricStreamEpochPullReport, FabricStreamEpochRepairReport,
    FabricStreamEpochTransitionStatus,
};
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
#[cfg(feature = "sqlite")]
mod libsql_tls;
#[cfg(feature = "ai-runtime")]
mod llm;
mod metrics;
mod nudb_transition;
mod persistence;
mod process_groups;
mod registry;
pub mod resp;
pub mod resp_cache;
mod spawn;
#[cfg(feature = "native-codegen")]
pub(crate) use spawn::spawn_from_module_with_authority;
mod timer;
mod trace;
mod workflow;
pub use trace::TraceContext;

#[cfg(test)]
mod cluster_dst;

#[cfg(test)]
mod cluster_dst_trace_tests;

#[cfg(test)]
mod cluster_sim;

#[cfg(test)]
mod tests;

pub use actor::*;
pub use blocking_executor::*;
pub use cache::*;
pub use cache_cluster::*;
pub use cache_dispatch::*;
pub use cache_pipeline::*;
pub use cache_routing::*;
#[cfg(feature = "cache-server")]
pub use cache_server::*;
pub use callbacks::RuntimeVmCallbacks;
pub(crate) use callbacks::{BytecodeDistributedCallbacks, BytecodeRuntimeCallbacks};
pub use cluster::*;
pub use crdt::*;
pub use crdt_manager::*;
pub use crdt_reg::{LWWRegister, MVRegister, RGAElement, RGA};
pub use distributed::*;
pub use foreign_call::*;
pub use foreign_executor::*;
pub use gc::{ForeignRefOp, GcStats, OrcaCoordinator, OrcaGc, OrcaHeap};
pub use grain::*;
pub use heap::*;
pub use http_server::{render_route_handler, HttpMethod, HttpServerState, WebDevServer, WebRoute};
pub use mailbox::*;
pub use network::NetworkTransport;
pub use network::*;
pub use nudb_transition::NuDbTransitionJournal;
pub use object_store::*;
pub use orca_cycle::*;
pub use persistence::*;
pub use process_groups::*;
pub use registry::*;
pub use resp_cache::*;
pub use scheduler::*;
pub use supervisor::*;
pub use timer::*;

use crate::crdt_peer_sync::PeerCrdtFrontiers;
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

/// RAII ownership for ObjectRefs while a cross-shard envelope is in flight.
///
/// The source acquires these refs before publishing the message. On successful
/// destination mailbox admission they are transferred into the receiving
/// actor's lifetime hold set. Any send, resolution, hydration, or admission
/// failure drops the lease and rolls the temporary refs back automatically.
#[derive(Debug)]
struct ObjectRefLease {
    store: ObjectStore,
    ids: Vec<ObjectId>,
}

impl ObjectRefLease {
    fn acquire(store: &ObjectStore, payload: &[Value]) -> Option<Self> {
        let mut seen = HashSet::new();
        let mut ids = Vec::new();

        for id in payload.iter().filter_map(|value| value.as_object_id()) {
            if !seen.insert(id) {
                continue;
            }
            if !store.clone_ref(id) {
                for acquired in ids.drain(..) {
                    store.drop_ref(acquired);
                }
                return None;
            }
            ids.push(id);
        }

        Some(Self {
            store: store.clone(),
            ids,
        })
    }

    fn transfer_to_actor(mut self, actor: &mut Actor) {
        if self.ids.is_empty() {
            return;
        }

        if actor.held_object_store.is_none() {
            actor.held_object_store = Some(self.store.clone());
        }

        for id in self.ids.drain(..) {
            if !actor.held_objects.insert(id) {
                // The actor already owns a lifetime hold for this ObjectId.
                // Drop the redundant in-flight reference.
                self.store.drop_ref(id);
            }
        }
    }
}

impl Drop for ObjectRefLease {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            self.store.drop_ref(id);
        }
    }
}

/// A message routed between Runtime shards in a multi-threaded deployment.
///
/// Each shard owns a disjoint subset of actors (by `actor_id % shard_count`).
/// Cross-shard messages carry shard-safe values, including `TAG_OBJECT`
/// handles backed by the node-shared immutable ObjectStore. Actor-heap pointers,
/// actor refs, and closures remain forbidden, keeping ORCA ownership shard-local.
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
        object_holds: ObjectRefLease,
    },
    /// Name-based delivery to an actor owned by another shard. The owning
    /// shard resolves the name against the target actor's behavior table;
    /// the source shard must never invent a numeric fallback.
    DeliverNamedMessage {
        target_id: u64,
        behavior_name: String,
        payload: Vec<Value>,
        sender: u64,
        trace_id: Option<String>,
        object_holds: ObjectRefLease,
    },
    /// Enqueue an actor on the target shard (wake from idle/waiting).
    EnqueueActor { actor_id: u64 },
}

/// Admission result for a local-process actor delivery.
///
/// Fabric uses this to distinguish successful mailbox/channel admission from
/// bounded-capacity backpressure without changing the public actor-send API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageAdmission {
    Accepted,
    Backpressured,
    Rejected,
}

pub struct Runtime {
    pub actors: HashMap<u64, Actor>,
    pub supervisors: HashMap<u64, Supervisor>,
    pub scheduler: Scheduler,
    pub next_run_slot: Option<u64>,
    pub actor_pool: Vec<Actor>,
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

    /// Durable actors opted into `RespawnOnNodeLoss` (RFC 0014).
    ///
    /// Epoch ownership lives on the Actor itself; this set records only
    /// participation in node-loss shadow replication and directory gossip.
    pub(crate) respawn_opted: HashSet<u64>,
    /// Shadow replicas this node holds (RFC 0014 §3): actor id → replica,
    /// received via `Packet::ShadowReplicate` from the actor's home node.
    /// Consumed by the re-spawn driver when the home node is confirmed gone.
    pub(crate) shadow_replicas: HashMap<u64, ShadowReplica>,

    // CRDT manager (v0.6)
    pub crdt_manager: Option<CrdtManager>,
    /// Receiver-specific CRDT knowledge. A peer frontier advances only after
    /// the transport ACK for the packet carrying that logical batch arrives.
    pub(crate) crdt_peer_frontiers: PeerCrdtFrontiers,
    /// Tracked NUL0 packet sequence -> (peer id, logical CRDT batch id).
    ///
    /// At most the newest tracked batch per peer is retained. Ignoring an
    /// older late ACK can cause a harmless retransmission; accepting an ACK
    /// for a packet we no longer track could incorrectly advance knowledge.
    pub(crate) pending_crdt_sync_acks: HashMap<u64, (u64, u64)>,

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
    /// Canonical ActorMeta.name retained after actor reaping so migration
    /// forwarding and recovery can translate runtime-local behavior ids
    /// without guessing schema ownership.
    pub(crate) recovery_schema_names: HashMap<u64, String>,
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
    #[cfg(feature = "native-codegen")]
    pub aot_modules: std::collections::HashMap<String, *const crate::aot::AotModule>,
    /// Owns the registered AOT modules so the raw pointers in `aot_modules`
    /// (and on actors) stay valid for the Runtime's lifetime.
    #[cfg(feature = "native-codegen")]
    pub aot_module_storage: Vec<Box<crate::aot::AotModule>>,
    /// Actor ID of the dead-letter queue (created lazily).
    /// Undeliverable messages are routed here.
    pub dlq_actor_id: Option<u64>,
    /// Callback invoked when the scheduler loop reaches true quiescence
    /// (empty run queue, no inflight LLM calls, no pending timers).
    /// The embedder (e.g. NLC guest agent) wires this to host signaling.
    pub idle_callback: Option<Box<dyn FnMut()>>,
    // HTTP server state (v0.7+).
    pub http_server: Option<HttpServerState>,
    // Test effect interception is deliberately absent from production builds:
    // privileged host effects must not have a pre-authority dispatch hook.
    #[cfg(test)]
    test_handlers: HashMap<String, Box<dyn Fn(&[Value]) -> Option<Value>>>,
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
            next_run_slot: None,
            actor_pool: Vec::new(),
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
            respawn_opted: HashSet::new(),
            shadow_replicas: HashMap::new(),
            // Standalone runtimes use node id 0; `enable_distribution` swaps
            // this for a real node-id manager. Initialized eagerly so
            // `state crdt` fields register and `Crdt.*` ops work without
            // distribution enabled.
            crdt_manager: Some(CrdtManager::new(0)),
            crdt_peer_frontiers: PeerCrdtFrontiers::new(),
            pending_crdt_sync_acks: HashMap::new(),
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
            recovery_schema_names: HashMap::new(),
            #[cfg(feature = "ai-runtime")]
            ai: AiRuntimeRegistry::new(),
            #[cfg(feature = "ai-runtime")]
            supervisor_teams: SupervisorTeamRegistry::new(),
            crypto: Box::new(crate::backends::DefaultCryptoProvider::new()),
            spawnable_behaviors: HashMap::new(),
            #[cfg(feature = "native-codegen")]
            aot_modules: std::collections::HashMap::new(),
            #[cfg(feature = "native-codegen")]
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
            #[cfg(test)]
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
        object_store: ObjectStore,
    ) -> Self {
        let mut rt = Runtime::new();
        rt.object_store = object_store;
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

        let object_store = ObjectStore::new();
        let mut shards = Vec::with_capacity(num_shards);
        for (i, (_tx, rx)) in channels.into_iter().enumerate() {
            shards.push(Runtime::new_shard(
                i as u16,
                num_shards as u16,
                senders.clone(),
                rx,
                object_store.clone(),
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
    #[cfg(test)]
    pub(crate) fn install_test_handler<F>(&mut self, effect_name: &str, handler: F)
    where
        F: Fn(&[Value]) -> Option<Value> + 'static,
    {
        self.test_handlers
            .insert(effect_name.to_string(), Box::new(handler));
    }

    /// Check whether a test handler is installed for `qualified_name` and
    /// return its result if so.
    #[cfg(test)]
    pub(crate) fn check_test_handler(&self, qualified_name: &str, regs: &[Value]) -> Option<Value> {
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

    /// Spawn a durable workflow actor. Workflows are always persistent and
    /// keep an append-only event journal in addition to snapshots.
    ///
    /// This compatibility API returns actor id 0 when the initial durable
    /// commit fails. New callers that need the persistence error should use
    /// `try_spawn_workflow_actor`.
    pub fn spawn_workflow_actor(
        &mut self,
        name: &str,
        init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
        state_models: HashMap<String, StateModel>,
    ) -> u64 {
        match self.try_spawn_workflow_actor(name, init, state_models) {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(workflow = name, %error, "durable workflow spawn failed");
                0
            }
        }
    }

    /// Spawn a durable workflow and report failure if its initial
    /// `WorkflowStarted` journal entry or first snapshot cannot be committed.
    /// The actor is not published or scheduled until both writes succeed.
    pub fn try_spawn_workflow_actor(
        &mut self,
        name: &str,
        init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
        state_models: HashMap<String, StateModel>,
    ) -> std::io::Result<u64> {
        spawn::try_spawn_actor_with_models(self, init, state_models, true, Some(name), None)
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
