//! Actor definition and lifecycle.

use super::gc::OrcaGc;
use super::*;
use crate::runtime::object_store::ObjectId;
use crate::vm::Value;
use std::collections::{HashMap, HashSet};

/// Actor state machine: Created → Running → Waiting → Suspended → Terminated
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorState {
    Created,
    Running,
    Waiting,    // Mailbox empty, no work
    Suspended,  // Explicitly suspended
    Terminated, // Actor has exited
}

/// Scheduling priority of an actor (Erlang-style process priority).
///
/// The scheduler dequeues ready High-priority actors before Normal, and
/// Normal before Low (strict per-level preference, FIFO within a level —
/// see `Scheduler::enqueue_with_priority`). Priority affects scheduling
/// order only; it does not touch message delivery order
/// (`Mailbox::receive_match` stays FIFO and ignores `Message::priority`).
/// Set from Nulang via `perform Actor.set_priority(0|1|2)`.
/// Execution backend for an actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActorBackend {
    /// Native bytecode with JIT tiering (trusted, default).
    Native,
    /// WASM Component with share-nothing isolation (untrusted).
    WasmComponent { component_path: String },
}

impl Default for ActorBackend {
    fn default() -> Self {
        ActorBackend::Native
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActorPriority {
    High,
    #[default]
    Normal,
    Low,
}

// -- Flight recorder (deterministic replay support) ---------------------

/// A single entry in an actor's flight-recorder trace.  Captures enough
/// information to deterministically replay the message sequence that led
/// to a crash or unexpected behavior.
#[derive(Debug, Clone)]
pub struct TraceEntry {
    /// Per-actor monotonic sequence number (arrival order).
    pub seq: u64,
    /// Actor ID of the sender (0 = system/runtime message).
    pub sender: u64,
    /// Target behavior ID.
    pub behavior_id: u16,
    /// Number of payload arguments.
    pub payload_len: usize,
    /// Human-readable summary of the first few payload values.
    pub payload_summary: String,
}

/// A fixed-size ring buffer that records the most recent N messages
/// delivered to an actor.  When the buffer is full, oldest entries are
/// overwritten.
#[derive(Debug, Clone)]
pub struct FlightRecorder {
    entries: Vec<TraceEntry>,
    /// Write cursor (next slot to fill).
    cursor: usize,
    /// Monotonic sequence counter for this actor.
    next_seq: u64,
    /// Maximum number of entries to retain.
    max_entries: usize,
}

impl FlightRecorder {
    /// Create a new flight recorder retaining up to `max_entries` messages.
    pub fn new(max_entries: usize) -> Self {
        FlightRecorder {
            entries: Vec::with_capacity(max_entries),
            cursor: 0,
            next_seq: 0,
            max_entries,
        }
    }

    /// Record a message delivery.
    pub fn record(&mut self, sender: u64, behavior_id: u16, payload: &[Value]) {
        let seq = self.next_seq;
        self.next_seq += 1;

        let payload_len = payload.len();
        let payload_summary = payload
            .iter()
            .take(3)
            .map(|v| v.to_string_repr())
            .collect::<Vec<_>>()
            .join(", ");

        let entry = TraceEntry {
            seq,
            sender,
            behavior_id,
            payload_len,
            payload_summary,
        };

        if self.entries.len() < self.max_entries {
            self.entries.push(entry);
        } else {
            self.entries[self.cursor] = entry;
            self.cursor = (self.cursor + 1) % self.max_entries;
        }
    }

    /// Return all recorded entries (raw ring buffer — use ordered_entries()
    /// for chronological order).
    pub fn entries(&self) -> &[TraceEntry] {
        &self.entries
    }

    /// Return entries in chronological order (oldest first), even when the
    /// ring buffer has wrapped.
    pub fn ordered_entries(&self) -> Vec<&TraceEntry> {
        if self.entries.is_empty() {
            return vec![];
        }
        if self.entries.len() < self.max_entries {
            return self.entries.iter().collect();
        }
        // Full ring: oldest is at cursor, newest at cursor-1 (wrapping)
        let mut result = Vec::with_capacity(self.entries.len());
        for i in 0..self.entries.len() {
            let idx = (self.cursor + i) % self.entries.len();
            result.push(&self.entries[idx]);
        }
        result
    }

    /// Number of entries recorded.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no messages have been recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all recorded entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cursor = 0;
        self.next_seq = 0;
    }
}
/// Serialized hibernation state for an actor.
#[derive(Debug, Clone)]
pub struct HibernationState {
    /// Serialized VM continuation and handler stack.
    pub continuation_bytes: Vec<u8>,
    /// Module hash at time of hibernation.
    pub module_hash: [u8; 32],
    /// Timestamp when hibernated (milliseconds since epoch).
    pub hibernated_at_ms: u64,
    /// Actor's state fields at time of hibernation.
    pub state_fields: HashMap<String, Value>,
}

/// An actor: independent unit of computation with isolated state and mailbox.
pub struct Actor {
    pub id: u64,
    pub name: String,
    pub state: ActorState,
    pub mailbox: Mailbox,
    pub heap: ActorHeap,
    pub orca_gc: OrcaGc,                    // ORCA GC engine for this actor
    /// Wave D4 per-activation bump arena for allocations proven
    /// message-scoped by the conservative escape analysis in
    /// [`crate::iso_arena`].  Reset in O(1) when a handler activation
    /// completes without suspending.  Unused unless the VM's iso-arena
    /// flag is on (`NULANG_ISO_ARENA=1` / `--iso-arena`).
    pub iso_arena: crate::iso_arena::IsoArena,
    pub state_data: HashMap<String, Value>, // Named actor state fields
    pub state_models: HashMap<String, StateModel>, // Persistence model per field
    pub event_log: Vec<(String, Vec<Value>)>, // Emitted events for event_sourced actors
    /// Last persisted event sequence per EventSourced field, for compaction tracking.
    pub event_sourced_sequences: HashMap<String, u64>,
    /// How many events between compaction snapshots for EventSourced fields (default 100).
    pub event_sourced_compaction_interval: u64,
    pub persistent: bool,  // Whether this actor survives restarts
    pub is_workflow: bool, // True if generated from a workflow declaration
    pub behavior_table: Vec<BehaviorEntry>,
    /// AOT-compiled behavior targets, parallel to `behavior_table`. `Some`
    /// means the behavior at that index dispatches through AOT native code;
    /// the scheduler arms the target before invoking `handler_fn`.
    pub aot_targets: Vec<Option<crate::aot::AotDispatchTarget>>,
    /// Bytecode behavior offsets by behavior_id. Empty entries mean no bytecode
    /// handler for that behavior (native handler or missing).
    pub bytecode_offsets: Vec<usize>,
    /// Saga compensation code offsets by behavior_id. `None` means the step has
    /// no compensation expression.
    pub compensation_offsets: Vec<Option<usize>>,
    /// Names of steps already compensated (used during recovery replay).
    pub compensated_steps: Vec<String>,
    /// Bytecode module used by this actor's bytecode behaviors.
    pub bytecode_module: Option<crate::bytecode::CodeModule>,
    /// Index of the loaded bytecode module in the runtime VM.
    pub bytecode_module_idx: Option<usize>,
    pub parent: Option<u64>, // Supervisor
    pub children: Vec<u64>,  // Supervised actors
    pub monitors: Vec<u64>,  // Actors monitoring this one
    pub links: Vec<u64>,     // Bidirectional links
    pub trap_exits: bool,    // If true, exit signals become messages instead of killing this actor
    /// Scheduling priority, consulted by the scheduler on every enqueue.
    pub priority: ActorPriority,
    pub reduction_count: u32, // Lifetime messages handled (monotonic progress metric)
    turn_reductions: u32,     // Messages handled in the current scheduling turn
    pub max_reductions: u32,  // Max reductions per turn before yield (preemption)
    pub sequence: u64,        // Last persisted sequence number
    /// Sentinel heap object used by the cycle detector to represent this
    /// actor as a holder of foreign references.
    cycle_sentinel: Option<*mut OrcaHeader>,
    /// Suspended VM state for a workflow step waiting on a signal.
    pub suspended_execution: Option<SuspendedExecution>,
    /// JIT safepoint counter: decremented per JIT region entry in JIT code.
    /// When it reaches 0, the JIT yields back to the scheduler.
    pub jit_safepoint_counter: u64,
    /// True when suspended_execution holds a JIT-yield suspension
    /// (as opposed to an LLM/signal/receive-wait suspension).
    pub jit_yield_pending: bool,
    /// Name of the signal this workflow actor is currently waiting for, if any.
    pub waiting_signal: Option<String>,
    /// Signals that have been received by this workflow actor (name, payload).
    pub received_signals: Vec<(String, Option<String>)>,
    /// Read-only query handlers registered on a workflow actor, keyed by
    /// query name.  A handler is either a function/closure value invoked
    /// with the actor bound as `self`, or a plain value returned as-is.
    /// Handlers are ephemeral: they are not journaled and must be
    /// re-registered after a node restart.
    pub query_handlers: HashMap<String, Value>,
    /// True if this actor was generated from an `agent` declaration.
    pub is_agent: bool,
    /// Spawn-time capability manifest (canonical tokens such as
    /// `Net::TcpOut(host:port)`), installed by `spawn Foo() with [...]`.
    /// Network host functions reject destinations not present in this set;
    /// empty means no outbound network grants (ungranted-by-default).
    pub capabilities: std::collections::BTreeSet<String>,
    /// Execution backend for this actor.
    pub backend: ActorBackend,
    /// True while a background worker thread holds an in-flight LLM request
    /// issued by this actor's suspended bytecode behavior.
    #[cfg(feature = "ai-runtime")]
    pub llm_inflight: bool,
    /// Prompt of the in-flight LLM request, if any (kept for resume
    /// bookkeeping; cleared when the completion is pumped).
    #[cfg(feature = "ai-runtime")]
    pub llm_pending_prompt: Option<String>,
    /// Completed background LLM result waiting to be consumed when the
    /// suspended behavior re-executes its `LlmAsk` instruction.
    #[cfg(feature = "ai-runtime")]
    pub llm_completed: Option<Result<nulang_ai::LlmResponse, nulang_ai::LlmError>>,
    /// State of an in-flight timed selective receive (`receive ... after
    /// ms =>`), from the first suspension until the wait resolves (match,
    /// timeout, or the behavior ends). `None` when no receive-wait is live.
    pub receive_wait: Option<ReceiveWaitState>,
    /// True when a TimerSleepWake has fired for this actor.  The re-executed
    /// PerformAsync checks this flag and returns Ready to complete the sleep.
    pub timer_sleep_fired: bool,
    /// Cached parsed retry configuration for agent actors.
    pub retry_config: Option<crate::ast::AgentRetryConfig>,
    /// Cached parsed fallback configuration for agent actors.
    pub fallback_config: Vec<crate::ast::AgentFallbackEntry>,
    /// Flight recorder ring buffer for deterministic replay debugging.
    /// Records the N most recent messages delivered to this actor.
    pub flight_recorder: FlightRecorder,
    /// Hibernation state: None = active, Some = hibernated with serialized bytes.
    pub hibernation_state: Option<HibernationState>,
    /// Time (in milliseconds) since last activity. Used for hibernation timeout.
    pub idle_ms: u64,
    /// If true, the scheduler-driven dehydration scanner never hibernates this
    /// actor. Used to keep a grain resident while it is actively needed.
    pub pinned: bool,
    /// Fields modified since the last checkpoint (incremental persistence).
    /// Cleared after each successful snapshot. Empty on a freshly spawned
    /// actor (all fields are serialized on the first checkpoint).
    pub dirty_fields: HashSet<String>,
    /// Object-store ids held by this actor.  Populated when a message carrying
    /// an object ref is delivered.  Dropped on actor exit.
    pub held_objects: HashSet<ObjectId>,
}

/// State of an actor's in-flight timed selective receive.
///
/// The timeout timer is armed exactly once per wait (at the first
/// suspension), so a wake-then-re-suspend cycle (a non-matching message
/// arrived) keeps the original deadline instead of restarting the clock.
/// `timed_out` is set by the timer-fire path; the re-executed `ReceiveWait`
/// consumes it and resolves the wait with the no-match sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveWaitState {
    /// The armed timeout timer (cancelled when the wait resolves by match).
    pub timer_id: TimerId,
    /// True once the timeout timer has fired.
    pub timed_out: bool,
}

/// Captured VM state plus metadata for resuming a workflow step.
#[derive(Debug)]
pub struct SuspendedExecution {
    pub vm_state: crate::vm::SuspendedVmState,
    pub behavior_idx: usize,
    pub step_name: String,
}

/// A behavior entry: maps behavior name to handler.
pub struct BehaviorEntry {
    pub name: String,
    pub handler_fn: fn(&mut Actor, &[Value]),
}

impl Actor {
    pub fn new(id: u64, name: impl Into<String>, mailbox_cap: usize) -> Self {
        Actor {
            id,
            name: name.into(),
            state: ActorState::Created,
            mailbox: Mailbox::new(mailbox_cap),
            heap: {
                let mut heap = ActorHeap::new(64 * 1024); // 64KB initial heap
                heap.set_actor_id(id);
                heap
            },
            orca_gc: OrcaGc::new(id), // ORCA GC engine
            iso_arena: crate::iso_arena::IsoArena::new(),
            state_data: HashMap::new(),
            state_models: HashMap::new(),
            event_log: Vec::new(),
            event_sourced_sequences: HashMap::new(),
            event_sourced_compaction_interval: 100,
            persistent: false,
            is_workflow: false,
            behavior_table: Vec::new(),
            aot_targets: Vec::new(),
            bytecode_offsets: Vec::new(),
            compensation_offsets: Vec::new(),
            compensated_steps: Vec::new(),
            bytecode_module: None,
            bytecode_module_idx: None,
            parent: None,
            children: Vec::new(),
            monitors: Vec::new(),
            links: Vec::new(),
            trap_exits: false,
            priority: ActorPriority::Normal,
            jit_safepoint_counter: crate::jit::runtime::JIT_SAFEPOINT_BUDGET,
            jit_yield_pending: false,
            reduction_count: 0,
            turn_reductions: 0,
            max_reductions: 1000,
            sequence: 0,
            cycle_sentinel: None,
            suspended_execution: None,
            waiting_signal: None,
            received_signals: Vec::new(),
            query_handlers: HashMap::new(),
            is_agent: false,
            capabilities: std::collections::BTreeSet::new(),
            backend: ActorBackend::default(),
            #[cfg(feature = "ai-runtime")]
            llm_inflight: false,
            #[cfg(feature = "ai-runtime")]
            llm_pending_prompt: None,
            dirty_fields: HashSet::new(),
            #[cfg(feature = "ai-runtime")]
            llm_completed: None,
            receive_wait: None,
            timer_sleep_fired: false,
            retry_config: None,
            flight_recorder: FlightRecorder::new(1000),
            fallback_config: Vec::new(),
            hibernation_state: None,
            idle_ms: 0,
            pinned: false,
            held_objects: HashSet::new(),
        }
    }

    /// Hibernate this actor: serialize its state and VM continuation,
    /// store in hibernation_state, and return the serialized bytes.
    pub fn hibernate(
        &mut self,
        vm: &mut crate::vm::VM,
        module_hash: &[u8; 32],
    ) -> Result<Vec<u8>, String> {
        if self.hibernation_state.is_some() {
            return Err("Actor already hibernated".to_string());
        }
        // Capture current VM continuation
        let cont = crate::vm::Continuation::capture(vm, 0).ok_or("No active frame")?;
        let bytes = crate::runtime::heap_serialize::serialize_continuation(
            &cont,
            &vm.handler_stack,
            vm,
            module_hash,
        )?;
        self.hibernation_state = Some(HibernationState {
            continuation_bytes: bytes.clone(),
            module_hash: *module_hash,
            hibernated_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            state_fields: self.state_data.clone(),
        });
        Ok(bytes)
    }

    /// Wake this actor from hibernation: deserialize and restore VM state.
    ///
    /// If the hibernation was recorded without an active continuation (e.g. an
    /// idle grain that had no in-flight behavior), waking simply clears the
    /// hibernation marker so the next message starts a fresh behavior.
    pub fn wake_from_hibernation(&mut self, vm: &mut crate::vm::VM) -> Result<(), String> {
        let hibernation = self.hibernation_state.take().ok_or("Not hibernated")?;
        if hibernation.continuation_bytes.is_empty() {
            return Ok(());
        }
        let (cont, handlers) = crate::runtime::heap_serialize::deserialize_continuation(
            &hibernation.continuation_bytes,
            vm,
        )?;
        // Restore VM state
        cont.restore(vm, crate::vm::Value::unit());
        // Restore handler stack
        vm.handler_stack = handlers;
        Ok(())
    }

    /// Check if this actor is currently hibernated.
    pub fn is_hibernated(&self) -> bool {
        self.hibernation_state.is_some()
    }

    /// Increment idle time; returns true if hibernation threshold exceeded.
    pub fn increment_idle(&mut self, ms: u64) -> bool {
        self.idle_ms += ms;
        self.idle_ms >= 30000 // 30 second default threshold
    }

    /// Reset idle timer on activity.
    pub fn reset_idle(&mut self) {
        self.idle_ms = 0;
    }

    /// Pin the actor so the scheduler-driven dehydration scanner never
    /// hibernates it.
    pub fn pin(&mut self) {
        self.pinned = true;
    }

    /// Unpin the actor, allowing dehydration again.
    pub fn unpin(&mut self) {
        self.pinned = false;
    }

    /// True if the actor currently has in-flight execution that must not be
    /// interrupted by dehydration (a suspended behavior, an armed receive-wait,
    /// a JIT-yield suspension, or an in-flight LLM call).
    pub fn is_mid_execution(&self) -> bool {
        if self.suspended_execution.is_some()
            || self.receive_wait.is_some()
            || self.jit_yield_pending
        {
            return true;
        }
        #[cfg(feature = "ai-runtime")]
        {
            self.llm_inflight
        }
        #[cfg(not(feature = "ai-runtime"))]
        {
            false
        }
    }

    /// Return the cycle-detector sentinel header for this actor.
    ///
    /// The sentinel is lazily allocated on the actor's heap and pinned
    /// (sticky) so it is never collected. It represents the actor itself as
    /// a holder of foreign references for coarse-grained cycle detection.
    pub fn cycle_sentinel(&mut self) -> Option<*mut OrcaHeader> {
        if self.cycle_sentinel.is_none() {
            if let Some(ptr) = self.heap.alloc(8, TypeTag::Raw) {
                let header = unsafe { ActorHeap::header_of(ptr) };
                unsafe {
                    // SAFETY: fresh allocation on this actor's heap; the
                    // single scheduler thread is the only mutator.
                    (*header).sticky = true;
                }
                self.cycle_sentinel = Some(header);
            }
        }
        self.cycle_sentinel
    }

    /// Pop a message from the mailbox.
    pub fn receive(&mut self) -> Option<Message> {
        self.mailbox.pop()
    }

    /// Push a message into the mailbox.
    pub fn send(&mut self, msg: Message) -> Result<(), Message> {
        self.mailbox.push_local(msg)
    }

    /// Set or update a named state field.  Marks the field dirty for
    /// incremental persistence (only dirty fields are re-serialized on
    /// the next checkpoint).
    pub fn set_state_field(&mut self, name: impl Into<String>, value: Value) {
        let name_str = name.into();
        self.dirty_fields.insert(name_str.clone());
        self.state_data.insert(name_str.clone(), value);

        // Auto-sync CRDT fields on mutation
        if let Some(StateModel::Crdt(_crdt_type)) = self.state_models.get(&name_str) {
            self.dirty_fields.insert(name_str.clone());
        }
    }

    /// Get a named state field.
    pub fn get_state_field(&self, name: &str) -> Option<Value> {
        self.state_data.get(name).copied()
    }

    /// Check if the actor has exceeded its per-turn reduction quota and should yield.
    pub fn should_yield(&self) -> bool {
        self.turn_reductions >= self.max_reductions
    }

    /// Register a named behavior handler.
    ///
    /// The behavior name is used to route messages to the correct handler.
    /// The handler function receives a mutable reference to the actor and
    /// the message payload.
    pub fn register_behavior(
        &mut self,
        name: impl Into<String>,
        handler: fn(&mut Actor, &[Value]),
    ) {
        self.behavior_table.push(BehaviorEntry {
            name: name.into(),
            handler_fn: handler,
        });
    }

    /// Reset the per-turn reduction budget (called after yielding or when the
    /// actor goes waiting). Does not touch the monotonic `reduction_count`.
    pub fn reset_reductions(&mut self) {
        self.turn_reductions = 0;
    }

    /// Increment the reduction count: both the monotonic lifetime metric and
    /// the per-turn budget counter.
    pub fn increment_reductions(&mut self, count: u32) {
        self.reduction_count += count;
        self.turn_reductions += count;
    }

    /// Allocate a null-terminated string on the actor heap and return a pointer
    /// value. Returns nil if allocation fails.
    pub fn allocate_string(&mut self, s: &str) -> Value {
        let bytes = s.as_bytes();
        match self.heap.alloc(bytes.len() + 1, TypeTag::String) {
            Some(ptr) => {
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                    *ptr.add(bytes.len()) = 0;
                }
                Value::ptr(ptr)
            }
            None => Value::nil(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_actor_new() {
        let actor = Actor::new(1, "test", 0);
        assert_eq!(actor.id, 1);
        assert_eq!(actor.name, "test");
        assert_eq!(actor.state, ActorState::Created);
        assert!(!actor.persistent);
        assert!(!actor.is_workflow);
        assert!(!actor.is_agent);
        assert_eq!(actor.max_reductions, 1000);
        assert_eq!(actor.reduction_count, 0);
    }

    #[test]
    fn test_actor_set_and_get_state_field() {
        let mut actor = Actor::new(1, "test", 0);
        actor.set_state_field("key", Value::int(42));
        assert_eq!(actor.get_state_field("key"), Some(Value::int(42)));
    }

    #[test]
    fn test_actor_get_state_field_missing() {
        let actor = Actor::new(1, "test", 0);
        assert_eq!(actor.get_state_field("missing"), None);
    }

    #[test]
    fn test_actor_set_state_field_updates() {
        let mut actor = Actor::new(1, "test", 0);
        actor.set_state_field("key", Value::int(1));
        actor.set_state_field("key", Value::int(2));
        assert_eq!(actor.get_state_field("key"), Some(Value::int(2)));
    }

    #[test]
    fn test_actor_should_yield_false() {
        let actor = Actor::new(1, "test", 0);
        assert!(!actor.should_yield());
    }

    #[test]
    fn test_actor_should_yield_true() {
        let mut actor = Actor::new(1, "test", 0);
        actor.increment_reductions(1000);
        assert!(actor.should_yield());
    }

    #[test]
    fn test_actor_reset_reductions() {
        let mut actor = Actor::new(1, "test", 0);
        actor.increment_reductions(500);
        assert_eq!(actor.reduction_count, 500);
        actor.reset_reductions();
        // The monotonic lifetime count survives the reset; only the per-turn
        // budget is cleared.
        assert_eq!(actor.reduction_count, 500);
        assert!(!actor.should_yield());
    }

    #[test]
    fn test_actor_register_behavior() {
        let mut actor = Actor::new(1, "test", 0);
        fn handler(_actor: &mut Actor, _args: &[Value]) {}
        actor.register_behavior("my_handler", handler);
        assert_eq!(actor.behavior_table.len(), 1);
        assert_eq!(actor.behavior_table[0].name, "my_handler");
    }

    #[test]
    fn test_actor_allocate_string() {
        let mut actor = Actor::new(1, "test", 0);
        let val = actor.allocate_string("hello");
        assert!(!val.is_nil(), "allocation should return a non-nil value");
    }

    #[test]
    fn test_actor_send_receive() {
        let mut actor = Actor::new(1, "test", 0);
        let msg = Message {
            behavior_id: 1,
            payload: Arc::new(vec![Value::int(42)]),
            sender: 99,
            priority: MessagePriority::Normal,
            trace_id: None,
        };
        assert!(actor.send(msg.clone()).is_ok());
        let received = actor.receive().expect("should receive a message");
        assert_eq!(received.behavior_id, 1);
        assert_eq!(received.sender, 99);
        assert_eq!(*received.payload, vec![Value::int(42)]);
    }
}
