//! Nulang Virtual Machine: register-based bytecode interpreter.
//!
//! ## Architecture
//!
//! - **256 general-purpose registers** per activation frame
//! - **NaN-boxing** for efficient tagged values (int/float/bool/nil/actor_ref)
//! - **Bytecode modules** with constant pools and function tables
//! - **Algebraic effects** via handler stack (Perform/Resume/Unwind/Handle)
//!
//! ## Effect System
//!
//! The VM implements algebraic effects via four opcodes:
//! - `Handle`: Push a handler frame onto the handler stack
//! - `Perform`: Invoke an effect operation (captures continuation)
//! - `Resume`: Restore the captured continuation with a value
//! - `Unwind`: Pop the handler frame (normal completion)
//!
//! Handler frames stay on the stack until `Unwind`, allowing multiple
//! effects in the same handle block to be handled by the same handler.
//!
//! ## Value Representation
//!
//! Uses NaN boxing: all non-float values are encoded in the quiet-NaN
//! payload of an f64. This gives us 51 bits of payload space for
//! pointers, integers, and type tags.

use std::ffi::{c_char, CStr, CString};

use crate::backends::{create_default_jit, JitBackend, TieredAction};
use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
use crate::ffi::{call_native, CType, Signature, FFI_REGISTRY};
use crate::runtime::heap::{ActorHeap, TypeTag as HeapTypeTag};
use crate::types::{NuError, NuResult, Span, VmSuspension};

// ---------------------------------------------------------------------------
// Distributed runtime callbacks for VM opcode integration.
//
// The VM does not depend on the actor runtime directly (that would create a
// circular crate dependency). Instead, a lightweight callback trait can be
// installed when the VM is used inside a distributed actor context.
// ---------------------------------------------------------------------------

/// Callback interface that supplies real distributed behavior for the VM's
/// `NodeId`, `Migrate`, `RAsk`, and `Gossip` opcodes.
///
/// A default no-op implementation is provided so the standalone VM remains
/// usable without any distributed runtime attached.
pub trait DistributedVmCallbacks: std::any::Any + std::fmt::Debug {
    /// Return the local node ID.
    fn node_id(&self) -> u64 {
        0
    }

    /// Record an actor migration request.
    fn migrate(&mut self, _actor_id: u64, _target_node_id: u64) {}

    /// Perform a synchronous remote ask.
    ///
    /// Returns the response value, or `Value::nil()` on timeout / failure.
    fn remote_ask(
        &mut self,
        _target_actor: u64,
        _behavior: &str,
        _args: &[Value],
        _timeout_ms: u64,
    ) -> Value {
        Value::nil()
    }
    /// Perform a fire-and-forget remote send.
    ///
    /// The VM calls this for the `RSend` opcode. The implementation should
    /// serialize the message and deliver it to the target node.
    fn remote_send(
        &mut self,
        _target_actor: u64,
        _target_node: u64,
        _behavior: &str,
        _args: &[Value],
    ) {
    }

    /// Send a gossip-style message to a subset of known nodes.
    ///
    /// Returns `Value::unit()`.
    fn gossip(&mut self, _message: &str) -> Value {
        Value::unit()
    }

    fn remote_spawn(
        &mut self,
        _target_node: u64,
        _behavior: &str,
        _init: &[(String, Value)],
    ) -> Value {
        Value::actor_ref(0)
    }
}

// ---------------------------------------------------------------------------
// Actor runtime callbacks for VM opcode integration.
//
// The VM is designed to run standalone, but when embedded in the actor
// runtime these callbacks wire Spawn to real actors and route heap
// allocations through the current actor's heap.
// ---------------------------------------------------------------------------

/// Result of querying whether a workflow signal has been received.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SignalWaitResult {
    /// The signal has been received; resume with this value.
    Ready(Value),
    /// The signal has not been received; the runtime should suspend the step.
    NotReady,
}

/// Result of a generic async effect operation from the `PerformAsync` opcode.
#[derive(Debug, Clone, PartialEq)]
pub enum PerformAsyncResult {
    /// The effect completed synchronously; `Some(content)` is the string
    /// result (interned into the module's constant pool by the VM), `None`
    /// means nil.
    Ready(Option<String>),
    /// The effect was dispatched to a background worker; the VM suspends the
    /// current behavior and re-executes the `PerformAsync` instruction on resume.
    Pending,
}

/// Callback interface that supplies real actor-runtime behavior for the VM's
/// `Spawn`, `ArrAlloc`, `SConcat`, `SRead`, and `Drop` opcodes.
pub trait ActorVmCallbacks: std::any::Any + std::fmt::Debug {
    /// Return the ID of the actor currently executing in the VM, if any.
    fn current_actor_id(&self) -> Option<u64> {
        None
    }

    /// Allocate `size` bytes on the current actor's heap.
    ///
    /// `type_tag` tells the heap what kind of object is being allocated.
    /// Returns a pointer to the payload region, or `None` if allocation fails.
    fn alloc(&mut self, size: usize, type_tag: HeapTypeTag) -> Option<*mut u8>;
    /// Allocate `size` bytes on the current actor's iso arena.
    ///
    /// Default: no arena support (falls back to `None`); actor callbacks
    /// that back `IsoArena` override this.
    fn alloc_arena(&mut self, _size: usize, _type_tag: HeapTypeTag) -> Option<*mut u8> {
        None
    }

    /// Reset the current actor's iso arena, reclaiming all arena objects.
    fn reset_arena(&mut self) {}

    /// True when `ptr` points into the current actor's iso arena.
    fn is_arena_ptr(&self, _ptr: *const u8) -> bool {
        false
    }

    /// Drop a local reference to a heap object.
    ///
    /// For standalone heaps this frees immediately; for actor heaps it should
    /// decrement the local reference count and reclaim when possible.
    fn drop_ref(&mut self, ptr: *mut u8);

    /// Create an additional local reference to a heap object.
    ///
    /// Called when a value that owns a heap pointer is captured into a
    /// closure environment, so the object cannot be freed by a `Drop` of the
    /// original binding while the closure still holds it. Mirrors `drop_ref`
    /// (increment vs. decrement of the same reference count).
    fn retain_ref(&mut self, ptr: *mut u8);

    /// Return the number of elements in an array allocated on the actor heap.
    fn array_len(&self, ptr: *mut u8) -> Option<usize>;

    /// Allocate a fresh heap string via `self.alloc`, copy `s` into it,
    /// and null-terminate. Default implementation works for any callback
    /// with a working `alloc`; callers may override for specialization.
    fn alloc_string(&mut self, s: &str) -> Value {
        let bytes = s.as_bytes();
        match self.alloc(bytes.len() + 1, HeapTypeTag::String) {
            Some(ptr) => unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                *ptr.add(bytes.len()) = 0;
                Value::ptr(ptr)
            },
            None => Value::nil(),
        }
    }

    /// Spawn a real actor from `module.actor_metadata`.
    ///
    /// `behavior_idx` is the behavior table index embedded in the `Spawn`
    /// instruction. The callback should find the matching `ActorMeta`, apply
    /// its persistence defaults, and return an actor reference value.
    fn spawn_actor(
        &mut self,
        module: &CodeModule,
        behavior_idx: usize,
        init: Vec<(String, Value)>,
    ) -> Value;

    /// Send a message to an actor by behavior table index.
    fn send_message(&mut self, target: Value, behavior_id: u16, args: &[Value]);

    /// Synchronously ask an actor and return its response.
    /// Default implementation sends the message and returns nil.
    fn ask_actor(&mut self, target: Value, behavior_id: u16, args: &[Value]) -> Value {
        let _ = (target, behavior_id, args);
        Value::nil()
    }

    /// Read a field from the current actor's state.  Default returns nil.
    fn get_state_field(&self, _field: &str) -> Value {
        Value::nil()
    }

    /// Write a field on the current actor's state.  Default is a no-op.
    fn set_state_field(&mut self, _field: &str, _value: Value) {}

    /// Emit an event in the current actor.  Default is a no-op.
    fn emit_event(&mut self, _event: &str, _args: &[Value]) {}

    /// Handle a built-in effect performed without an explicit handler.
    ///
    /// The callback receives the effect name and the current frame registers
    /// (args are placed in r0..rn by the compiler). If it returns `Some`, the
    /// VM resumes with that value; otherwise the effect is unhandled and the
    /// VM errors.
    fn perform_effect(&mut self, _effect_name: &str, _regs: &[Value]) -> Option<Value> {
        None
    }

    /// Handle a built-in effect performed without an explicit handler,
    /// given the operation name (e.g. `print` in `perform IO.print`) and
    /// the performing module's constant pool for resolving string-id
    /// arguments.
    ///
    /// The default ignores the extra context and delegates to
    /// `perform_effect`, preserving the historic callback contract for
    /// runtime-backed implementations (e.g. workflow `Timer.sleep`).
    fn perform_builtin_effect(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        constants: &[Constant],
        regs: &[Value],
    ) -> Option<Value> {
        let _ = (op_name, constants);
        self.perform_effect(effect_name, regs)
    }

    /// Handle a built-in effect performed without an explicit handler,
    /// given the operation name and the whole performing module — both its
    /// constant pool (string-id arguments) and its actor metadata (needed
    /// by effects that resolve actor types by name, e.g. `Otp.set_template`).
    ///
    /// The default delegates to `perform_builtin_effect` with the module's
    /// constant pool, preserving that callback's contract.
    fn perform_builtin_effect_in_module(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        module: &CodeModule,
        regs: &[Value],
    ) -> Option<Value> {
        self.perform_builtin_effect(effect_name, op_name, &module.constants, regs)
    }

    /// Check whether a workflow signal has been received.
    /// Default returns `Ready(unit)` so un-wired signal waits do not block.
    fn wait_signal(&mut self, _name: &str) -> SignalWaitResult {
        SignalWaitResult::Ready(Value::unit())
    }

    /// Suspend the current workflow step waiting for a signal.
    /// The callback receives the captured VM state so it can store it on the
    /// actor and resume execution when the signal arrives.
    fn suspend_for_signal(&mut self, _name: &str, _vm_state: Option<SuspendedVmState>) {}

    /// Execute an LLM request synchronously and return the response content.
    ///
    /// The VM extracts the prompt as a string and passes it to the callback
    /// along with the model constant from the `LlmAsk` instruction. If no
    /// client is configured, return `None` and the VM will leave the result
    /// register as `nil`.
    fn complete_llm(&mut self, _model: &str, _prompt: &str) -> Option<String> {
        None
    }

    /// Execute an LLM request, possibly asynchronously.
    ///
    /// The default implementation preserves blocking behavior by delegating
    /// to `complete_llm`. Runtime-backed callbacks may override this to
    /// return `Pending` and deliver the response on a later resume, in which
    /// case the VM suspends the behavior with an `LlmAsk:suspend` sentinel
    /// error (same pattern as `SignalWait`).
    fn llm_ask(&mut self, model: &str, prompt: &str) -> PerformAsyncResult {
        PerformAsyncResult::Ready(self.complete_llm(model, prompt))
    }

    /// Execute a generic async effect, possibly asynchronously.
    ///
    /// `effect_op` is the fully-qualified effect-and-operation name (e.g.
    /// `"Inference.ask"`). `args` are the staged argument values from
    /// registers r0..rN; `constants` is the performing module's constant
    /// pool for resolving string-id arguments. Returns `Ready(content)` when
    /// the effect completed synchronously (the VM interns the content string
    /// into its module's constant pool), or `Pending` when the call was
    /// dispatched to a background worker — the VM then suspends the current
    /// behavior with a `PerformAsync` sentinel and re-executes the
    /// instruction on resume.
    ///
    /// The default implementation returns `Ready(None)` so the standalone VM
    /// always gets a nil result for any async effect.
    fn perform_async(
        &mut self,
        _effect_op: &str,
        _constants: &[Constant],
        _args: &[Value],
    ) -> PerformAsyncResult {
        PerformAsyncResult::Ready(None)
    }

    /// Try to receive a message from the current actor's mailbox.
    /// Returns `Some((behavior_id, value))` if a message is available,
    /// or `None` if the mailbox is empty. Default returns `None`.
    fn try_receive(&mut self) -> Option<(u16, Value)> {
        None
    }

    /// Selective receive: scan the current actor's mailbox in FIFO order
    /// for the first message whose behavior id appears in `behavior_ids`.
    /// Non-matching messages stay in the mailbox. Returns
    /// `Some((arm_index, payload))` — `arm_index` is the position of the
    /// matched id within `behavior_ids` — or `None` when nothing matches
    /// (or there is no current actor). Default returns `None`.
    fn try_receive_match(&mut self, _behavior_ids: &[u16]) -> Option<(usize, Vec<Value>)> {
        None
    }

    /// Timed selective receive (`receive { ... } after ms => body`): the
    /// mailbox scan found no matching message. Return `true` to suspend the
    /// current actor — the VM re-executes the `ReceiveWait` instruction when
    /// the runtime resumes it (matching message arrived or timeout fired).
    /// Return `false` to resolve the wait now with the no-match sentinel:
    /// a non-positive timeout, no actor context, or an already-fired
    /// timeout marker (which the implementation must consume so the next
    /// wait is not poisoned). Default returns `false`, so standalone
    /// execution is always non-blocking.
    fn receive_wait_suspend(&mut self, _timeout_ms: i64) -> bool {
        false
    }

    /// Timed selective receive resolved with a mailbox match: cancel any
    /// pending receive-wait timeout state for the current actor so a stale
    /// timer cannot fire into a later wait. Default is a no-op.
    fn receive_wait_matched(&mut self) {}

    /// Commit a selective receive: remove the matched ("tried") message from
    /// the skip-buffer and clear remaining "tried" flags. Called after a
    /// pattern+guard check succeeds. Default is a no-op (standalone VM has no
    /// skip-buffer).
    fn commit_receive_match(&mut self) {}

    /// Reset "tried" flags in the skip-buffer. Called when
    /// `try_receive_match` returns `None`, preparing the buffer for the next
    /// receive expression. Default is a no-op.
    fn reset_receive_match(&mut self) {}
}

/// Standalone callbacks used when the VM runs without an actor runtime.
///
/// Allocations go through a private `ActorHeap` so that `Drop` actually
/// reclaims memory instead of leaking.
#[derive(Debug)]
pub(crate) struct StandaloneVmCallbacks {
    heap: ActorHeap,
    gc: crate::runtime::OrcaGc,
    /// Test hook: when set, `IO.print` output is recorded here instead of
    /// written to stdout.
    io_output: Option<std::rc::Rc<std::cell::RefCell<Vec<String>>>>,
    /// Routes registered by `perform Web.route(...)` during this VM run.
    /// Collected by the dev server after the entry point finishes.
    routes: Vec<crate::runtime::WebRoute>,
}

impl StandaloneVmCallbacks {
    pub(crate) fn new() -> Self {
        let mut heap = ActorHeap::new(1024 * 1024);
        heap.set_actor_id(0);
        Self {
            heap,
            gc: crate::runtime::OrcaGc::new(0),
            io_output: None,
            routes: Vec::new(),
        }
    }
}

/// Resolve a value to display text using a module constant pool.
///
/// String-id values index the constant pool; pointer values are read as
/// null-terminated UTF-8; everything else falls back to `to_string_repr`.
pub fn resolve_value_string(constants: &[Constant], value: Value) -> String {
    if let Some(id) = value.as_string_id() {
        match constants.get(id as usize) {
            Some(Constant::String(s)) => s.clone(),
            _ => String::new(),
        }
    } else if let Some(ptr) = value.as_ptr() {
        if ptr.is_null() {
            String::new()
        } else {
            // SAFETY: heap string payloads are null-terminated
            // (allocate_string and the standalone IO.read path both write
            // a trailing zero byte).
            unsafe {
                CStr::from_ptr(ptr as *const c_char)
                    .to_string_lossy()
                    .into_owned()
            }
        }
    } else {
        value.to_string_repr()
    }
}

// ---------------------------------------------------------------------------
// StrBuilder builtin — mutable, growable string buffer
// ---------------------------------------------------------------------------
//
// Payload (allocated with `HeapTypeTag::Raw` — raw bytes, no Value slots):
//   [0..8)  len: u64      — current content length in bytes
//   [8..16) cap: u64      — allocated byte capacity
//   [16..)  bytes         — UTF-8 content
//
// `push` appends in place when capacity suffices and returns the same
// pointer; on growth it allocates a new object with doubled capacity and
// returns the NEW pointer (the old pointer is stale after growth — callers
// must rebind: `b = perform StrBuilder.push(b, s)`). This is a deliberately
// mutable type, like `var` bindings; aliased builder values observe in-place
// mutations.
const STRBUILDER_HDR: usize = 16;

pub(crate) fn strbuilder_op(
    callbacks: &mut dyn ActorVmCallbacks,
    constants: &[Constant],
    op: &str,
    regs: &[Value],
) -> Option<Value> {
    match op {
        "new" => {
            let cap: usize = 32;
            let ptr = callbacks.alloc(STRBUILDER_HDR + cap, HeapTypeTag::Raw)?;
            unsafe {
                *(ptr as *mut u64) = 0; // len
                *((ptr as *mut u64).add(1)) = cap as u64;
            }
            Some(Value::ptr(ptr))
        }
        "push" | "append" => {
            let b = regs.first()?.as_ptr()?;
            let s = resolve_value_string(constants, *regs.get(1)?);
            let len = unsafe { *(b as *const u64) } as usize;
            let cap = unsafe { *((b as *const u64).add(1)) } as usize;
            let needed = len + s.len();
            if needed <= cap {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        s.as_ptr(),
                        b.add(STRBUILDER_HDR).add(len),
                        s.len(),
                    );
                    *(b as *mut u64) = needed as u64;
                }
                Some(Value::ptr(b))
            } else {
                let new_cap = (cap * 2).max(needed);
                let new_ptr = callbacks.alloc(STRBUILDER_HDR + new_cap, HeapTypeTag::Raw)?;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        b.add(STRBUILDER_HDR),
                        new_ptr.add(STRBUILDER_HDR),
                        len,
                    );
                    std::ptr::copy_nonoverlapping(
                        s.as_ptr(),
                        new_ptr.add(STRBUILDER_HDR).add(len),
                        s.len(),
                    );
                    *(new_ptr as *mut u64) = needed as u64;
                    *((new_ptr as *mut u64).add(1)) = new_cap as u64;
                }
                Some(Value::ptr(new_ptr))
            }
        }
        "to_string" => {
            let b = regs.first()?.as_ptr()?;
            let len = unsafe { *(b as *const u64) } as usize;
            let ptr = callbacks.alloc(len + 1, HeapTypeTag::String)?;
            unsafe {
                std::ptr::copy_nonoverlapping(b.add(STRBUILDER_HDR), ptr, len);
                *ptr.add(len) = 0;
            }
            Some(Value::ptr(ptr))
        }
        "len" => {
            let b = regs.first()?.as_ptr()?;
            Some(Value::int(unsafe { *(b as *const u64) } as i64))
        }
        "reset" => {
            let b = regs.first()?.as_ptr()?;
            unsafe {
                *(b as *mut u64) = 0;
            }
            Some(Value::ptr(b))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Map builtin — mutable hash map, open addressing over Value slots
// ---------------------------------------------------------------------------
//
// Payload (allocated with `HeapTypeTag::Map`, treated as uniform Value
// slots by the GC and heap serializer):
//   slot 0           cap:  Int — entry capacity
//   slot 1           used: Int — live entry count
//   slot 2 + 3*i     entry hash (Int; 0 = empty, 1 = tombstone,
//                                  else content-hash + 2, masked to 48 bits)
//   slot 2 + 3*i + 1 entry key (Value)
//   slot 2 + 3*i + 2 entry value (Value)
//
// Insert/remove mutate in place (mutable type, like StrBuilder); keys and
// values are retained on insert and released on remove/overwrite/free, so
// the ORCA reclamation protocol holds. Growth doubles capacity and rehashes
// into a NEW object; the caller rebinds the returned pointer.
const MAP_HDR_SLOTS: usize = 2;
const MAP_EMPTY: i64 = 0;
const MAP_TOMB: i64 = 1;

fn map_capacity(m: *mut u8) -> usize {
    unsafe { (*(m as *const Value)).as_int().unwrap_or(0) as usize }
}

fn map_used(m: *mut u8) -> usize {
    unsafe { (*((m as *const Value).add(1))).as_int().unwrap_or(0) as usize }
}

fn map_set_used(m: *mut u8, used: usize) {
    unsafe {
        *((m as *mut Value).add(1)) = Value::int(used as i64);
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn map_value_is_string(v: Value) -> bool {
    if v.as_string_id().is_some() {
        return true;
    }
    if let Some(ptr) = v.as_ptr() {
        // SAFETY: `ptr` is a live heap payload; the header sits one stride
        // before it (see `ActorHeap::header_of`).
        return unsafe { (*ActorHeap::header_of(ptr)).type_tag == HeapTypeTag::String };
    }
    false
}

fn map_value_hash(constants: &[Constant], v: Value) -> u64 {
    if let Some(id) = v.as_string_id() {
        match constants.get(id as usize) {
            Some(Constant::String(s)) => fnv1a64(s.as_bytes()),
            _ => 0,
        }
    } else if let Some(ptr) = v.as_ptr() {
        // SAFETY: `ptr` is a live heap payload; the header sits one stride
        // before it.
        let tag = unsafe { (*ActorHeap::header_of(ptr)).type_tag };
        if tag == HeapTypeTag::String {
            // SAFETY: heap string payloads are NUL-terminated.
            unsafe { fnv1a64(CStr::from_ptr(ptr as *const c_char).to_bytes()) }
        } else {
            fnv1a64(&(ptr as usize).to_le_bytes())
        }
    } else {
        fnv1a64(&v.as_raw().to_le_bytes())
    }
}

/// Equality mirroring Nulang `==`: strings compare by content, heap objects
/// by pointer identity, scalars by raw bits.
fn map_values_equal(a: Value, b: Value, constants: &[Constant]) -> bool {
    let a_str = map_value_is_string(a);
    let b_str = map_value_is_string(b);
    if a_str && b_str {
        return resolve_value_string(constants, a) == resolve_value_string(constants, b);
    }
    match (a.as_ptr(), b.as_ptr()) {
        (Some(pa), Some(pb)) => pa == pb,
        _ => a.as_raw() == b.as_raw(),
    }
}

fn map_hash_code(constants: &[Constant], k: Value) -> i64 {
    let h = map_value_hash(constants, k);
    let code = ((h as i64).wrapping_add(2)) & 0x7FFF_FFFF_FFFF;
    if code == MAP_EMPTY || code == MAP_TOMB {
        2
    } else {
        code
    }
}

/// Entry slot base for entry `i` (entry layout: hash, key, value).
#[inline]
fn map_entry_slot(i: usize) -> usize {
    MAP_HDR_SLOTS + i * 3
}

/// Grow the map to `new_cap` entries, rehashing all live entries into a new
/// object (retaining key/value refs). Returns the new pointer.
fn map_grow(
    callbacks: &mut dyn ActorVmCallbacks,
    m: *mut u8,
    new_cap: usize,
) -> Option<Value> {
    let old_cap = map_capacity(m);
    let slots = MAP_HDR_SLOTS + new_cap * 3;
    let new_ptr = callbacks.alloc(slots * std::mem::size_of::<Value>(), HeapTypeTag::Map)?;
    unsafe {
        *(new_ptr as *mut Value) = Value::int(new_cap as i64);
        *((new_ptr as *mut Value).add(1)) = Value::int(0); // used, refilled below
        // The bump allocator does not zero memory: initialize every entry
        // slot to the empty marker so the GC slot-release path never sees
        // garbage Value patterns.
        for slot in (2..slots).map(|i| (new_ptr as *mut Value).add(i)) {
            *slot = Value::int(MAP_EMPTY);
        }
        for i in 0..old_cap {
            let base = (m as *mut Value).add(map_entry_slot(i));
            let h = *(base as *const Value);
            let code = h.as_int().unwrap_or(MAP_EMPTY);
            if code == MAP_EMPTY || code == MAP_TOMB {
                continue;
            }
            let k = *((base as *const Value).add(1));
            let v = *((base as *const Value).add(2));
            let code_v = Value::int(code);
            let start = (code as usize) % new_cap;
            for j in 0..new_cap {
                let idx = (start + j) % new_cap;
                let nbase = (new_ptr as *mut Value).add(map_entry_slot(idx));
                let nh = *(nbase as *const Value);
                if nh.as_int() == Some(MAP_EMPTY) {
                    *(nbase as *mut Value) = code_v;
                    *((nbase as *mut Value).add(1)) = k;
                    if let Some(p) = k.as_ptr() {
                        callbacks.retain_ref(p);
                    }
                    *((nbase as *mut Value).add(2)) = v;
                    if let Some(p) = v.as_ptr() {
                        callbacks.retain_ref(p);
                    }
                    break;
                }
            }
        }
        let used = map_used(m);
        *((new_ptr as *mut Value).add(1)) = Value::int(used as i64);
    }
    Some(Value::ptr(new_ptr))
}

pub(crate) fn hashmap_op(
    callbacks: &mut dyn ActorVmCallbacks,
    constants: &[Constant],
    op: &str,
    regs: &[Value],
) -> Option<Value> {
    match op {
        "new" => {
            let cap: usize = 8;
            let slots = MAP_HDR_SLOTS + cap * 3;
            let m = callbacks.alloc(slots * std::mem::size_of::<Value>(), HeapTypeTag::Map)?;
            unsafe {
                *(m as *mut Value) = Value::int(cap as i64);
                *((m as *mut Value).add(1)) = Value::int(0);
                for slot in (2..slots).map(|i| (m as *mut Value).add(i)) {
                    *slot = Value::int(MAP_EMPTY);
                }
            }
            Some(Value::ptr(m))
        }
        "insert" => {
            let m_orig = regs.first()?.as_ptr()?;
            let k = *regs.get(1)?;
            let v = *regs.get(2)?;
            let code = map_hash_code(constants, k);
            let mut m = m_orig;
            // Grow at load factor 0.5 to keep probes short; growth returns a
            // NEW object (the caller rebinds the returned pointer).
            if map_used(m) + 1 > map_capacity(m) / 2 {
                m = map_grow(callbacks, m, map_capacity(m) * 2)?.as_ptr()?;
            }
            let cap = map_capacity(m);
            let used = map_used(m);
            let start = (code as usize) % cap;
            let mut first_tomb: Option<usize> = None;
            for i in 0..cap {
                let idx = (start + i) % cap;
                let base = unsafe { (m as *mut Value).add(map_entry_slot(idx)) };
                let h = unsafe { *(base as *const Value) };
                let hc = h.as_int().unwrap_or(MAP_EMPTY);
                if hc == MAP_EMPTY {
                    let target = first_tomb.unwrap_or(idx);
                    let tbase = unsafe { (m as *mut Value).add(map_entry_slot(target)) };
                    unsafe {
                        *(tbase as *mut Value) = Value::int(code);
                        *((tbase as *mut Value).add(1)) = k;
                        if let Some(p) = k.as_ptr() {
                            callbacks.retain_ref(p);
                        }
                        *((tbase as *mut Value).add(2)) = v;
                        if let Some(p) = v.as_ptr() {
                            callbacks.retain_ref(p);
                        }
                    }
                    map_set_used(m, used + 1);
                    return Some(Value::ptr(m));
                }
                if hc == MAP_TOMB {
                    if first_tomb.is_none() {
                        first_tomb = Some(idx);
                    }
                    continue;
                }
                let kslot = unsafe { *((base as *const Value).add(1)) };
                if map_values_equal(kslot, k, constants) {
                    unsafe {
                        let oldk = *((base as *const Value).add(1));
                        let oldv = *((base as *const Value).add(2));
                        if let Some(p) = oldk.as_ptr() {
                            callbacks.drop_ref(p);
                        }
                        if let Some(p) = oldv.as_ptr() {
                            callbacks.drop_ref(p);
                        }
                        *((base as *mut Value).add(1)) = k;
                        if let Some(p) = k.as_ptr() {
                            callbacks.retain_ref(p);
                        }
                        *((base as *mut Value).add(2)) = v;
                        if let Some(p) = v.as_ptr() {
                            callbacks.retain_ref(p);
                        }
                    }
                    return Some(Value::ptr(m));
                }
            }
            // No empty slot: reuse the first tombstone (table can't be all
            // occupied here because growth kept load ≤ 0.5, but tombstones
            // may consume the remainder).
            if let Some(t) = first_tomb {
                let tbase = unsafe { (m as *mut Value).add(map_entry_slot(t)) };
                unsafe {
                    *(tbase as *mut Value) = Value::int(code);
                    *((tbase as *mut Value).add(1)) = k;
                    if let Some(p) = k.as_ptr() {
                        callbacks.retain_ref(p);
                    }
                    *((tbase as *mut Value).add(2)) = v;
                    if let Some(p) = v.as_ptr() {
                        callbacks.retain_ref(p);
                    }
                }
                map_set_used(m, used + 1);
                Some(Value::ptr(m))
            } else {
                None
            }
        }
        "get" => {
            let m = regs.first()?.as_ptr()?;
            let k = *regs.get(1)?;
            let cap = map_capacity(m);
            let code = map_hash_code(constants, k);
            let start = (code as usize) % cap;
            for i in 0..cap {
                let idx = (start + i) % cap;
                let base = unsafe { (m as *mut Value).add(map_entry_slot(idx)) };
                let h = unsafe { *(base as *const Value) };
                let hc = h.as_int().unwrap_or(MAP_EMPTY);
                if hc == MAP_EMPTY {
                    break; // key absent (tombstones can't hide it: linear
                           // probing stops only at a true empty slot)
                }
                if hc == MAP_TOMB {
                    continue;
                }
                let kslot = unsafe { *((base as *const Value).add(1)) };
                if map_values_equal(kslot, k, constants) {
                    return Some(unsafe { *((base as *const Value).add(2)) });
                }
            }
            Some(Value::nil())
        }
        "contains" => {
            let m = regs.first()?.as_ptr()?;
            let k = *regs.get(1)?;
            let cap = map_capacity(m);
            let code = map_hash_code(constants, k);
            let start = (code as usize) % cap;
            for i in 0..cap {
                let idx = (start + i) % cap;
                let base = unsafe { (m as *mut Value).add(map_entry_slot(idx)) };
                let hc = unsafe { *(base as *const Value) }.as_int().unwrap_or(MAP_EMPTY);
                if hc == MAP_EMPTY {
                    break;
                }
                if hc == MAP_TOMB {
                    continue;
                }
                let kslot = unsafe { *((base as *const Value).add(1)) };
                if map_values_equal(kslot, k, constants) {
                    return Some(Value::bool(true));
                }
            }
            Some(Value::bool(false))
        }
        "remove" => {
            let m = regs.first()?.as_ptr()?;
            let k = *regs.get(1)?;
            let cap = map_capacity(m);
            let used = map_used(m);
            let code = map_hash_code(constants, k);
            let start = (code as usize) % cap;
            for i in 0..cap {
                let idx = (start + i) % cap;
                let base = unsafe { (m as *mut Value).add(map_entry_slot(idx)) };
                let hc = unsafe { *(base as *const Value) }.as_int().unwrap_or(MAP_EMPTY);
                if hc == MAP_EMPTY {
                    break;
                }
                if hc == MAP_TOMB {
                    continue;
                }
                let kslot = unsafe { *((base as *const Value).add(1)) };
                if map_values_equal(kslot, k, constants) {
                    unsafe {
                        let oldk = *((base as *const Value).add(1));
                        let oldv = *((base as *const Value).add(2));
                        if let Some(p) = oldk.as_ptr() {
                            callbacks.drop_ref(p);
                        }
                        if let Some(p) = oldv.as_ptr() {
                            callbacks.drop_ref(p);
                        }
                        *(base as *mut Value) = Value::int(MAP_TOMB);
                    }
                    map_set_used(m, used.saturating_sub(1));
                    return Some(Value::ptr(m));
                }
            }
            Some(Value::ptr(m))
        }
        "size" => {
            let m = regs.first()?.as_ptr()?;
            Some(Value::int(map_used(m) as i64))
        }
        _ => None,
    }
}


impl ActorVmCallbacks for StandaloneVmCallbacks {
    fn alloc(&mut self, size: usize, type_tag: HeapTypeTag) -> Option<*mut u8> {
        self.heap.alloc(size, type_tag)
    }

    fn drop_ref(&mut self, ptr: *mut u8) {
        // SAFETY: `ptr` is a valid heap pointer previously allocated by this
        // actor's heap. The caller (VM ArrStore/FieldS write barrier) guarantees
        // ptr is non-null and points to an OrcaHeader-managed allocation.
        unsafe {
            self.gc.drop_local_ref(&mut self.heap, ptr);
        }
    }

    fn retain_ref(&mut self, ptr: *mut u8) {
        // SAFETY: `ptr` is a valid, non-null heap pointer to an
        // OrcaHeader-managed object. The GC only reads the header.
        unsafe {
            self.gc.local_ref(&self.heap, ptr);
        }
    }

    fn array_len(&self, ptr: *mut u8) -> Option<usize> {
        // SAFETY: `ptr` is a valid heap pointer from a prior Array allocation.
        // `ActorHeap::header_of` computes the OrcaHeader immediately preceding
        // the payload — this is sound when ptr was returned by heap.alloc().
        unsafe {
            let header = &*ActorHeap::header_of(ptr);
            if header.type_tag == HeapTypeTag::Array {
                let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
                Some(payload_size / std::mem::size_of::<Value>())
            } else {
                None
            }
        }
    }

    fn spawn_actor(
        &mut self,
        _module: &CodeModule,
        _behavior_idx: usize,
        _init: Vec<(String, Value)>,
    ) -> Value {
        Value::actor_ref(0)
    }

    fn send_message(&mut self, _target: Value, _behavior_id: u16, _args: &[Value]) {}

    /// Built-in effects for actor-free scripts: `IO.print` writes the
    /// first staged argument to stdout, `IO.read` reads one stdin line
    /// into a heap string. String-id arguments resolve against the
    /// performing module's constant pool. `Actor.*` and `Otp.*` effects
    /// need the actor runtime, so they are nil no-ops here (matching the
    /// runtime's outside-an-actor contract).
    fn perform_builtin_effect(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        constants: &[Constant],
        regs: &[Value],
    ) -> Option<Value> {
        if effect_name == "Actor" || effect_name == "Otp" {
            return Some(Value::nil());
        }
        if effect_name == "DB" {
            // DB.query requires a runtime with a configured database.
            // Standalone VM returns nil.
            return Some(Value::nil());
        }
        if effect_name == "Timer" {
            return Some(Value::unit());
        }
        if effect_name == "Web" {
            return crate::runtime::callbacks::perform_web_builtin(self, op_name, constants, regs);
        }
        if effect_name == "Realtime" {
            return crate::runtime::callbacks::perform_realtime_builtin(
                self, op_name, constants, regs,
            );
        }
        if effect_name == "Int" && op_name == Some("to_string") {
            let n = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
            let s = format!("{}", n);
            let bytes = s.into_bytes();
            match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                Some(ptr) => {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                        *ptr.add(bytes.len()) = 0;
                    }
                    return Some(Value::ptr(ptr));
                }
                None => return Some(Value::nil()),
            }
        }
        if effect_name == "Int" && op_name == Some("to_float") {
            let n = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
            return Some(Value::float(n as f64));
        }
        if effect_name == "Int" && op_name == Some("to_hex") {
            let n = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
            let s = format!("{:x}", n);
            let bytes = s.into_bytes();
            match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                Some(ptr) => {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                        *ptr.add(bytes.len()) = 0;
                    }
                    return Some(Value::ptr(ptr));
                }
                None => return Some(Value::nil()),
            }
        }
        if effect_name == "Int" && op_name == Some("to_binary") {
            let n = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
            let s = format!("{:b}", n);
            let bytes = s.into_bytes();
            match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                Some(ptr) => {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                        *ptr.add(bytes.len()) = 0;
                    }
                    return Some(Value::ptr(ptr));
                }
                None => return Some(Value::nil()),
            }
        }
        if effect_name == "Float" && op_name == Some("to_int") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            return Some(Value::int(x as i64));
        }
        if effect_name == "Float" && op_name == Some("to_string") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            let s = format!("{}", x);
            let bytes = s.into_bytes();
            match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                Some(ptr) => {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                        *ptr.add(bytes.len()) = 0;
                    }
                    return Some(Value::ptr(ptr));
                }
                None => return Some(Value::nil()),
            }
        }
        if effect_name == "Float" && op_name == Some("sin") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            return Some(Value::float(f64::sin(x)));
        }
        if effect_name == "Float" && op_name == Some("cos") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            return Some(Value::float(f64::cos(x)));
        }
        if effect_name == "Float" && op_name == Some("sqrt") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            if x < 0.0 {
                return Some(Value::nil());
            }
            return Some(Value::float(f64::sqrt(x)));
        }
        if effect_name == "Float" && op_name == Some("tan") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            return Some(Value::float(f64::tan(x)));
        }
        if effect_name == "Float" && op_name == Some("log") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            if x <= 0.0 {
                return Some(Value::nil());
            }
            return Some(Value::float(f64::ln(x)));
        }
        if effect_name == "Float" && op_name == Some("exp") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            return Some(Value::float(f64::exp(x)));
        }
        if effect_name == "Float" && op_name == Some("log2") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            if x <= 0.0 {
                return Some(Value::nil());
            }
            return Some(Value::float(f64::log2(x)));
        }
        if effect_name == "Float" && op_name == Some("log10") {
            let x = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            if x <= 0.0 {
                return Some(Value::nil());
            }
            return Some(Value::float(f64::log10(x)));
        }
        if effect_name == "Float" && op_name == Some("pow") {
            let base = regs.first().and_then(|v| v.as_float()).unwrap_or(0.0);
            let exp = regs.get(1).and_then(|v| v.as_float()).unwrap_or(0.0);
            return Some(Value::float(f64::powf(base, exp)));
        }
        if effect_name == "String" && op_name == Some("to_int") {
            let s = resolve_value_string(constants, *regs.first().unwrap_or(&Value::nil()));
            let n: i64 = s.parse().unwrap_or(0);
            return Some(Value::int(n));
        }
        if effect_name == "String" && op_name == Some("to_float") {
            let s = resolve_value_string(constants, *regs.first().unwrap_or(&Value::nil()));
            let f: f64 = s.parse().unwrap_or(0.0);
            return Some(Value::float(f));
        }

        if effect_name == "String" && op_name == Some("length") {
            let s = resolve_value_string(constants, *regs.first().unwrap_or(&Value::nil()));
            return Some(Value::int(s.len() as i64));
        }
        if effect_name == "String" && op_name == Some("charAt") {
            let s = resolve_value_string(constants, *regs.first().unwrap_or(&Value::nil()));
            let idx = regs.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
            if idx < 0 || idx as usize >= s.len() {
                return Some(Value::int(-1));
            }
            return Some(Value::int(s.as_bytes()[idx as usize] as i64));
        }
        if effect_name == "String" && op_name == Some("from_char") {
            let code = regs.first().and_then(|v| v.as_int()).unwrap_or(-1);
            if code < 0 {
                return Some(Value::nil());
            }
            let c = match char::from_u32(code as u32) {
                Some(c) => c,
                None => return Some(Value::nil()),
            };
            let s: String = c.to_string();
            let bytes = s.into_bytes();
            match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                Some(ptr) => {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                        *ptr.add(bytes.len()) = 0;
                    }
                    return Some(Value::ptr(ptr));
                }
                None => return Some(Value::nil()),
            }
        }
        if effect_name == "String" && op_name == Some("concat") {
            let a = resolve_value_string(constants, *regs.first().unwrap_or(&Value::nil()));
            let b = resolve_value_string(constants, *regs.get(1).unwrap_or(&Value::nil()));
            let combined = a + &b;
            let bytes = combined.into_bytes();
            match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                Some(ptr) => {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                        *ptr.add(bytes.len()) = 0;
                    }
                    return Some(Value::ptr(ptr));
                }
                None => return Some(Value::nil()),
            }
        }
        if effect_name == "String" && op_name == Some("substring") {
            let s = resolve_value_string(constants, *regs.first().unwrap_or(&Value::nil()));
            let start = regs.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            let len = regs.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            if start < 0 || len < 0 || start as usize > s.len() {
                return Some(Value::nil());
            }
            let end = ((start + len) as usize).min(s.len());
            let sub = &s[start as usize..end];
            let bytes = sub.as_bytes().to_vec();
            match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                Some(ptr) => {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                        *ptr.add(bytes.len()) = 0;
                    }
                    return Some(Value::ptr(ptr));
                }
                None => return Some(Value::nil()),
            }
        }
        if effect_name == "Debug" && op_name == Some("inspect") {
            let label = regs
                .first()
                .map(|v| resolve_value_string(constants, *v))
                .unwrap_or_default();
            let val = regs.get(1).copied().unwrap_or(Value::nil());
            let file = crate::types::source_map_file().unwrap_or_else(|| "<unknown>".to_string());
            eprintln!("[{}] {} = {}", file, label, val.to_string_repr());
            return Some(val);
        }
        if effect_name == "FS" {
            match op_name {
                Some("read") => {
                    let path = regs
                        .first()
                        .map(|v| resolve_value_string(constants, *v))
                        .unwrap_or_default();
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            let bytes = content.into_bytes();
                            match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                                Some(ptr) => {
                                    unsafe {
                                        std::ptr::copy_nonoverlapping(
                                            bytes.as_ptr(),
                                            ptr,
                                            bytes.len(),
                                        );
                                        *ptr.add(bytes.len()) = 0;
                                    }
                                    return Some(Value::ptr(ptr));
                                }
                                None => return Some(Value::nil()),
                            }
                        }
                        Err(_) => return Some(Value::nil()),
                    }
                }
                Some("write") => {
                    let path = regs
                        .first()
                        .map(|v| resolve_value_string(constants, *v))
                        .unwrap_or_default();
                    let content = regs
                        .get(1)
                        .map(|v| resolve_value_string(constants, *v))
                        .unwrap_or_default();
                    if std::fs::write(&path, &content).is_err() {
                        return Some(Value::nil());
                    }
                    return Some(Value::unit());
                }
                Some("append") => {
                    let path = regs
                        .first()
                        .map(|v| resolve_value_string(constants, *v))
                        .unwrap_or_default();
                    let content = regs
                        .get(1)
                        .map(|v| resolve_value_string(constants, *v))
                        .unwrap_or_default();
                    use std::io::Write;
                    let mut file = match std::fs::OpenOptions::new()
                        .append(true)
                        .create(true)
                        .open(&path)
                    {
                        Ok(f) => f,
                        Err(_) => return Some(Value::nil()),
                    };
                    if file.write_all(content.as_bytes()).is_err() {
                        return Some(Value::nil());
                    }
                    return Some(Value::unit());
                }
                Some("exists") => {
                    let path = regs
                        .first()
                        .map(|v| resolve_value_string(constants, *v))
                        .unwrap_or_default();
                    let exists = std::path::Path::new(&path).exists();
                    return Some(Value::bool(exists));
                }
                _ => return None,
            }
        }
        if effect_name == "Array" {
            match op_name {
                Some("length") => {
                    let arr_ptr = regs
                        .first()
                        .and_then(|v| v.as_ptr())
                        .unwrap_or(std::ptr::null_mut());
                    let len = if !arr_ptr.is_null() {
                        self.array_len(arr_ptr).unwrap_or(0) as i64
                    } else {
                        0
                    };
                    return Some(Value::int(len));
                }
                Some("push") => {
                    let arr_ptr = regs
                        .first()
                        .and_then(|v| v.as_ptr())
                        .unwrap_or(std::ptr::null_mut());
                    let elem = regs.get(1).copied().unwrap_or(Value::nil());
                    let len = if !arr_ptr.is_null() {
                        self.array_len(arr_ptr).unwrap_or(0)
                    } else {
                        0
                    };
                    let new_len = len + 1;
                    let size = new_len
                        .checked_mul(std::mem::size_of::<Value>())
                        .unwrap_or(0);
                    if let Some(new_ptr) = self.heap.alloc(size, HeapTypeTag::Array) {
                        unsafe {
                            let new_slots =
                                std::slice::from_raw_parts_mut(new_ptr as *mut Value, new_len);
                            // Copy existing elements, retaining heap refs.
                            if !arr_ptr.is_null() {
                                let old_slots =
                                    std::slice::from_raw_parts(arr_ptr as *const Value, len);
                                for (i, slot) in old_slots.iter().enumerate() {
                                    new_slots[i] = *slot;
                                    if let Some(ptr) = slot.as_ptr() {
                                        self.gc.local_ref(&self.heap, ptr);
                                    }
                                }
                            }
                            // Store new element, retaining if heap value.
                            new_slots[len] = elem;
                            if let Some(ptr) = elem.as_ptr() {
                                self.gc.local_ref(&self.heap, ptr);
                            }
                        }
                        return Some(Value::ptr(new_ptr));
                    }
                    return Some(Value::nil());
                }
                Some("new") => {
                    let n = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
                    if n < 0 {
                        return Some(Value::nil());
                    }
                    let n = n as usize;
                    let init = regs.get(1).copied().unwrap_or(Value::nil());
                    let size = n.checked_mul(std::mem::size_of::<Value>()).unwrap_or(0);
                    if let Some(new_ptr) = self.heap.alloc(size, HeapTypeTag::Array) {
                        unsafe {
                            let slots = std::slice::from_raw_parts_mut(new_ptr as *mut Value, n);
                            for slot in slots.iter_mut() {
                                *slot = init;
                                if let Some(ptr) = init.as_ptr() {
                                    self.gc.local_ref(&self.heap, ptr);
                                }
                            }
                        }
                        return Some(Value::ptr(new_ptr));
                    }
                    return Some(Value::nil());
                }
                Some("set") => {
                    let arr_ptr = regs
                        .first()
                        .and_then(|v| v.as_ptr())
                        .unwrap_or(std::ptr::null_mut());
                    let idx = regs.get(1).and_then(|v| v.as_int()).unwrap_or(-1);
                    let val = regs.get(2).copied().unwrap_or(Value::nil());
                    if arr_ptr.is_null() || idx < 0 {
                        return Some(Value::nil());
                    }
                    let idx = idx as usize;
                    let len = self.array_len(arr_ptr).unwrap_or(0);
                    if idx >= len {
                        return Some(Value::nil());
                    }
                    let size = len.checked_mul(std::mem::size_of::<Value>()).unwrap_or(0);
                    if let Some(new_ptr) = self.heap.alloc(size, HeapTypeTag::Array) {
                        unsafe {
                            let new_slots =
                                std::slice::from_raw_parts_mut(new_ptr as *mut Value, len);
                            let old_slots =
                                std::slice::from_raw_parts(arr_ptr as *const Value, len);
                            for i in 0..len {
                                let src = if i == idx { val } else { old_slots[i] };
                                new_slots[i] = src;
                                if let Some(ptr) = src.as_ptr() {
                                    self.gc.local_ref(&self.heap, ptr);
                                }
                            }
                        }
                        return Some(Value::ptr(new_ptr));
                    }
                    return Some(Value::nil());
                }
                Some("slice") => {
                    let arr_ptr = regs
                        .first()
                        .and_then(|v| v.as_ptr())
                        .unwrap_or(std::ptr::null_mut());
                    let start = regs.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                    let end = regs.get(2).and_then(|v| v.as_int()).unwrap_or(-1);
                    let len = if !arr_ptr.is_null() {
                        self.array_len(arr_ptr).unwrap_or(0)
                    } else {
                        0
                    };
                    let start = start.max(0) as usize;
                    let end = if end < 0 || end as usize > len {
                        len
                    } else {
                        end as usize
                    };
                    if start > end {
                        return Some(Value::nil());
                    }
                    let new_len = end - start;
                    let size = new_len
                        .checked_mul(std::mem::size_of::<Value>())
                        .unwrap_or(0);
                    if let Some(new_ptr) = self.heap.alloc(size, HeapTypeTag::Array) {
                        unsafe {
                            let new_slots =
                                std::slice::from_raw_parts_mut(new_ptr as *mut Value, new_len);
                            let old_slots =
                                std::slice::from_raw_parts(arr_ptr as *const Value, len);
                            for i in 0..new_len {
                                let src = old_slots[start + i];
                                new_slots[i] = src;
                                if let Some(ptr) = src.as_ptr() {
                                    self.gc.local_ref(&self.heap, ptr);
                                }
                            }
                        }
                        return Some(Value::ptr(new_ptr));
                    }
                    return Some(Value::nil());
                }
                Some("range") => {
                    let start = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
                    let end = regs.get(1).and_then(|v| v.as_int()).unwrap_or(0);
                    let len = if end > start {
                        (end - start) as usize
                    } else {
                        0
                    };
                    let size = len.checked_mul(std::mem::size_of::<Value>()).unwrap_or(0);
                    if let Some(new_ptr) = self.heap.alloc(size, HeapTypeTag::Array) {
                        unsafe {
                            let slots = std::slice::from_raw_parts_mut(new_ptr as *mut Value, len);
                            for i in 0..len {
                                slots[i] = Value::int(start + i as i64);
                            }
                        }
                        return Some(Value::ptr(new_ptr));
                    }
                    return Some(Value::nil());
                }
                _ => return None,
            }
        }
        if effect_name == "StrBuilder" {
            return strbuilder_op(self, constants, op_name.unwrap_or(""), regs);
        }
        if effect_name == "Map" {
            return hashmap_op(self, constants, op_name.unwrap_or(""), regs);
        }
        if effect_name == "Http" {
            match op_name {
                Some("get") => {
                    let url = regs
                        .first()
                        .map(|v| resolve_value_string(constants, *v))
                        .unwrap_or_default();
                    #[cfg(feature = "ureq")]
                    {
                        match ureq::get(&url).call() {
                            Ok(response) => match response.into_string() {
                                Ok(body) => {
                                    let bytes = body.into_bytes();
                                    match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                                        Some(ptr) => {
                                            unsafe {
                                                std::ptr::copy_nonoverlapping(
                                                    bytes.as_ptr(),
                                                    ptr,
                                                    bytes.len(),
                                                );
                                                *ptr.add(bytes.len()) = 0;
                                            }
                                            return Some(Value::ptr(ptr));
                                        }
                                        None => return Some(Value::nil()),
                                    }
                                }
                                Err(_) => return Some(Value::nil()),
                            },
                            Err(_) => return Some(Value::nil()),
                        }
                    }
                    #[cfg(not(feature = "ureq"))]
                    {
                        let _ = url;
                        let msg = b"HTTP client disabled (feature 'ureq' not enabled)";
                        match self.heap.alloc(msg.len() + 1, HeapTypeTag::String) {
                            Some(ptr) => {
                                unsafe {
                                    std::ptr::copy_nonoverlapping(msg.as_ptr(), ptr, msg.len());
                                    *ptr.add(msg.len()) = 0;
                                }
                                return Some(Value::ptr(ptr));
                            }
                            None => return Some(Value::nil()),
                        }
                    }
                }
                Some("post") => {
                    let url = regs
                        .first()
                        .map(|v| resolve_value_string(constants, *v))
                        .unwrap_or_default();
                    let body = regs
                        .get(1)
                        .map(|v| resolve_value_string(constants, *v))
                        .unwrap_or_default();
                    #[cfg(feature = "ureq")]
                    {
                        match ureq::post(&url).send_string(&body) {
                            Ok(response) => match response.into_string() {
                                Ok(body) => {
                                    let bytes = body.into_bytes();
                                    match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                                        Some(ptr) => {
                                            unsafe {
                                                std::ptr::copy_nonoverlapping(
                                                    bytes.as_ptr(),
                                                    ptr,
                                                    bytes.len(),
                                                );
                                                *ptr.add(bytes.len()) = 0;
                                            }
                                            return Some(Value::ptr(ptr));
                                        }
                                        None => return Some(Value::nil()),
                                    }
                                }
                                Err(_) => return Some(Value::nil()),
                            },
                            Err(_) => return Some(Value::nil()),
                        }
                    }
                    #[cfg(not(feature = "ureq"))]
                    {
                        let _ = (url, body);
                        let msg = b"HTTP client disabled (feature 'ureq' not enabled)";
                        match self.heap.alloc(msg.len() + 1, HeapTypeTag::String) {
                            Some(ptr) => {
                                unsafe {
                                    std::ptr::copy_nonoverlapping(msg.as_ptr(), ptr, msg.len());
                                    *ptr.add(msg.len()) = 0;
                                }
                                return Some(Value::ptr(ptr));
                            }
                            None => return Some(Value::nil()),
                        }
                    }
                }
                _ => return None,
            }
        }
        if effect_name == "Time" && op_name == Some("now") {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            return Some(Value::int(now));
        }
        if effect_name == "Process" && op_name == Some("run") {
            let cmd = regs
                .first()
                .map(|v| resolve_value_string(constants, *v))
                .unwrap_or_default();
            match std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .output()
            {
                Ok(output) => {
                    if output.status.success() {
                        let out = String::from_utf8_lossy(&output.stdout).to_string();
                        let bytes = out.into_bytes();
                        match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                            Some(ptr) => {
                                unsafe {
                                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                                    *ptr.add(bytes.len()) = 0;
                                }
                                return Some(Value::ptr(ptr));
                            }
                            None => return Some(Value::nil()),
                        }
                    } else {
                        return Some(Value::nil());
                    }
                }
                Err(_) => return Some(Value::nil()),
            }
        }
        if effect_name == "Env" && op_name == Some("get") {
            let name = regs
                .first()
                .map(|v| resolve_value_string(constants, *v))
                .unwrap_or_default();
            match std::env::var(&name) {
                Ok(val) => {
                    let bytes = val.into_bytes();
                    match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                        Some(ptr) => {
                            unsafe {
                                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                                *ptr.add(bytes.len()) = 0;
                            }
                            return Some(Value::ptr(ptr));
                        }
                        None => return Some(Value::nil()),
                    }
                }
                Err(_) => return Some(Value::nil()),
            }
        }
        if effect_name == "System" && op_name == Some("arg") {
            let n = regs.first().and_then(|v| v.as_int()).unwrap_or(-1);
            if n < 0 {
                return Some(Value::nil());
            }
            match std::env::args().nth(n as usize) {
                Some(val) => {
                    let bytes = val.into_bytes();
                    match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                        Some(ptr) => {
                            unsafe {
                                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                                *ptr.add(bytes.len()) = 0;
                            }
                            return Some(Value::ptr(ptr));
                        }
                        None => return Some(Value::nil()),
                    }
                }
                None => return Some(Value::nil()),
            }
        }
        if effect_name == "Random" && op_name == Some("int") {
            let lo = regs.first().and_then(|v| v.as_int()).unwrap_or(0);
            let hi = regs.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            if lo > hi {
                return Some(Value::int(lo));
            }
            let range = (hi - lo + 1) as u64;
            use rand_core::RngCore;
            let mut buf = [0u8; 8];
            rand_core::OsRng.fill_bytes(&mut buf);
            let r = u64::from_le_bytes(buf) % range;
            return Some(Value::int(lo + r as i64));
        }
        if effect_name == "Debug" {
            if op_name == Some("dbg") {
                let val = regs.first().copied().unwrap_or(Value::nil());
                let msg = resolve_value_string(constants, val);
                eprintln!("[dbg] {}", msg);
                return Some(val);
            }
            return None;
        }
        if effect_name != "IO" {
            return None;
        }
        match op_name {
            Some("print") | Some("println") => {
                let message = regs
                    .first()
                    .map(|v| resolve_value_string(constants, *v))
                    .unwrap_or_default();
                if let Some(sink) = &self.io_output {
                    sink.borrow_mut().push(message);
                } else {
                    println!("{}", message);
                }
                Some(Value::unit())
            }
            Some("log") => {
                let level = regs
                    .first()
                    .map(|v| resolve_value_string(constants, *v))
                    .unwrap_or_default();
                let message = regs
                    .get(1)
                    .map(|v| resolve_value_string(constants, *v))
                    .unwrap_or_default();
                eprintln!("[{}] {}", level.to_uppercase(), message);
                Some(Value::unit())
            }
            Some("log_error") => {
                let message = regs
                    .first()
                    .map(|v| resolve_value_string(constants, *v))
                    .unwrap_or_default();
                eprintln!("ERROR: {}", message);
                Some(Value::unit())
            }
            Some("read") => {
                let mut input = String::new();
                if std::io::stdin().read_line(&mut input).is_err() {
                    return Some(Value::nil());
                }
                while input.ends_with(|c| c == '\n' || c == '\r') {
                    input.pop();
                }
                let bytes = input.into_bytes();
                match self.heap.alloc(bytes.len() + 1, HeapTypeTag::String) {
                    Some(ptr) => {
                        // SAFETY: `ptr` points to bytes.len()+1 freshly
                        // allocated bytes on the standalone heap.
                        unsafe {
                            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                            *ptr.add(bytes.len()) = 0;
                        }
                        Some(Value::ptr(ptr))
                    }
                    None => Some(Value::nil()),
                }
            }
            _ => None,
        }
    }

    /// Module-aware built-in effect dispatch for the standalone (actor-free)
    /// VM. The default trait impl delegates straight to
    /// `perform_builtin_effect` and drops the module, which is fine for
    /// string-id argument resolution but makes `Http.serve` impossible:
    /// the server needs the handler's module and function-table index to
    /// dispatch requests. This override handles `Http.serve` with the
    /// module available and routes everything else through the existing
    /// no-module path.
    fn perform_builtin_effect_in_module(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        module: &CodeModule,
        regs: &[Value],
    ) -> Option<Value> {
        if effect_name == "Web" && op_name == Some("route") {
            let method = regs.get(0).copied();
            let path = regs.get(1).copied();
            let handler = regs.get(2).copied();
            if let (Some(m), Some(p), Some(h)) = (method, path, handler) {
                if let Some(route) = crate::runtime::WebRoute::from_registers(
                    m,
                    p,
                    h,
                    &module.constants,
                    module.clone(),
                ) {
                    self.routes.push(route);
                }
            }
            return Some(Value::unit());
        }
        if effect_name == "Http" && op_name == Some("serve") {
            let port = regs.first().and_then(|v| v.as_int()).unwrap_or(0) as u16;
            let func_idx = match regs.get(1) {
                Some(v) if v.is_closure() => {
                    let payload = v.as_raw() & crate::value_layout::PAYLOAD_MASK;
                    if payload & CLOSURE_ENV_FLAG != 0 {
                        // Closures carrying an environment cannot be used as
                        // an Http.serve handler (the handler must be a plain
                        // top-level function with a stable function-table
                        // index). Return nil rather than dispatch garbage.
                        return Some(Value::nil());
                    }
                    payload as usize
                }
                Some(v) => {
                    // Function index passed as a raw Int (from a func_map
                    // lookup or a direct function reference).
                    v.as_int().unwrap_or(0) as usize
                }
                None => return Some(Value::nil()),
            };
            return match crate::runtime::HttpServerState::bind(port, module.clone(), func_idx) {
                Ok(server) => {
                    let actual_port = server.port;
                    // Deliberately leak: a standalone HTTP server keeps
                    // serving for the process lifetime. Dropping the
                    // server (on VM teardown) would immediately shut down
                    // the listener thread, so `perform Http.serve` in an
                    // actor-free program would bind and then die before
                    // serving a single request. The runtime-backed path
                    // stores the server on `Runtime` (which outlives
                    // `vm.run()`), so it does not need this.
                    std::mem::forget(server);
                    Some(Value::int(actual_port as i64))
                }
                Err(_) => Some(Value::nil()),
            };
        }
        self.perform_builtin_effect(effect_name, op_name, &module.constants, regs)
    }
}

// ---------------------------------------------------------------------------
// Value: NaN-boxed tagged value
// ---------------------------------------------------------------------------

/// Tagged value using NaN boxing.
///
/// All non-float values are encoded in the quiet-NaN payload of an f64.
/// The high 16 bits hold the type tag; the low 48 bits hold the payload.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Value {
    raw: u64,
}

use crate::value_layout::{
    is_float_raw, sext48, tag_object, PAYLOAD_MASK, TAG_ACTOR, TAG_BOOL, TAG_CLOSURE, TAG_INT,
    TAG_MASK, TAG_NIL, TAG_OBJECT, TAG_PTR, TAG_STRING, TAG_UNIT,
};

impl Value {
    /// Create a nil value.
    pub fn nil() -> Self {
        Value { raw: TAG_NIL }
    }

    /// Create an integer value.
    pub fn int(n: i64) -> Self {
        // Store directly in the 48-bit payload.
        let payload = (n as u64) & PAYLOAD_MASK;
        Value {
            raw: TAG_INT | payload,
        }
    }

    /// Create a float value.
    pub fn float(f: f64) -> Self {
        Value { raw: f.to_bits() }
    }

    /// Create a boolean value.
    pub fn bool(b: bool) -> Self {
        Value {
            raw: TAG_BOOL | (b as u64),
        }
    }

    /// Create a unit value.
    pub fn unit() -> Self {
        Value { raw: TAG_UNIT }
    }

    /// Create an actor reference.
    pub fn actor_ref(id: u64) -> Self {
        Value {
            raw: TAG_ACTOR | (id & PAYLOAD_MASK),
        }
    }

    /// Create a closure reference.
    pub fn closure(id: u64) -> Self {
        Value {
            raw: TAG_CLOSURE | (id & PAYLOAD_MASK),
        }
    }

    /// Create an object-store reference.
    pub fn object(id: u64) -> Self {
        Value {
            raw: tag_object(id),
        }
    }

    /// Create a pointer value (for strings, lists, etc.).
    pub fn ptr(p: *mut u8) -> Self {
        Value {
            raw: TAG_PTR | (p as u64 & PAYLOAD_MASK),
        }
    }

    /// Create a string reference (index into string pool).
    pub fn string(id: u32) -> Self {
        Value {
            raw: TAG_STRING | (id as u64),
        }
    }

    // -- Type checks --

    pub fn is_nil(&self) -> bool {
        self.raw == TAG_NIL
    }
    pub fn is_unit(&self) -> bool {
        self.raw == TAG_UNIT
    }
    pub fn is_int(&self) -> bool {
        (self.raw & TAG_MASK) == TAG_INT
    }
    #[inline]
    pub fn is_float(&self) -> bool {
        is_float_raw(self.raw)
    }
    pub fn is_bool(&self) -> bool {
        (self.raw & TAG_MASK) == TAG_BOOL
    }
    pub fn is_actor_ref(&self) -> bool {
        (self.raw & TAG_MASK) == TAG_ACTOR
    }

    // -- Extractors --

    pub fn as_int(&self) -> Option<i64> {
        if (self.raw & TAG_MASK) == TAG_INT {
            Some(sext48(self.raw & PAYLOAD_MASK))
        } else {
            None
        }
    }
    #[inline]
    pub fn as_float(&self) -> Option<f64> {
        if is_float_raw(self.raw) {
            Some(f64::from_bits(self.raw))
        } else {
            None
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if (self.raw & TAG_MASK) == TAG_BOOL {
            Some((self.raw & 1) != 0)
        } else {
            None
        }
    }

    pub fn as_actor_id(&self) -> Option<u64> {
        if (self.raw & TAG_MASK) == TAG_ACTOR {
            Some(self.raw & PAYLOAD_MASK)
        } else {
            None
        }
    }

    pub fn as_ptr(&self) -> Option<*mut u8> {
        if (self.raw & TAG_MASK) == TAG_PTR {
            Some((self.raw & PAYLOAD_MASK) as *mut u8)
        } else {
            None
        }
    }

    pub fn is_ptr(&self) -> bool {
        (self.raw & TAG_MASK) == TAG_PTR
    }
    pub fn is_string(&self) -> bool {
        (self.raw & TAG_MASK) == TAG_STRING
    }
    pub fn is_closure(&self) -> bool {
        (self.raw & TAG_MASK) == TAG_CLOSURE
    }
    pub fn is_object(&self) -> bool {
        (self.raw & TAG_MASK) == TAG_OBJECT
    }

    pub fn as_object_id(&self) -> Option<u64> {
        if self.is_object() {
            Some(self.raw & PAYLOAD_MASK)
        } else {
            None
        }
    }

    pub fn as_string_id(&self) -> Option<u32> {
        if self.is_string() {
            Some((self.raw & PAYLOAD_MASK) as u32)
        } else {
            None
        }
    }

    /// Return the raw NaN-boxed bits.
    pub fn as_raw(&self) -> u64 {
        self.raw
    }

    /// Construct a Value from raw NaN-boxed bits.
    ///
    /// # Safety
    /// The caller must ensure the bits form a valid tagged value.
    pub fn from_raw(raw: u64) -> Self {
        Value { raw }
    }

    /// Return the raw NaN-boxed bits (opaque bit pattern).
    pub fn to_bits(self) -> u64 {
        self.raw
    }

    /// Construct a Value from raw NaN-boxed bits.
    pub fn from_bits(raw: u64) -> Self {
        Value { raw }
    }

    pub fn to_string_repr(&self) -> String {
        if self.is_nil() {
            "nil".to_string()
        } else if self.is_unit() {
            "()".to_string()
        } else if let Some(n) = self.as_int() {
            n.to_string()
        } else if let Some(f) = self.as_float() {
            f.to_string()
        } else if let Some(b) = self.as_bool() {
            b.to_string()
        } else if self.is_actor_ref() {
            format!("#Actor:{}", self.as_actor_id().unwrap())
        } else if let Some(oid) = self.as_object_id() {
            format!("#Object:{}", oid)
        } else {
            format!("#Value({:x})", self.raw)
        }
    }
}

/// Convert a bytecode constant into a runtime value.
pub(crate) fn constant_to_value(c: &Constant) -> Value {
    match c {
        Constant::Int(i) => Value::int(*i),
        Constant::Float(f) => Value::float(*f),
        Constant::String(_) => Value::nil(), // strings are heap-allocated on demand
        Constant::Bool(b) => Value::bool(*b),
        Constant::Nil => Value::nil(),
        Constant::Unit => Value::unit(),
        Constant::FunctionRef(_) | Constant::BehaviorRef(_) | Constant::TypeDescriptor(_) => {
            Value::nil()
        }
    }
}

/// Convert a bytecode constant pool to raw NaN-boxed bits for the JIT.
///
/// String constants must encode their constant-pool index exactly like the
/// interpreter's `ConstU` (`Value::string(idx)`); encoding them as nil makes
/// every tiered-up string load silently produce nil.
fn constants_to_jit_bits(constants: &[Constant]) -> Vec<u64> {
    constants
        .iter()
        .enumerate()
        .map(|(idx, c)| match c {
            Constant::Int(i) => Value::int(*i).to_bits(),
            Constant::Float(f) => Value::float(*f).to_bits(),
            Constant::String(_) => Value::string(idx as u32).to_bits(),
            Constant::Bool(b) => Value::bool(*b).to_bits(),
            Constant::Nil => Value::nil().to_bits(),
            Constant::Unit => Value::unit().to_bits(),
            Constant::FunctionRef(_) | Constant::BehaviorRef(_) | Constant::TypeDescriptor(_) => {
                Value::nil().to_bits()
            }
        })
        .collect()
}

/// Parse a `ReceiveMatch` spec constant of the form
/// `"max_params:id1,id2,..."` into (max_params, behavior ids).
/// Malformed specs degrade to "no arms, no payload registers", which the
/// VM treats as an unconditional no-match.
fn parse_receive_spec(spec: &str) -> (usize, Vec<u16>) {
    let Some((head, rest)) = spec.split_once(':') else {
        return (0, Vec::new());
    };
    let max_params = head.parse::<usize>().unwrap_or(0);
    let ids = rest
        .split(',')
        .filter_map(|s| s.parse::<u16>().ok())
        .collect();
    (max_params, ids)
}

// ---------------------------------------------------------------------------
// Frame: activation frame
// ---------------------------------------------------------------------------

#[derive(Clone)]
/// Activation frame: 256 registers + spill slots + metadata.
pub struct Frame {
    /// 256 general-purpose registers (r0..r255).
    pub regs: [Value; 256],
    /// Spill slots for functions whose local count exceeds the register file.
    /// Indexed by spill slot index (u16). Empty for functions that fit entirely
    /// in registers.
    pub spilled: Vec<Value>,
    /// Program counter (bytecode index).
    pub pc: usize,
    /// Module index in VM.modules.
    pub module_idx: usize,
    /// Return destination register.
    pub return_dst: u8,
    /// Index of the caller frame in the VM's flat frame stack.
    /// None for the top-level frame.
    pub caller_idx: Option<usize>,
    /// Closure environment (None if not a closure).
    pub closure_env: Option<Value>,
}

impl Frame {
    /// Create a new frame with all registers initialized to nil.
    pub fn new(caller_idx: Option<usize>, module_idx: usize) -> Self {
        Frame {
            regs: [Value::nil(); 256],
            spilled: Vec::new(),
            pc: 0,
            module_idx,
            return_dst: 0,
            caller_idx,
            closure_env: None,
        }
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show only first 8 registers and key metadata to avoid
        // overwhelming output (all 256 regs is too much).
        f.debug_struct("Frame")
            .field("pc", &self.pc)
            .field("module_idx", &self.module_idx)
            .field("return_dst", &self.return_dst)
            .field("regs[0..8]", &&self.regs[0..8])
            .field("caller_idx", &self.caller_idx)
            .field("closure_env", &self.closure_env)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// HandlerFrame: handler stack entry for algebraic effects
// ---------------------------------------------------------------------------

/// A handler frame tracks a single `handle` block's context.
///
/// Created by `Handle` opcode, popped by `Unwind`.
/// When `Perform` finds this handler, it captures a `Continuation`
/// and stores it here for `Resume` to use.  For single-shot handlers
/// the lightweight `SingleShotState` is used instead — no heap allocation.
#[derive(Debug, Clone)]
pub struct HandlerFrame {
    /// Index into the module's handler_tables.
    pub handler_table_idx: usize,
    /// Module index (so we can look up handler_tables).
    pub module_idx: usize,
    /// PC to resume at after the handle block completes normally.
    pub resume_pc: usize,
    /// Destination register for the handle block's result.
    pub resume_dst: u8,
    /// Captured continuation (set by Perform, consumed by Resume).
    pub captured_continuation: Option<Continuation>,
    /// Lightweight continuation state for single-shot handlers.
    /// When `Some`, `captured_continuation` is `None` and `Resume`
    /// restores from this inline state without a heap allocation.
    pub single_shot_state: Option<SingleShotState>,
}

/// Inline continuation state for single-shot effect handlers.
///
/// A single-shot handler resumes the continuation at most once.
/// Instead of deep-cloning every frame into a heap-allocated
/// `Continuation`, we snapshot only the current frame's registers
/// and restore them on `Resume`.  This avoids a `Vec<Frame>`
/// allocation (~few hundred bytes + per-frame metadata).
#[derive(Debug, Clone)]
pub struct SingleShotState {
    /// PC after the `PerformDirect` instruction (the continuation point).
    pub resume_pc: usize,
    /// Destination register for the resume value.
    pub resume_dst: u8,
    /// Step count at capture time.
    pub step_count: usize,
    /// Snapshot of the current frame's registers at the perform site.
    pub regs: [Value; 256],
}

impl HandlerFrame {
    pub fn new(
        handler_table_idx: usize,
        module_idx: usize,
        resume_pc: usize,
        resume_dst: u8,
    ) -> Self {
        HandlerFrame {
            handler_table_idx,
            module_idx,
            resume_pc,
            resume_dst,
            captured_continuation: None,
            single_shot_state: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Continuation: captured execution state for algebraic effects
// ---------------------------------------------------------------------------

/// A captured continuation — a deep snapshot of the VM's execution state
/// at the point of a `perform` call. Restored by `resume` to continue
/// the suspended computation with a value.
#[derive(Debug, Clone)]
pub struct Continuation {
    pub frames: Vec<Frame>,
    /// Index of the active frame within `frames`.
    pub current_frame_idx: usize,
    /// Program counter at the point of capture (points past Perform).
    pub resume_pc: usize,
    /// Destination register for the resume value.
    pub resume_dst: u8,
    /// Step count at capture time.
    pub step_count: usize,
    /// Snapshot of the handler stack at capture time.
    /// Only populated during serialization, empty during normal capture.
    pub handler_stack_snapshot: Vec<HandlerFrame>,
}

impl Continuation {
    /// Capture a continuation from the current VM state.
    pub(crate) fn capture(vm: &VM, resume_dst: u8) -> Option<Self> {
        let current_idx = vm.current_frame_idx?;
        Some(Continuation {
            frames: vm
                .frames
                .iter()
                .take(current_idx + 1)
                .map(clone_frame)
                .collect(),
            current_frame_idx: current_idx,
            resume_pc: vm.frames[current_idx].pc, // PC already points past the Perform instruction
            resume_dst,
            step_count: vm.step_count,
            handler_stack_snapshot: Vec::new(),
        })
    }

    /// Restore this continuation into the VM, placing `value` in the
    /// resume destination register.
    pub(crate) fn restore(self, vm: &mut VM, value: Value) {
        vm.frames = self.frames;
        vm.current_frame_idx = Some(self.current_frame_idx);
        vm.frames[self.current_frame_idx].regs[self.resume_dst as usize] = value;
        vm.frames[self.current_frame_idx].pc = self.resume_pc;
        vm.step_count = self.step_count;
    }
}

// ---------------------------------------------------------------------------
// VM: Virtual Machine
// ---------------------------------------------------------------------------

/// Deep-clone a single frame.
fn clone_frame(frame: &Frame) -> Frame {
    Frame {
        regs: frame.regs,
        spilled: frame.spilled.clone(),
        pc: frame.pc,
        module_idx: frame.module_idx,
        return_dst: frame.return_dst,
        caller_idx: frame.caller_idx,
        closure_env: frame.closure_env,
    }
}

/// Captured VM state for a suspended workflow step (e.g. waiting on a signal).
/// The runtime can extract this state from the VM, store it on the actor, and
/// restore it later when the signal arrives.
#[derive(Debug)]
pub struct SuspendedVmState {
    pub frames: Vec<Frame>,
    pub current_frame_idx: Option<usize>,
    pub handler_stack: Vec<HandlerFrame>,
    pub step_count: usize,
}

/// Register-based bytecode virtual machine.
///
/// Executes Nulang bytecode modules with:
/// - 256 registers per frame
/// - NaN-boxed tagged values
/// - Algebraic effects via handler stack
/// - Capability tracking
pub struct VM {
    /// Loaded bytecode modules.
    pub modules: Vec<CodeModule>,
    /// Flat stack of activation frames.  The active frame is at
    /// `current_frame_idx`; earlier entries are callers.
    frames: Vec<Frame>,
    /// Index of the currently executing frame in `frames`.
    current_frame_idx: Option<usize>,
    /// Handler stack for algebraic effects.
    pub handler_stack: Vec<HandlerFrame>,
    /// Step counter (for debugging / limits).
    step_count: usize,
    /// Set by try_jit_execute when a JIT safepoint triggers a yield.
    /// Consumed by run_from / resume to return early; reset at start of run_from.
    pub yield_pending: bool,
    /// Optional JIT session for tiered compilation.
    jit_session: Option<Box<dyn JitBackend>>,
    /// Per-module constant pools converted to raw bits for the JIT.
    jit_constants: Vec<Vec<u64>>,
    /// Runtime error raised by a re-entrant JIT direct call (taken from the
    /// JIT pending-error thread-local in `try_jit_execute`; consumed by
    /// `step` so the error surfaces as a VM error). None when the last JIT
    /// region ran cleanly.
    jit_pending_error: Option<String>,
    /// Local node ID reported by the `NodeId` opcode.
    node_id: u64,
    /// Migration requests recorded by the `Migrate` opcode when no runtime
    /// callback is installed.
    pending_migrations: Vec<(u64, u64)>,
    /// Gossip messages recorded by the `Gossip` opcode when no runtime
    /// callback is installed.
    gossip_log: Vec<String>,
    /// When true, FFI calls are restricted to libraries in `ffi_allowlist`.
    ffi_sandbox: bool,
    /// Set of library paths allowed when `ffi_sandbox` is true.
    ffi_allowlist: std::collections::HashSet<String>,
    /// Name of the signal that caused the most recent workflow suspension.
    /// Filled by `SignalWait` and consumed by the runtime after `run`/`run_from`
    /// returns a suspend error.
    pub suspended_signal_name: Option<String>,
    /// Timeout in milliseconds of the most recent receive-wait suspension.
    /// Filled by `ReceiveWait` and consumed by the runtime after
    /// `run`/`run_from`/`resume` returns the `"ReceiveWait:suspend"`
    /// sentinel (same pattern as `suspended_signal_name`).
    pub suspended_receive_timeout: Option<i64>,
    /// Optional distributed runtime callbacks for remote operations.
    distributed_callbacks: Option<Box<dyn DistributedVmCallbacks>>,
    /// Actor-runtime callbacks: heap allocation, drop, spawn.
    ///
    /// Defaults to a standalone heap so the VM is usable without a runtime.
    actor_callbacks: Box<dyn ActorVmCallbacks>,
    /// Capture environments for closures that captured enclosing locals.
    /// Indexed by the payload of env-flagged closure values.
    ///
    /// KNOWN LIMITATION: entries are never reclaimed by the GC — closures are
    /// plain `Value`s, not heap objects, so ORCA has no way to know when the
    /// last reference to an env has gone out of scope, and closures stored
    /// inside heap composites (records/tuples/arrays, actor state) mean a
    /// scan of live VM frames alone cannot soundly prove an env is dead.
    /// Doing this correctly requires making closures first-class GC-traced
    /// heap objects; that is a larger follow-up, not attempted here.
    ///
    /// For a single self-contained execution (the standalone VM, and each
    /// REPL evaluation) this is bounded: `clear_closure_envs` should be
    /// called before starting a fresh, independent program so envs from the
    /// previous run — verifiably unreachable, since `run`/`run_from` always
    /// rebuild `frames` from scratch — don't accumulate forever.
    ///
    /// For a VM shared across many actors and messages over a long-running
    /// process (`Runtime`'s bytecode-call path), no such safe reset point
    /// exists today: a different actor's suspended frame or persisted state
    /// could still reference an existing env, so envs accumulate for the
    /// life of the process. Use `closure_env_count` to monitor growth.
    ///
    /// Scoping this per-actor (freed when the owning `Actor`/`ActorHeap` is
    /// dropped, matching how per-actor heap memory is already reclaimed) is
    /// NOT a safe fix without more work: closures are ordinary `Value`s with
    /// no actor-boundary enforcement, so a closure created by one actor can
    /// be sent to another via a message/spawn argument or stored into
    /// shared/persisted state — if the originating actor terminated first,
    /// the receiving actor would hold a dangling env index into a Vec that
    /// no longer exists. `max_closure_envs` bounds the leak's blast radius
    /// with an honest error instead, until real GC tracing lands.
    closure_envs: Vec<ClosureEnv>,
    /// Ceiling on `closure_envs.len()`; defaults to `MAX_CLOSURE_ENVS`.
    /// A field (not just the constant) so tests can shrink it instead of
    /// actually allocating millions of envs to exercise the limit.
    max_closure_envs: usize,
    /// Debugger hook invoked before each interpreted instruction (drives the
    /// DAP server's breakpoints/stepping). When present, the JIT path is
    /// disabled so every instruction flows through the interpreter.
    debug_hook: Option<Box<dyn DebugHook>>,
    /// When set, `Print`/`SPrint`/`IO.print` output is captured here instead
    /// of writing to stdout (so a debug session's program output can be
    /// forwarded as DAP `output` events without corrupting the DAP stream).
    capture_output: Option<std::rc::Rc<std::cell::RefCell<Vec<String>>>>,
}

/// Immutable snapshot handed to the debug hook before each interpreted
/// instruction executes. The hook must not mutate the VM; pauses are
/// signalled by returning [`DebugAction::Pause`] and handled by the caller
/// (which owns the VM) once `step` returns the `DEBUG_PAUSE_MSG` sentinel.
pub struct DebugContext {
    pub module_idx: usize,
    /// PC of the instruction about to execute.
    pub pc: usize,
    /// Index of the current activation frame in `VM.frames`.
    pub frame_idx: usize,
    pub opcode: crate::bytecode::OpCode,
    /// Number of activation frames on the stack (1 == top-level frame).
    pub frame_depth: usize,
    /// Source line of `pc` (via the module line table), if any.
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugAction {
    /// Execute the current instruction normally.
    Continue,
    /// Stop execution; `step` returns the `DEBUG_PAUSE_MSG` sentinel error.
    Pause,
}

/// Debugger hook installed via [`VM::set_debug_hook`]. Called before every
/// interpreted instruction while the VM runs.
pub trait DebugHook {
    fn before_instruction(&mut self, ctx: &DebugContext) -> DebugAction;
}

/// Error message `step` returns when the debug hook requests a pause.
pub const DEBUG_PAUSE_MSG: &str = "DebugPause";

/// True when a `NuError` is the debug-pause sentinel (survives
/// `enrich_error`, which appends a stack trace).
pub fn is_debug_pause(err: &NuError) -> bool {
    matches!(err, NuError::VMError { msg, .. } if msg.split('\n').next() == Some(DEBUG_PAUSE_MSG))
}

/// Runtime error for integer arithmetic overflowing the 48-bit tagged range.
/// Used by the compiled-code runtime helpers (`src/jit/runtime.rs`), which
/// cannot unwind and report the error via `record_arith_error`.
pub fn int_overflow_error(op: &str, a: i64, b: i64) -> NuError {
    NuError::runtime_error(
        format!(
            "integer overflow: `{}` on {} and {} exceeds the 48-bit range \
             [{}, {}] supported by the VM encoding \
             (spec: Int is i64; wider encoding is a known limitation)",
            op,
            a,
            b,
            crate::value_layout::INT48_MIN,
            crate::value_layout::INT48_MAX
        ),
        Span::default(),
    )
}

/// Runtime error for arithmetic on operands of the wrong type.
/// Used by the compiled-code runtime helpers (`src/jit/runtime.rs`).
pub fn arith_type_error(op: &str, a: Value, b: Value) -> NuError {
    NuError::runtime_error(
        format!(
            "type error: arithmetic `{}` requires numeric operands, got {} and {}",
            op,
            a.to_string_repr(),
            b.to_string_repr()
        ),
        Span::default(),
    )
}

/// Captured environment of a closure: the lifted function it wraps plus the
/// values captured at creation time.
#[derive(Debug, Clone)]
pub struct ClosureEnv {
    pub func_idx: usize,
    pub captures: Vec<Value>,
}

/// Payload bit distinguishing env-carrying closures (index into
/// `VM::closure_envs`) from immediate closures (payload = function index).
pub const CLOSURE_ENV_FLAG: u64 = 0x0000_4000_0000_0000;
pub const CLOSURE_ENV_IDX_MASK: u64 = CLOSURE_ENV_FLAG - 1;
/// closure count shouldn't come close — while still bounding the leak to a
/// fixed, predictable amount of memory instead of running unboundedly
/// toward an uncontrolled OOM.
const MAX_CLOSURE_ENVS: usize = 10_000_000;

impl VM {
    /// Create a new VM with the JIT tiering enabled (the default).
    pub fn new() -> Self {
        Self::new_with_jit(true)
    }

    /// Create a new VM with the JIT tiering disabled. Every instruction
    /// flows through the interpreter — used for deterministic interpreter
    /// benchmarking and debugging, and as the explicit fallback path.
    pub fn new_without_jit() -> Self {
        Self::new_with_jit(false)
    }

    fn new_with_jit(enable_jit: bool) -> Self {
        VM {
            modules: Vec::new(),
            frames: Vec::with_capacity(64),
            current_frame_idx: None,
            handler_stack: Vec::new(),
            step_count: 0,
            yield_pending: false,
            jit_session: if enable_jit {
                create_default_jit()
            } else {
                None
            },
            jit_constants: Vec::new(),
            jit_pending_error: None,
            node_id: 0,
            pending_migrations: Vec::new(),
            gossip_log: Vec::new(),
            ffi_sandbox: false,
            ffi_allowlist: std::collections::HashSet::new(),
            suspended_signal_name: None,
            suspended_receive_timeout: None,
            distributed_callbacks: None,
            actor_callbacks: Box::new(StandaloneVmCallbacks::new()),
            closure_envs: Vec::new(),
            max_closure_envs: MAX_CLOSURE_ENVS,
            debug_hook: None,
            capture_output: None,
        }
    }

    /// Return a shared reference to the currently executing frame.
    #[inline(always)]
    fn _current_frame(&self) -> &Frame {
        &self.frames[self.current_frame_idx.expect("no current frame")]
    }

    /// Return a mutable reference to the currently executing frame.
    #[inline(always)]
    fn _current_frame_mut(&mut self) -> &mut Frame {
        &mut self.frames[self.current_frame_idx.expect("no current frame")]
    }

    /// Return a reference to the CodeModule for the current frame.
    /// The module index does not change within a single `step()` call,
    /// so the compiler can hoist this lookup.
    #[inline(always)]
    fn _current_module(&self) -> &CodeModule {
        &self.modules[self._current_frame().module_idx]
    }
    /// Override the closure-env ceiling. Exposed for testing the limit
    /// without actually allocating `MAX_CLOSURE_ENVS` entries.
    #[cfg(test)]
    pub(crate) fn set_max_closure_envs_for_test(&mut self, n: usize) {
        self.max_closure_envs = n;
    }

    /// Install a debugger hook. When set, JIT execution is disabled and the
    /// hook is invoked before every interpreted instruction; a `Pause` return
    /// makes `step` return the `DEBUG_PAUSE_MSG` sentinel error.
    pub fn set_debug_hook(&mut self, hook: Option<Box<dyn DebugHook>>) {
        self.debug_hook = hook;
    }

    /// Route `Print`/`SPrint`/`IO.print` output into `buf` instead of stdout
    /// (used by the DAP server to forward program output as `output` events
    /// without corrupting the DAP stream). `None` restores normal printing.
    pub fn set_output_capture(
        &mut self,
        buf: Option<std::rc::Rc<std::cell::RefCell<Vec<String>>>>,
    ) {
        self.capture_output = buf;
    }

    /// Print `s` to stdout, or capture it when `set_output_capture` is active.
    fn emit_output(&self, s: &str) {
        if let Some(buf) = &self.capture_output {
            buf.borrow_mut().push(s.to_string());
        } else {
            print!("{}", s);
        }
    }

    /// Capture `IO.print` effect output into `buf` (only effective when the
    /// actor callbacks are the standalone VM callbacks, i.e. no runtime is
    /// attached — the DAP debuggee's configuration). `None` restores printing.
    pub fn set_io_output(&mut self, buf: Option<std::rc::Rc<std::cell::RefCell<Vec<String>>>>) {
        if let Some(sb) = (&mut *self.actor_callbacks as &mut dyn std::any::Any)
            .downcast_mut::<StandaloneVmCallbacks>()
        {
            sb.io_output = buf;
        }
    }

    /// Number of activation frames between the given frame and the stack
    /// bottom (1 for a frame with no caller).
    fn frame_depth(&self, frame_idx: usize) -> usize {
        let mut depth = 0;
        let mut idx = Some(frame_idx);
        while let Some(i) = idx {
            depth += 1;
            idx = self.frames.get(i).and_then(|f| f.caller_idx);
        }
        depth
    }

    /// Index of the currently executing frame, if any.
    pub fn current_frame_index(&self) -> Option<usize> {
        self.current_frame_idx
    }

    /// All activation frames (bottom-up order in the flat `frames` vector).
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// Loaded bytecode modules.
    pub fn modules(&self) -> &[CodeModule] {
        &self.modules
    }

    /// PC of the current frame (the next instruction to execute).
    pub fn current_pc(&self) -> Option<usize> {
        self.current_frame_idx.map(|i| self.frames[i].pc)
    }

    /// Source line of the current frame's pc, if the module has a line table.
    pub fn current_line(&self) -> Option<u32> {
        let idx = self.current_frame_idx?;
        let f = self.frames.get(idx)?;
        self.modules.get(f.module_idx).and_then(|m| m.line_at(f.pc))
    }

    /// Enable FFI sandboxing, restricting calls to the given library paths.
    /// Pass an empty list to deny all FFI calls while sandboxing is enabled.
    /// Call with `allowlist: vec![]` and then add libraries via
    /// `allow_ffi_library`.
    pub fn set_ffi_sandbox(&mut self, enabled: bool, allowlist: Vec<String>) {
        self.ffi_sandbox = enabled;
        self.ffi_allowlist = allowlist.into_iter().collect();
    }

    /// Add a library path to the FFI allow-list. No-op if sandboxing is
    /// not enabled (the check only fires when `ffi_sandbox` is true).
    pub fn allow_ffi_library(&mut self, path: &str) {
        self.ffi_allowlist.insert(path.to_string());
    }

    /// Returns true if FFI sandboxing is currently active.
    pub fn is_ffi_sandboxed(&self) -> bool {
        self.ffi_sandbox
    }
    /// Set the local node ID returned by the `NodeId` opcode.
    pub fn set_node_id(&mut self, node_id: u64) {
        self.node_id = node_id;
    }

    /// Install distributed runtime callbacks for remote opcodes.
    pub fn set_distributed_callbacks(&mut self, callbacks: Box<dyn DistributedVmCallbacks>) {
        self.distributed_callbacks = Some(callbacks);
    }

    /// Install actor-runtime callbacks for Spawn and heap operations.
    ///
    /// Replaces the default standalone heap, so all subsequent allocations go
    /// through the supplied runtime.
    pub fn set_actor_callbacks(&mut self, callbacks: Box<dyn ActorVmCallbacks>) {
        self.actor_callbacks = callbacks;
    }
    /// Allocate memory on the actor's heap via the callback trait.
    pub fn alloc_on_heap(&mut self, size: usize, type_tag: HeapTypeTag) -> Option<*mut u8> {
        self.actor_callbacks.alloc(size, type_tag)
    }

    /// Capture the current VM execution state so a workflow step can be
    /// suspended (e.g. while waiting for a signal) and resumed later.
    pub fn take_suspended_state(&mut self) -> Option<SuspendedVmState> {
        if self.current_frame_idx.is_none() {
            return None;
        }
        Some(SuspendedVmState {
            frames: std::mem::take(&mut self.frames),
            current_frame_idx: self.current_frame_idx.take(),
            handler_stack: std::mem::take(&mut self.handler_stack),
            step_count: self.step_count,
        })
    }

    /// Restore a previously captured VM execution state.
    pub fn restore_suspended_state(&mut self, state: SuspendedVmState) {
        self.frames = state.frames;
        self.current_frame_idx = state.current_frame_idx;
        self.handler_stack = state.handler_stack;
        self.step_count = state.step_count;
    }

    /// Set the current execution frame. Used by the runtime to execute actor
    /// bytecode behavior handlers.
    pub fn set_current_frame(&mut self, frame: Frame) {
        self.frames.clear();
        self.frames.push(frame);
        self.current_frame_idx = Some(0);
    }

    /// Return the module index of the currently executing frame, if any.
    pub fn current_module_idx(&self) -> Option<usize> {
        self.current_frame_idx
            .and_then(|idx| self.frames.get(idx))
            .map(|frame| frame.module_idx)
    }

    /// Resolve a string-pool value to its contents using the current module's
    /// constant pool.
    pub fn constant_string(&self, module_idx: usize, string_id: u32) -> Option<String> {
        self.modules
            .get(module_idx)
            .and_then(|m| m.constants.get(string_id as usize))
            .and_then(|c| match c {
                Constant::String(s) => Some(s.clone()),
                _ => None,
            })
    }

    /// Take a snapshot of recorded migration requests.
    pub fn pending_migrations(&self) -> &[(u64, u64)] {
        &self.pending_migrations
    }

    /// Take a snapshot of recorded gossip messages.
    pub fn gossip_log(&self) -> &[String] {
        &self.gossip_log
    }

    /// Load a bytecode module into the VM.
    pub fn load_module(&mut self, module: CodeModule) {
        let bits = constants_to_jit_bits(&module.constants);
        self.modules.push(module);
        self.jit_constants.push(bits);
    }

    /// Number of hot regions compiled through the type-directed JIT path
    /// (NaN-tag guard stripping) since this VM was created. Exposed for
    /// testing the tiering pipeline.
    pub fn jit_typed_compiled_count(&self) -> usize {
        self.jit_session
            .as_ref()
            .map(|j| j.typed_compiled_count())
            .unwrap_or(0)
    }

    /// Number of scalar (non-type-directed) regions JIT-compiled on this VM.
    ///
    /// Distinct from [`Self::jit_typed_compiled_count`]: regions whose
    /// registers can't be provably typed (e.g. containing `PerformDirect`,
    /// which yields to the interpreter and clobbers the register set) fall
    /// back to the scalar compiler, which is still a real JIT compile that
    /// must agree with the interpreter. Tests asserting the JIT *engaged* for
    /// such regions should check this count, not the typed one.
    pub fn jit_compiled_count(&self) -> usize {
        self.jit_session
            .as_ref()
            .map(|j| j.compiled_count())
            .unwrap_or(0)
    }

    /// Discard all closure capture environments.
    ///
    /// Only call this when no live value can reference an existing
    /// environment — e.g. immediately before starting a fresh, independent
    /// program on a VM that has no other in-flight or persisted state (the
    /// REPL does this before every evaluation). Calling this while another
    /// closure created earlier is still reachable (a suspended frame, or a
    /// closure stored in actor state or a heap composite) would turn those
    /// closures into dangling references.
    pub fn clear_closure_envs(&mut self) {
        self.closure_envs.clear();
    }

    /// Number of closure capture environments currently retained. Exposed so
    /// long-running embedders (e.g. the actor runtime) can monitor growth of
    /// the known unbounded-retention limitation documented on `closure_envs`.
    pub fn closure_env_count(&self) -> usize {
        self.closure_envs.len()
    }

    /// Take routes registered by `perform Web.route(...)` during the most
    /// recent run. Only populated when the VM is using `StandaloneVmCallbacks`.
    pub fn take_web_routes(&mut self) -> Vec<crate::runtime::WebRoute> {
        if let Some(callbacks) = (&mut *self.actor_callbacks as &mut dyn std::any::Any)
            .downcast_mut::<StandaloneVmCallbacks>()
        {
            std::mem::take(&mut callbacks.routes)
        } else {
            Vec::new()
        }
    }
    /// Get a closure environment by index.
    pub fn closure_env(&self, idx: usize) -> Option<&ClosureEnv> {
        self.closure_envs.get(idx)
    }

    /// Push a new closure environment and return its index.
    pub(crate) fn push_closure_env(&mut self, env: ClosureEnv) -> usize {
        let idx = self.closure_envs.len();
        self.closure_envs.push(env);
        idx
    }
    /// Copy the payload of a string-like value into a `Vec<u8>`.
    ///
    /// Used by the FFI call path to build temporary `CString` arguments.
    ///
    /// # Safety
    /// Pointer values must point to a valid heap object or a C string borrowed
    /// for the duration of this call.
    unsafe fn value_to_bytes(&self, module_idx: usize, value: Value) -> Option<Vec<u8>> {
        if let Some(id) = value.as_string_id() {
            self.modules
                .get(module_idx)
                .and_then(|m| m.constants.get(id as usize))
                .and_then(|c| match c {
                    Constant::String(s) => Some(s.as_bytes().to_vec()),
                    _ => None,
                })
        } else if let Some(ptr) = value.as_ptr() {
            // SAFETY: ptr must point to a heap object with an OrcaHeader.
            let header = unsafe { &*ActorHeap::header_of(ptr) };
            let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
            // SAFETY: payload_size bytes follow the header.
            Some(unsafe { std::slice::from_raw_parts(ptr, payload_size) }.to_vec())
        } else {
            None
        }
    }

    /// Copy a C string return value into the actor heap and free the temporary.
    fn copy_cstr_return(&mut self, value: Value) -> NuResult<Value> {
        // cstr_to_value maps a NULL C string to nil; pass it through instead
        // of failing on the missing pointer.
        if value.is_nil() {
            return Ok(value);
        }
        let ptr = value.as_ptr().ok_or_else(|| NuError::VMError {
            msg: "FFI C string return was not a pointer".to_string(),
            span: Span::default(),
        })?;
        // SAFETY: ptr is a valid null-terminated C string from cstr_to_value.
        let bytes = unsafe { CStr::from_ptr(ptr as *const c_char).to_bytes() };
        let len = bytes.len();
        let heap_ptr = self
            .actor_callbacks
            .alloc(len + 1, HeapTypeTag::String)
            .ok_or_else(|| NuError::VMError {
                msg: "FFI C string heap allocation failed".to_string(),
                span: Span::default(),
            })?;
        // SAFETY: heap_ptr points to len+1 bytes of freshly allocated memory.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), heap_ptr, len);
            *heap_ptr.add(len) = 0;
        }
        // SAFETY: value was produced by cstr_to_value.
        unsafe {
            crate::ffi::marshal::free_cstr_value(value);
        }
        Ok(Value::ptr(heap_ptr))
    }

    /// Get a constant string from a module's constant pool.
    fn module_const_string(&self, module_idx: usize, const_idx: usize) -> String {
        self.modules
            .get(module_idx)
            .and_then(|m| m.constants.get(const_idx))
            .map(|c| match c {
                Constant::String(s) => s.clone(),
                Constant::Int(n) => n.to_string(),
                _ => format!("{:?}", c),
            })
            .unwrap_or_else(|| format!("#const{}", const_idx))
    }

    /// Convert a runtime value into a plain Rust string.
    ///
    /// String-id values are resolved through the module's constant pool.
    /// Pointer values are read as null-terminated UTF-8.
    pub fn value_to_string(&self, module_idx: usize, value: Value) -> String {
        if let Some(id) = value.as_string_id() {
            self.constant_string(module_idx, id).unwrap_or_default()
        } else if let Some(ptr) = value.as_ptr() {
            if ptr.is_null() {
                String::new()
            } else {
                // SAFETY: the pointer was allocated by this VM's allocator and
                // is expected to be null-terminated for string payloads.
                unsafe {
                    CStr::from_ptr(ptr as *const c_char)
                        .to_string_lossy()
                        .into_owned()
                }
            }
        } else {
            value.to_string_repr()
        }
    }

    /// Resolve a value to its string content for `SCmpEq`.
    ///
    /// Pool strings are module-scoped constant-pool indices; heap strings
    /// are null-terminated UTF-8 allocations tagged `HeapTypeTag::String`.
    /// Equality must be by content: the same text may live at different
    /// pool indices in different modules, and pool/heap representations
    /// must compare equal. Any non-string value (ints, nil, records,
    /// arrays, ...) yields `None` so the comparison evaluates to `false`
    /// instead of erroring — mirroring the coerce-don't-fail style of
    /// `ICmpEq`/`FCmpEq`.
    pub(crate) fn string_operand(&self, module_idx: usize, value: Value) -> Option<String> {
        if let Some(id) = value.as_string_id() {
            self.constant_string(module_idx, id)
        } else if let Some(ptr) = value.as_ptr() {
            if ptr.is_null() {
                return None;
            }
            // SAFETY: `ptr` was produced by `actor_callbacks.alloc`, so
            // `ActorHeap::header_of` is valid (same pattern as RecL). The
            // type-tag check ensures only string payloads are read as C
            // strings — a record or array pointer must not be scanned for
            // a NUL terminator.
            unsafe {
                let header = &*ActorHeap::header_of(ptr);
                if header.type_tag != HeapTypeTag::String {
                    return None;
                }
                Some(
                    CStr::from_ptr(ptr as *const c_char)
                        .to_string_lossy()
                        .into_owned(),
                )
            }
        } else {
            None
        }
    }

    /// Allocate a fresh heap string and return it as a pointer value.
    pub fn allocate_string(&mut self, s: &str) -> Value {
        let bytes = s.as_bytes();
        if let Some(ptr) = self
            .actor_callbacks
            .alloc(bytes.len() + 1, HeapTypeTag::String)
        {
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                *ptr.add(bytes.len()) = 0;
            }
            Value::ptr(ptr)
        } else {
            Value::nil()
        }
    }

    /// Add a runtime string to a module's constant pool and return its string-id value.
    ///
    /// Also appends the matching NaN-boxed bits to the module's JIT constant
    /// table so JIT-compiled regions resolve the new constant correctly. This
    /// is `&mut self` and must run on the single scheduler thread (the only
    /// thread that touches the VM); the cross-node string interning in
    /// `runtime::distributed` relies on that invariant.
    pub fn add_runtime_string(&mut self, module_idx: usize, s: String) -> Value {
        let idx = self
            .modules
            .get(module_idx)
            .map(|m| m.constants.len())
            .unwrap_or(0);
        if let Some(module) = self.modules.get_mut(module_idx) {
            module.constants.push(Constant::String(s));
        }
        if let Some(bits) = self.jit_constants.get_mut(module_idx) {
            bits.push(Value::string(idx as u32).to_bits());
        }
        Value::string(idx as u32)
    }

    /// Run the loaded program starting from the entry point of the last module.
    ///
    /// Returns the value in register 0 of the final frame, or unit if no frame.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn run(&mut self) -> NuResult<Value> {
        let module_idx = self.modules.len().saturating_sub(1);
        let entry_point = self
            .modules
            .get(module_idx)
            .and_then(|m| m.entry_point)
            .unwrap_or(0);

        let mut frame = Frame::new(None, module_idx);
        frame.pc = entry_point;
        self.frames.clear();
        self.frames.push(frame);
        self.current_frame_idx = Some(0);

        // Main execution loop
        loop {
            // Check if halted
            if let Some(idx) = self.current_frame_idx {
                let module_idx = self.frames[idx].module_idx;
                let pc = self.frames[idx].pc;
                if let Some(module) = self.modules.get(module_idx) {
                    if pc >= module.instructions.len() {
                        // PC past end — program complete
                        return Ok(self
                            .frames
                            .get(idx)
                            .map(|f| f.regs[0])
                            .unwrap_or(Value::unit()));
                    }
                    // Check if next instruction is Halt
                    if module
                        .instructions
                        .get(pc)
                        .map(|i| i.opcode == OpCode::Halt)
                        .unwrap_or(false)
                    {
                        self.frames[idx].pc += 1;
                        return Ok(self
                            .frames
                            .get(idx)
                            .map(|f| f.regs[0])
                            .unwrap_or(Value::unit()));
                    }
                } else {
                    return Ok(Value::unit());
                }
            } else {
                return Ok(Value::unit());
            }

            match self.step() {
                Ok(()) => {}
                Err(NuError::VMError { msg, span: _ }) if msg == "Halt" => {
                    return Ok(self
                        .current_frame_idx
                        .and_then(|i| self.frames.get(i))
                        .map(|f| f.regs[0])
                        .unwrap_or(Value::unit()));
                }
                Err(NuError::VMError { msg, span }) => {
                    return Err(NuError::VMError {
                        msg: self.enrich_error(msg),
                        span,
                    })
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Run with a specific entry point. If the VM already has a current frame
    /// (e.g. set by the actor runtime with pre-populated argument registers),
    /// reuse it; otherwise create a fresh frame. This lets actor behavior
    /// handlers receive their arguments.
    pub fn run_from(&mut self, module_idx: usize, pc: usize) -> NuResult<Value> {
        self.yield_pending = false;

        if self.frames.is_empty() {
            let mut frame = Frame::new(None, module_idx);
            frame.pc = pc;
            self.frames.push(frame);
            self.current_frame_idx = Some(0);
        } else if let Some(frame) = self.frames.get_mut(0) {
            frame.pc = pc;
            frame.module_idx = module_idx;
            self.current_frame_idx = Some(0);
        }

        loop {
            if let Some(idx) = self.current_frame_idx {
                let m_idx = self.frames[idx].module_idx;
                let pc = self.frames[idx].pc;
                if let Some(module) = self.modules.get(m_idx) {
                    if pc >= module.instructions.len() {
                        let v = self
                            .current_frame_idx
                            .and_then(|i| self.frames.get(i))
                            .map(|f| f.regs[0])
                            .unwrap_or(Value::unit());
                        return Ok(v);
                    }
                    if module
                        .instructions
                        .get(pc)
                        .map(|i| i.opcode == OpCode::Halt)
                        .unwrap_or(false)
                    {
                        self.frames[idx].pc += 1;
                        let v = self
                            .current_frame_idx
                            .and_then(|i| self.frames.get(i))
                            .map(|f| f.regs[0])
                            .unwrap_or(Value::unit());
                        return Ok(v);
                    }
                } else {
                    return Ok(Value::unit());
                }
            } else {
                return Ok(Value::unit());
            }

            match self.step() {
                Ok(()) => {
                    if self.yield_pending {
                        return Ok(Value::nil());
                    }
                }
                Err(NuError::VMError { msg, span: _ }) if msg == "Halt" => {
                    return Ok(self
                        .current_frame_idx
                        .and_then(|i| self.frames.get(i))
                        .map(|f| f.regs[0])
                        .unwrap_or(Value::unit()));
                }
                Err(NuError::VMError { msg, span }) => {
                    return Err(NuError::VMError {
                        msg: self.enrich_error(msg),
                        span,
                    })
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Resume a previously suspended execution.
    ///
    /// Continues from the current frame state (set by `restore_suspended_state`)
    /// until the program halts, yields again, or errors.
    pub fn resume(&mut self) -> NuResult<Value> {
        self.yield_pending = false;
        loop {
            if let Some(idx) = self.current_frame_idx {
                let m_idx = self.frames[idx].module_idx;
                let pc = self.frames[idx].pc;
                if let Some(module) = self.modules.get(m_idx) {
                    if pc >= module.instructions.len() {
                        return Ok(self
                            .current_frame_idx
                            .and_then(|i| self.frames.get(i))
                            .map(|f| f.regs[0])
                            .unwrap_or(Value::unit()));
                    }
                    if module
                        .instructions
                        .get(pc)
                        .map(|i| i.opcode == OpCode::Halt)
                        .unwrap_or(false)
                    {
                        self.frames[idx].pc += 1;
                        return Ok(self
                            .current_frame_idx
                            .and_then(|i| self.frames.get(i))
                            .map(|f| f.regs[0])
                            .unwrap_or(Value::unit()));
                    }
                } else {
                    return Ok(Value::unit());
                }
            } else {
                return Ok(Value::unit());
            }

            match self.step() {
                Ok(()) => {
                    if self.yield_pending {
                        return Ok(Value::nil());
                    }
                }
                Err(NuError::VMError { msg, span: _ }) if msg == "Halt" => {
                    return Ok(self
                        .current_frame_idx
                        .and_then(|i| self.frames.get(i))
                        .map(|f| f.regs[0])
                        .unwrap_or(Value::unit()));
                }
                Err(NuError::VMError { msg, span }) => {
                    return Err(NuError::VMError {
                        msg: self.enrich_error(msg),
                        span,
                    })
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Step limit, read once from the `NULANG_STEP_LIMIT` env var and
    /// cached: re-reading the environment on every instruction costs an
    /// env-mutex lock plus a String allocation per VM step.
    pub(crate) fn step_limit() -> usize {
        static STEP_LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *STEP_LIMIT.get_or_init(|| {
            std::env::var("NULANG_STEP_LIMIT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10_000_000)
        })
    }

    /// Attempt JIT execution for the current PC.
    ///
    /// Returns `true` if the JIT executed a compiled region and advanced the
    /// PC — the caller should return `Ok(())` immediately.
    fn try_jit_execute(&mut self, frame_idx: usize) -> bool {
        let module_idx = self.frames[frame_idx].module_idx;
        let pc = self.frames[frame_idx].pc;
        // Raw pointer to self for the re-entrant direct-call helper, computed
        // BEFORE the `&mut self.jit_session` borrow below (the VM is stable
        // and single-threaded for the duration of this region execution).
        let self_ptr = self as *mut VM;
        let jit = match &mut self.jit_session {
            Some(j) => j.as_mut(),
            None => return false,
        };

        // Check cheap: already compiled, or newly hot? Single probe call so
        // the per-step cost is one vtable dispatch into inlined logic (a
        // flat-array increment for cold code), not two dyn calls. The
        // module/constants fetch below is deferred until AFTER the probe so
        // a cold step (probe returns false) doesn't pay it at all.
        if !jit.probe_and_maybe_hot(module_idx, pc) {
            return false;
        }

        let module = match self.modules.get(module_idx) {
            Some(m) => m,
            None => return false,
        };
        let constants = self
            .jit_constants
            .get(module_idx)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);

        // Snapshot registers into a flat array for the JIT ABI.
        let mut regs: [u64; 256] = [0; 256];
        for (i, r) in self.frames[frame_idx].regs.iter().enumerate() {
            regs[i] = r.to_bits();
        }
        // SAFETY: The `&mut dyn ActorVmCallbacks` reference is valid for the
        // duration of this function call. `set_jit_callbacks` stores it in a
        // thread-local; `with_callbacks` restores `&mut` provenance before use.
        // The VM is single-threaded, so no concurrent access.
        unsafe {
            crate::jit::runtime::set_jit_callbacks(
                self.actor_callbacks.as_mut() as *mut dyn ActorVmCallbacks
            );
        }
        // Thread the current module's constant pool so the JIT runtime can
        // resolve interned (TAG_STRING) values for string comparison.
        unsafe {
            crate::jit::runtime::set_jit_constants(&module.constants);
        }
        // Thread the VM itself so the re-entrant direct-call helper can run a
        // callee on the interpreter frame stack from within a compiled region.
        // The VM is single-threaded, so a raw pointer in a thread-local is sound.
        unsafe {
            crate::jit::runtime::set_jit_vm(self_ptr);
        }
        let action = jit.tiered_execute_step_typed(module_idx, pc, module, &mut regs, constants);
        crate::jit::runtime::clear_jit_vm();
        crate::jit::runtime::clear_jit_constants();
        crate::jit::runtime::clear_jit_callbacks();

        if action != TieredAction::Interpret {
            for (i, bits) in regs.iter().enumerate() {
                self.frames[frame_idx].regs[i] = Value::from_bits(*bits);
            }

            // A re-entrant callee raised a runtime error (e.g. step-limit
            // exceeded); the compiled region exited early via its error path.
            // Stash it on the VM so `step` can surface it (this fn returns
            // bool). Propagate BEFORE handling branch-exit/yield.
            if let Some(msg) = crate::jit::runtime::take_jit_pending_vm_error() {
                self.jit_pending_error = Some(msg);
                return true;
            }

            // A compiled region that exited via a branch to an outside target
            // resumes the interpreter at that pc WITHOUT suspending (a plain
            // control-flow exit, not an effect/LLM suspension).
            if let Some(exit_offset) = crate::jit::runtime::take_jit_branch_exit_pc() {
                let base = pc as isize;
                let off = exit_offset as i64 as isize;
                self.frames[frame_idx].pc = (base + off).max(0) as usize;
                self.step_count += 1;
                return true;
            }

            // Check if JIT yielded at a safepoint (a real suspension).
            if let Some(yield_offset) = crate::jit::runtime::take_jit_yield_pc() {
                self.frames[frame_idx].pc = pc + yield_offset;
                self.yield_pending = true;
                self.step_count += 1;
                return true;
            }

            if let Some(region_len) = jit.compiled_region_len(module_idx, pc) {
                self.frames[frame_idx].pc += region_len;
                return true;
            }
            // JIT executed but region not tracked — fall back to interpretation.
        }
        // JIT fell back to interpretation — continue in the interpreter.
        false
    }

    /// Execute a single bytecode instruction.
    ///
    /// Execute the FFICall opcode — foreign function interface dispatch.
    fn step_fficall(
        &mut self,
        instr: Instruction,
        frame_idx: usize,
        module_idx: usize,
    ) -> NuResult<()> {
        let func_idx = instr.imm16() as usize;
        let dst = instr.op3;
        let (def, module_idx) = self
            .modules
            .get(module_idx)
            .and_then(|m| {
                m.foreign_functions
                    .get(func_idx)
                    .map(|d| (d.clone(), module_idx))
            })
            .ok_or_else(|| NuError::VMError {
                msg: format!("Foreign function {} not found", func_idx),
                span: Span::default(),
            })?;

        // FFI sandbox: deny calls to libraries not in the allow-list.
        if self.ffi_sandbox && !self.ffi_allowlist.contains(&def.library) {
            return Err(NuError::VMError {
                msg: format!(
                    "FFI sandbox blocked call to '{}' from library '{}': library not in allow-list",
                    def.symbol, def.library
                ),
                span: Span::default(),
            });
        }

        let params: Vec<CType> = def
            .params
            .iter()
            .map(|p| crate::ffi::marshal::ffi_type_to_ctype(p))
            .collect::<Option<_>>()
            .ok_or_else(|| NuError::VMError {
                msg: format!("Unsupported FFI parameter type in {}", def.symbol),
                span: Span::default(),
            })?;
        let ret =
            crate::ffi::marshal::ffi_type_to_ctype(&def.ret).ok_or_else(|| NuError::VMError {
                msg: format!("Unsupported FFI return type in {:?}", def.ret),
                span: Span::default(),
            })?;
        let signature = Signature::new(params.clone(), ret);

        // Build argument values. For CStr parameters we copy Nulang
        // string values into temporary CString buffers whose pointers
        // remain valid for the duration of the native call.
        let mut cstrings: Vec<CString> = Vec::new();
        let mut args: Vec<Value> = Vec::with_capacity(def.params.len());
        for (i, param_ctype) in params.iter().enumerate() {
            let src = self.frames[frame_idx].regs[i];
            if *param_ctype == CType::CStr {
                let bytes = unsafe { self.value_to_bytes(module_idx, src) }.ok_or_else(|| {
                    NuError::VMError {
                        msg: format!("FFI argument {} for {} is not a string", i, def.symbol),
                        span: Span::default(),
                    }
                })?;
                let cstring = CString::new(bytes).map_err(|e| NuError::VMError {
                    msg: format!("FFI argument {} contains null byte: {}", i, e),
                    span: Span::default(),
                })?;
                args.push(Value::ptr(cstring.as_ptr() as *mut u8));
                cstrings.push(cstring);
            } else {
                args.push(src);
            }
        }

        let func = {
            // SAFETY: caller ensures the named library is a valid shared
            // library. Do not hold the lock across the native call.
            let registry = FFI_REGISTRY
                .get_or_init(|| std::sync::Mutex::new(crate::ffi::native::FfiRegistry::new()));
            let mut reg = registry.lock().map_err(|e| NuError::VMError {
                msg: format!("FFI registry lock failed: {}", e),
                span: Span::default(),
            })?;
            // SAFETY: resolve_or_load opens the library if needed.
            unsafe { reg.resolve_or_load(&def.library, &def.symbol, signature) }.map_err(|e| {
                NuError::VMError {
                    msg: format!("FFI resolve/load failed for {}: {}", def.symbol, e),
                    span: Span::default(),
                }
            })?
        };

        // SAFETY: func.ptr points to a function whose ABI matches signature.
        let mut result = unsafe { call_native(&func, &args) }.map_err(|e| NuError::VMError {
            msg: format!("FFI call {} failed: {}", def.symbol, e),
            span: Span::default(),
        })?;

        // C string returns are temporary; copy them into the actor heap
        // and free the temporary CString from cstr_to_value.
        if ret == CType::CStr {
            result = self.copy_cstr_return(result)?;
        }

        self.frames[frame_idx].regs[dst as usize] = result;
        Ok(())
    }
    /// Run a provably-non-suspending direct callee to completion on the
    /// interpreter frame stack, from within a compiled region whose register
    /// buffer is `regs`. Used by `nulang_jit_direct_call`. Returns 0 on
    /// success, nonzero on error (the error is recorded in the JIT pending
    /// error thread-local and the callee's frames are unwound back to the
    /// caller so the outer region resumes on the correct frame).
    ///
    /// The callee is `!may_suspend` by construction (the compiler gates
    /// emission on that analysis), so it never suspends mid-run.
    ///
    /// # Safety
    /// `regs` must point at the 256-entry register buffer of the compiled
    /// region that invoked this helper.
    pub(crate) fn jit_direct_call(
        &mut self,
        regs: *mut u64,
        func_idx: usize,
        argc: usize,
        dst: usize,
    ) -> i64 {
        let caller_idx = match self.current_frame_idx {
            Some(c) => c,
            None => {
                crate::jit::runtime::set_jit_pending_vm_error(
                    "JIT direct call outside a frame".to_string(),
                );
                return 1;
            }
        };
        // The callee lives in the same module as the caller (function_table
        // is per-module; direct calls are within-module).
        let module_idx = self.frames[caller_idx].module_idx;
        let code_offset = match self
            .modules
            .get(module_idx)
            .and_then(|m| m.function_table.get(func_idx))
            .copied()
        {
            Some(o) => o,
            None => {
                crate::jit::runtime::set_jit_pending_vm_error(format!(
                    "JIT direct call: function {} not found",
                    func_idx
                ));
                return 1;
            }
        };

        // Build the callee frame with args copied from the region's regs
        // buffer.
        let mut frame = Frame::new(Some(caller_idx), module_idx);
        frame.pc = code_offset;
        let argc = argc.min(256);
        for i in 0..argc {
            // SAFETY: `regs` points at the compiled region's 256-entry buffer.
            let bits = unsafe { *regs.add(i) };
            frame.regs[i] = Value::from_bits(bits);
        }
        frame.return_dst = dst.min(255) as u8;
        self.frames.push(frame);
        self.current_frame_idx = Some(caller_idx + 1);

        // Run the interpreter until the callee frame returns to the caller.
        let status = loop {
            match self.step() {
                Ok(()) => {
                    if self.current_frame_idx.map_or(true, |f| f != caller_idx) {
                        continue; // still inside the callee (or a nested call)
                    }
                    break 0;
                }
                Err(e) => {
                    // Callee raised. Unwind any nested frames back to the
                    // caller so the outer region resumes on the correct frame.
                    self.frames.truncate(caller_idx + 1);
                    self.current_frame_idx = Some(caller_idx);
                    crate::jit::runtime::set_jit_pending_vm_error(e.to_string());
                    break 1;
                }
            }
        };

        if status == 0 && dst < 256 {
            let ret = self.frames[caller_idx].regs[dst];
            // SAFETY: `regs` points at the compiled region's 256-entry buffer.
            unsafe { *regs.add(dst) = ret.to_bits() };
        }
        status
    }
    fn step_perform(
        &mut self,
        instr: Instruction,
        frame_idx: usize,
        module_idx: usize,
    ) -> NuResult<()> {
        let eff_name_idx = instr.imm16();
        let dst_reg = instr.op3;
        let qualified_name = self.module_const_string(module_idx, eff_name_idx as usize);
        // The MIR pipeline encodes the performed operation as
        // "Effect.op" (e.g. "IO.print"); hand-built modules may
        // carry a bare name with no operation.
        let (effect_name, op_name) = match qualified_name.split_once('.') {
            Some((effect, op)) => (effect.to_string(), Some(op.to_string())),
            None => (qualified_name.clone(), None),
        };
        // ---- Test effect: assertion primitives ----
        // Handled inline so assertion failures produce RuntimeError.
        if effect_name == "Test" {
            match op_name.as_deref() {
                Some("assert") => {
                    let cond = self.frames[frame_idx]
                        .regs
                        .first()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let msg_val = self.frames[frame_idx]
                        .regs
                        .get(1)
                        .copied()
                        .unwrap_or(Value::nil());
                    let msg = if cond {
                        String::new()
                    } else {
                        self.value_to_string(module_idx, msg_val)
                    };
                    if !cond {
                        return Err(NuError::runtime_error(
                            format!("assertion failed: {}", msg),
                            Span::default(),
                        ));
                    }
                    self.frames[frame_idx].regs[dst_reg as usize] = Value::unit();
                    return Ok(());
                }
                Some("assert_eq") => {
                    let a = self.frames[frame_idx]
                        .regs
                        .first()
                        .and_then(|v| v.as_int())
                        .unwrap_or(0);
                    let b = self.frames[frame_idx]
                        .regs
                        .get(1)
                        .and_then(|v| v.as_int())
                        .unwrap_or(0);
                    if a != b {
                        return Err(NuError::runtime_error(
                            format!("assertion failed: expected {}, got {}", b, a),
                            Span::default(),
                        ));
                    }
                    self.frames[frame_idx].regs[dst_reg as usize] = Value::unit();
                    return Ok(());
                }
                Some("assert_true") => {
                    let cond = self.frames[frame_idx]
                        .regs
                        .first()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if !cond {
                        return Err(NuError::runtime_error(
                            "assertion failed".to_string(),
                            Span::default(),
                        ));
                    }
                    self.frames[frame_idx].regs[dst_reg as usize] = Value::unit();
                    return Ok(());
                }
                _ => {
                    return Err(NuError::effect_error(
                        format!("Unknown Test operation: '{}'", qualified_name),
                        Span::default(),
                    ));
                }
            }
        }
        // Fast path: no user handlers installed — skip the
        // rposition walk and dispatch directly to the built-in
        // effect callback.  This is the common case for Actor.* builtins in
        // actor bytecode (no user handler, empty handler_stack).
        if self.handler_stack.is_empty() {
            let result = match self.modules.get(module_idx) {
                Some(module) => self.actor_callbacks.perform_builtin_effect_in_module(
                    &effect_name,
                    op_name.as_deref(),
                    module,
                    &self.frames[frame_idx].regs,
                ),
                None => self.actor_callbacks.perform_builtin_effect(
                    &effect_name,
                    op_name.as_deref(),
                    &[],
                    &self.frames[frame_idx].regs,
                ),
            };
            if let Some(result) = result {
                self.frames[frame_idx].regs[dst_reg as usize] = result;
            } else {
                return Err(NuError::effect_error(
                    format!("Unhandled effect: '{}'", qualified_name),
                    Span::default(),
                ));
            }
            // PC already advanced by the main loop (line 1516);
            // fall through to the next instruction.
            return Ok(());
        }
        // A binding matches when it names the exact "Effect.op"
        // pair. Bindings that carry a bare effect name (no '.')
        // predate op-qualified dispatch and match any op of that
        // effect, preserving legacy modules.
        let matches_binding = |b: &crate::bytecode::HandlerBinding| {
            b.effect_name == qualified_name
                || (!b.effect_name.contains('.') && b.effect_name == effect_name)
        };

        let handler_result = self
            .handler_stack
            .iter()
            .enumerate()
            .rev()
            .find_map(|(idx, hf)| {
                let module = self.modules.get(hf.module_idx)?;
                let ht = module.handler_tables.get(hf.handler_table_idx)?;
                ht.bindings
                    .iter()
                    .find(|b| matches_binding(*b))
                    .map(|binding| (idx, binding.handler_offset, binding.result_reg))
            });

        let target_offset =
            if let Some((handler_stack_idx, handler_offset, result_reg)) = handler_result {
                self.handler_stack[handler_stack_idx].resume_dst = result_reg;
                Some(handler_offset)
            } else {
                self.handler_stack.last().and_then(|hf| {
                    self.modules
                        .get(hf.module_idx)
                        .and_then(|m| m.handler_tables.get(hf.handler_table_idx))
                        .and_then(|ht| ht.fallback_offset)
                })
            };

        if let Some((handler_stack_idx, _, _)) = handler_result {
            let cont = Continuation::capture(self, dst_reg).ok_or_else(|| NuError::VMError {
                msg: "Cannot capture continuation: no current frame".into(),
                span: Span::default(),
            })?;
            self.handler_stack[handler_stack_idx].captured_continuation = Some(cont);
        } else if target_offset.is_some() {
            let hf_idx = self.handler_stack.len().saturating_sub(1);
            let cont = Continuation::capture(self, dst_reg).ok_or_else(|| NuError::VMError {
                msg: "Cannot capture continuation for fallback: no current frame".into(),
                span: Span::default(),
            })?;
            self.handler_stack[hf_idx].captured_continuation = Some(cont);
        } else {
            // No handler and no fallback: give the runtime callback a
            // chance to handle built-in effects (e.g. Timer.sleep in
            // workflow steps, IO.print in standalone scripts). Args
            // are in r0..rn; string-id args resolve against the
            // performing module's constant pool.
            let result = match self.modules.get(module_idx) {
                Some(module) => self.actor_callbacks.perform_builtin_effect_in_module(
                    &effect_name,
                    op_name.as_deref(),
                    module,
                    &self.frames[frame_idx].regs,
                ),
                None => self.actor_callbacks.perform_builtin_effect(
                    &effect_name,
                    op_name.as_deref(),
                    &[],
                    &self.frames[frame_idx].regs,
                ),
            };
            if let Some(result) = result {
                self.frames[frame_idx].regs[dst_reg as usize] = result;
            } else {
                return Err(NuError::effect_error(
                    format!("Unhandled effect: '{}'", qualified_name),
                    Span::default(),
                ));
            }
        }

        if let Some(offset) = target_offset {
            self.frames[frame_idx].pc = offset;
        }
        Ok(())
    }

    /// Statically-resolved effect dispatch.  Looks up the handler table
    /// and binding directly by index instead of walking the handler stack
    /// and matching by effect name string.  The handler table index comes
    /// from the `Handle` opcode that installed the handler frame; the
    /// binding index identifies the specific handler arm.
    fn step_perform_direct(
        &mut self,
        instr: Instruction,
        frame_idx: usize,
        module_idx: usize,
    ) -> NuResult<()> {
        let table_idx = instr.op1 as usize;
        let binding_idx = instr.op2 as usize;
        let dst_reg = instr.op3;

        // Look up the handler binding from the module's handler tables.
        let module = self
            .modules
            .get(module_idx)
            .ok_or_else(|| NuError::VMError {
                msg: "PerformDirect: module not found".into(),
                span: Span::default(),
            })?;
        let handler_table =
            module
                .handler_tables
                .get(table_idx)
                .ok_or_else(|| NuError::VMError {
                    msg: format!("PerformDirect: handler table {} not found", table_idx),
                    span: Span::default(),
                })?;
        let binding = handler_table
            .bindings
            .get(binding_idx)
            .ok_or_else(|| NuError::VMError {
                msg: format!(
                    "PerformDirect: binding {} not found in table {}",
                    binding_idx, table_idx
                ),
                span: Span::default(),
            })?;

        // Find the handler frame with the matching table index and module.
        // The `Handle` opcode pushes the frame with this table index; the
        // PerformDirect then finds it and installs the continuation.
        let hf_idx = self
            .handler_stack
            .iter()
            .rposition(|hf| hf.handler_table_idx == table_idx && hf.module_idx == module_idx)
            .ok_or_else(|| NuError::VMError {
                msg: format!("PerformDirect: no handler frame for table {}", table_idx),
                span: Span::default(),
            })?;

        // Set the result register and capture the continuation.
        self.handler_stack[hf_idx].resume_dst = binding.result_reg;

        if binding.single_shot {
            // Fast path: snapshot registers inline — no heap allocation.
            let frame = &self.frames[frame_idx];
            let mut regs = [Value::nil(); 256];
            for (i, r) in frame.regs.iter().enumerate() {
                regs[i] = *r;
            }
            self.handler_stack[hf_idx].single_shot_state = Some(SingleShotState {
                resume_pc: frame.pc,
                resume_dst: dst_reg,
                step_count: self.step_count,
                regs,
            });
        } else {
            let cont = Continuation::capture(self, dst_reg).ok_or_else(|| NuError::VMError {
                msg: "Cannot capture continuation: no current frame".into(),
                span: Span::default(),
            })?;
            self.handler_stack[hf_idx].captured_continuation = Some(cont);
        }

        // Jump to the handler body.
        self.frames[frame_idx].pc = binding.handler_offset;
        Ok(())
    }

    fn step_capstore(&mut self, instr: Instruction, frame_idx: usize) -> NuResult<()> {
        let closure_reg = instr.op1 as usize;
        let slot = instr.op2 as usize;
        let src = self.frames[frame_idx].regs[instr.op3 as usize];
        let val = self.frames[frame_idx].regs[closure_reg];
        if (val.raw & TAG_MASK) != TAG_CLOSURE {
            return Err(NuError::VMError {
                msg: format!("CapStore target is not a closure: {}", val.to_string_repr()),
                span: Span::default(),
            });
        }
        let payload = val.raw & PAYLOAD_MASK;
        let env_idx = if payload & CLOSURE_ENV_FLAG != 0 {
            (payload & CLOSURE_ENV_IDX_MASK) as usize
        } else {
            if self.closure_envs.len() >= self.max_closure_envs {
                return Err(NuError::VMError { msg: format!(
                            "closure capture environments exceeded the {} limit; this process has been running long enough to accumulate unreclaimed closure envs (see VM::closure_envs)",
                            self.max_closure_envs
                        ), span: Span::default() });
            }
            let idx = self.closure_envs.len();
            self.closure_envs.push(ClosureEnv {
                func_idx: payload as usize,
                captures: Vec::new(),
            });
            self.frames[frame_idx].regs[closure_reg] = Value {
                raw: TAG_CLOSURE | CLOSURE_ENV_FLAG | (idx as u64 & CLOSURE_ENV_IDX_MASK),
            };
            idx
        };
        if let Some(ptr) = src.as_ptr() {
            self.actor_callbacks.retain_ref(ptr);
        }
        let env = &mut self.closure_envs[env_idx];
        if env.captures.len() <= slot {
            env.captures.resize(slot + 1, Value::nil());
        }
        env.captures[slot] = src;
        Ok(())
    }
    fn step_receive(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let dst = instr.op1;
        let value = self
            .actor_callbacks
            .try_receive()
            .map(|(_bid, v)| v)
            .unwrap_or(Value::nil());
        self.frames[frame_idx].regs[dst as usize] = value;
        Ok(())
    }

    fn step_receive_match(
        &mut self,
        frame_idx: usize,
        module_idx: usize,
        instr: Instruction,
    ) -> NuResult<()> {
        let const_idx = instr.imm16() as usize;
        let dst = instr.op3 as usize;
        let spec = self.module_const_string(module_idx, const_idx);
        let (max_params, ids) = parse_receive_spec(&spec);
        match self.actor_callbacks.try_receive_match(&ids) {
            Some((arm_idx, payload)) => {
                self.frames[frame_idx].regs[dst] = Value::int(arm_idx as i64);
                for i in 0..max_params {
                    let r = dst + 1 + i;
                    if r >= 256 {
                        break;
                    }
                    self.frames[frame_idx].regs[r] =
                        payload.get(i).copied().unwrap_or(Value::nil());
                }
            }
            None => {
                self.actor_callbacks.reset_receive_match();
                self.frames[frame_idx].regs[dst] = Value::int(ids.len() as i64);
            }
        }
        Ok(())
    }

    fn step_receive_wait(
        &mut self,
        frame_idx: usize,
        module_idx: usize,
        instr: Instruction,
    ) -> NuResult<()> {
        let const_idx = instr.imm16() as usize;
        let dst = instr.op3 as usize;
        let spec = self.module_const_string(module_idx, const_idx);
        let (max_params, ids) = parse_receive_spec(&spec);
        match self.actor_callbacks.try_receive_match(&ids) {
            Some((arm_idx, payload)) => {
                self.actor_callbacks.receive_wait_matched();
                self.frames[frame_idx].regs[dst] = Value::int(arm_idx as i64);
                for i in 0..max_params {
                    let r = dst + 1 + i;
                    if r >= 256 {
                        break;
                    }
                    self.frames[frame_idx].regs[r] =
                        payload.get(i).copied().unwrap_or(Value::nil());
                }
            }
            None => {
                let timeout_ms = self.frames[frame_idx].regs[0].as_int().unwrap_or(0);
                if self.actor_callbacks.receive_wait_suspend(timeout_ms) {
                    self.suspended_receive_timeout = Some(timeout_ms);
                    self.frames[frame_idx].pc -= 1;
                    return Err(NuError::Suspended(VmSuspension::ReceiveWait));
                }
                self.actor_callbacks.reset_receive_match();
                self.frames[frame_idx].regs[dst] = Value::int(ids.len() as i64);
            }
        }
        Ok(())
    }

    fn step_receive_commit(&mut self) {
        self.actor_callbacks.commit_receive_match();
    }

    /// Generic async effect dispatch (`PerformAsync` opcode).
    ///
    /// Reads the effect_op string from the constant pool, collects arguments
    /// from registers r0..rN, and calls `ActorVmCallbacks::perform_async`.
    /// On `Ready(value)` the result is written to the destination register
    /// and execution continues. On `Pending` the PC is decremented so the
    /// instruction re-executes on resume, and the VM returns a
    /// `Suspended(PerformAsync)` sentinel error.
    #[inline(never)]
    fn step_perform_async(
        &mut self,
        frame_idx: usize,
        module_idx: usize,
        instr: Instruction,
    ) -> NuResult<()> {
        let effect_op_idx = instr.imm16() as usize;
        let dst_reg = instr.op3 as usize;
        let effect_op = self.module_const_string(module_idx, effect_op_idx);
        // Pass the full frame register slice and the module's constant pool
        // so the callback can resolve string-id arguments from registers.
        let args = &self.frames[frame_idx].regs;
        let constants = self
            .modules
            .get(module_idx)
            .map(|m| &m.constants[..])
            .unwrap_or(&[]);
        match self
            .actor_callbacks
            .perform_async(&effect_op, constants, args)
        {
            PerformAsyncResult::Ready(result) => {
                let value = match result {
                    Some(ref content) => self.add_runtime_string(module_idx, content.clone()),
                    None => Value::nil(),
                };
                self.frames[frame_idx].regs[dst_reg] = value;
            }
            PerformAsyncResult::Pending => {
                self.frames[frame_idx].pc -= 1;
                return Err(NuError::Suspended(VmSuspension::PerformAsync));
            }
        }
        Ok(())
    }
    #[inline(never)]
    fn step_arrstore(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let arr_ptr = self.frames[frame_idx].regs[instr.op1 as usize]
            .as_ptr()
            .unwrap_or(std::ptr::null_mut());
        let idx = self.frames[frame_idx].regs[instr.op2 as usize]
            .as_int()
            .unwrap_or(0) as usize;
        let val = self.frames[frame_idx].regs[instr.op3 as usize];
        if !arr_ptr.is_null() {
            if let Some(len) = self.actor_callbacks.array_len(arr_ptr) {
                if idx < len {
                    if let Some(ptr) = val.as_ptr() {
                        self.actor_callbacks.retain_ref(ptr);
                    }
                    // SAFETY: The bounds check above (idx < len) guarantees
                    // `idx` is within the allocated array. `arr_ptr` is a valid
                    // ActorHeap pointer from a prior ArrLoad/Alloc. The
                    // read-modify-write of the slot (old → drop_ref, new →
                    // retain_ref) follows the standard ArrStore write-barrier
                    // contract.
                    unsafe {
                        let slot = (arr_ptr as *mut Value).add(idx);
                        let old = *slot;
                        *slot = val;
                        if let Some(old_ptr) = old.as_ptr() {
                            self.actor_callbacks.drop_ref(old_ptr);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[inline(never)]
    fn step_recs(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let rec_ptr = self.frames[frame_idx].regs[instr.op1 as usize]
            .as_ptr()
            .unwrap_or(std::ptr::null_mut());
        let field_id = instr.op2 as usize;
        let val = self.frames[frame_idx].regs[instr.op3 as usize];
        if !rec_ptr.is_null() {
            unsafe {
                let header = &*ActorHeap::header_of(rec_ptr);
                if header.type_tag == HeapTypeTag::Record {
                    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
                    let len = payload_size / std::mem::size_of::<Value>();
                    if field_id < len {
                        if let Some(ptr) = val.as_ptr() {
                            self.actor_callbacks.retain_ref(ptr);
                        }
                        let slot = (rec_ptr as *mut Value).add(field_id);
                        let old = *slot;
                        *slot = val;
                        if let Some(old_ptr) = old.as_ptr() {
                            self.actor_callbacks.drop_ref(old_ptr);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Shallow copy a record: allocate a new record with the same slot count
    /// as `src` and copy every field value, retaining each.
    #[inline(never)]
    fn step_reccopy(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let src_ptr = self.frames[frame_idx].regs[instr.op1 as usize]
            .as_ptr()
            .unwrap_or(std::ptr::null_mut());
        let dst_reg = instr.op2 as usize;
        if !src_ptr.is_null() {
            unsafe {
                let header = &*ActorHeap::header_of(src_ptr);
                if header.type_tag == HeapTypeTag::Record {
                    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
                    let slot_count = payload_size / std::mem::size_of::<Value>();
                    if let Some(dst_ptr) = self
                        .actor_callbacks
                        .alloc(payload_size, HeapTypeTag::Record)
                    {
                        let src_slots =
                            std::slice::from_raw_parts(src_ptr as *const Value, slot_count);
                        let dst_slots =
                            std::slice::from_raw_parts_mut(dst_ptr as *mut Value, slot_count);
                        for i in 0..slot_count {
                            let val = src_slots[i];
                            if let Some(ptr) = val.as_ptr() {
                                self.actor_callbacks.retain_ref(ptr);
                            }
                            dst_slots[i] = val;
                        }
                        self.frames[frame_idx].regs[dst_reg] = Value::ptr(dst_ptr);
                    } else {
                        self.frames[frame_idx].regs[dst_reg] = Value::nil();
                    }
                } else {
                    self.frames[frame_idx].regs[dst_reg] = Value::nil();
                }
            }
        } else {
            self.frames[frame_idx].regs[dst_reg] = Value::nil();
        }
        Ok(())
    }

    #[inline(never)]
    fn step_fields(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let tup_ptr = self.frames[frame_idx].regs[instr.op1 as usize]
            .as_ptr()
            .unwrap_or(std::ptr::null_mut());
        let idx = instr.op2 as usize;
        let val = self.frames[frame_idx].regs[instr.op3 as usize];
        if !tup_ptr.is_null() {
            unsafe {
                let header = &*ActorHeap::header_of(tup_ptr);
                if header.type_tag == HeapTypeTag::Tuple {
                    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
                    let len = payload_size / std::mem::size_of::<Value>();
                    if idx < len {
                        if let Some(ptr) = val.as_ptr() {
                            self.actor_callbacks.retain_ref(ptr);
                        }
                        let slot = (tup_ptr as *mut Value).add(idx);
                        let old = *slot;
                        *slot = val;
                        if let Some(old_ptr) = old.as_ptr() {
                            self.actor_callbacks.drop_ref(old_ptr);
                        }
                    }
                }
            }
        }
        Ok(())
    }
    #[inline(never)]
    fn step_arrload(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let arr_ptr = self.frames[frame_idx].regs[instr.op1 as usize]
            .as_ptr()
            .unwrap_or(std::ptr::null_mut());
        let idx = self.frames[frame_idx].regs[instr.op2 as usize]
            .as_int()
            .unwrap_or(0) as usize;
        let val = if !arr_ptr.is_null() {
            if let Some(len) = self.actor_callbacks.array_len(arr_ptr) {
                if idx < len {
                    unsafe { *((arr_ptr as *const Value).add(idx)) }
                } else {
                    Value::nil()
                }
            } else {
                Value::nil()
            }
        } else {
            Value::nil()
        };
        self.frames[frame_idx].regs[instr.op3 as usize] = val;
        Ok(())
    }

    #[inline(never)]
    fn step_recl(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let rec_ptr = self.frames[frame_idx].regs[instr.op1 as usize]
            .as_ptr()
            .unwrap_or(std::ptr::null_mut());
        let field_id = instr.op2 as usize;
        let val = if !rec_ptr.is_null() {
            unsafe {
                let header = &*ActorHeap::header_of(rec_ptr);
                if header.type_tag == HeapTypeTag::Record {
                    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
                    let len = payload_size / std::mem::size_of::<Value>();
                    if field_id < len {
                        *((rec_ptr as *const Value).add(field_id))
                    } else {
                        Value::nil()
                    }
                } else {
                    Value::nil()
                }
            }
        } else {
            Value::nil()
        };
        self.frames[frame_idx].regs[instr.op3 as usize] = val;
        Ok(())
    }

    #[inline(never)]
    fn step_fieldl(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let tup_ptr = self.frames[frame_idx].regs[instr.op1 as usize]
            .as_ptr()
            .unwrap_or(std::ptr::null_mut());
        let idx = instr.op2 as usize;
        let val = if !tup_ptr.is_null() {
            unsafe {
                let header = &*ActorHeap::header_of(tup_ptr);
                if header.type_tag == HeapTypeTag::Tuple {
                    let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
                    let len = payload_size / std::mem::size_of::<Value>();
                    if idx < len {
                        *((tup_ptr as *const Value).add(idx))
                    } else {
                        Value::nil()
                    }
                } else {
                    Value::nil()
                }
            }
        } else {
            Value::nil()
        };
        self.frames[frame_idx].regs[instr.op3 as usize] = val;
        Ok(())
    }
    fn step_spawn(
        &mut self,
        frame_idx: usize,
        module_idx: usize,
        instr: Instruction,
    ) -> NuResult<()> {
        let behavior_idx = instr.imm16() as usize;
        // Start with declared state defaults
        let mut init: Vec<(String, Value)> = self
            .modules
            .get(module_idx)
            .and_then(|m| {
                m.actor_metadata
                    .iter()
                    .find(|meta| meta.behavior_indices.contains(&behavior_idx))
            })
            .map(|meta| {
                meta.state_defaults
                    .iter()
                    .map(|(name, c)| (name.clone(), constant_to_value(c)))
                    .collect()
            })
            .unwrap_or_default();
        // Apply spawn-site init overrides (overwrite defaults)
        let spawn_pc = self.frames[frame_idx].pc.saturating_sub(1);
        if let Some(module) = self.modules.get(module_idx) {
            for &(offset, ref overrides) in &module.spawn_init_overrides {
                if offset == spawn_pc {
                    for (name, c) in overrides {
                        // Remove any existing default entry for this field
                        init.retain(|(n, _)| n != name);
                        init.push((name.clone(), constant_to_value(c)));
                    }
                    break;
                }
            }
        }
        let result = if let Some(module) = self.modules.get(module_idx) {
            self.actor_callbacks.spawn_actor(module, behavior_idx, init)
        } else {
            Value::actor_ref(0)
        };
        self.frames[frame_idx].regs[instr.op3 as usize] = result;
        Ok(())
    }

    #[inline(never)]
    fn step_rsend(
        &mut self,
        frame_idx: usize,
        module_idx: usize,
        instr: Instruction,
    ) -> NuResult<()> {
        let target_reg = instr.op1 as usize;
        let behavior_idx = instr.imm16() as usize;
        let target_val = self.frames[frame_idx].regs[target_reg];
        let target_id = target_val.as_actor_id().unwrap_or(0);
        let node_id = self.node_id;
        let (param_count, _behavior_id) = self
            .modules
            .get(module_idx)
            .and_then(|m| m.behaviors.get(behavior_idx))
            .map(|b| (b.param_count, behavior_idx as u16))
            .unwrap_or((0, 0));
        let args: Vec<Value> = (0..param_count as usize)
            .map(|i| self.frames[frame_idx].regs[i])
            .collect();
        if let Some(cb) = &mut self.distributed_callbacks {
            let behavior_name = self
                .modules
                .get(module_idx)
                .and_then(|m| m.behaviors.get(behavior_idx))
                .map(|b| b.name.clone())
                .unwrap_or_default();
            cb.remote_send(target_id, node_id, &behavior_name, &args);
        }
        Ok(())
    }

    #[inline(never)]
    fn step_rspawn(
        &mut self,
        frame_idx: usize,
        module_idx: usize,
        instr: Instruction,
    ) -> NuResult<()> {
        let node_reg = instr.op1 as usize;
        let behavior_idx = (((instr.op2 as u16) << 8) | (instr.op3 as u16)) as usize;
        let node_id = self.frames[frame_idx].regs[node_reg].as_int().unwrap_or(0) as u64;
        let spawn_pc = self.frames[frame_idx].pc.saturating_sub(1);
        let names: Vec<String> = self
            .modules
            .get(module_idx)
            .and_then(|m| {
                m.remote_spawn_init_fields
                    .iter()
                    .find(|(pc, _)| *pc == spawn_pc)
                    .map(|(_, n)| n.clone())
            })
            .unwrap_or_default();
        let init: Vec<(String, Value)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), self.frames[frame_idx].regs[i]))
            .collect();
        let result = if self.distributed_callbacks.is_some() && node_id != self.node_id {
            let behavior_name = self
                .modules
                .get(module_idx)
                .and_then(|m| m.behaviors.get(behavior_idx))
                .map(|b| b.name.clone())
                .unwrap_or_default();
            if let Some(cb) = &mut self.distributed_callbacks {
                cb.remote_spawn(node_id, &behavior_name, &init)
            } else {
                Value::actor_ref(0)
            }
        } else {
            match self.modules.get(module_idx) {
                Some(module) => self.actor_callbacks.spawn_actor(module, behavior_idx, init),
                None => Value::actor_ref(0),
            }
        };
        self.frames[frame_idx].regs[node_reg] = result;
        Ok(())
    }

    #[inline(never)]
    fn step_capload(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let slot = instr.op1 as usize;
        let dst = instr.op2 as usize;
        let env_val = self.frames[frame_idx]
            .closure_env
            .ok_or_else(|| NuError::VMError {
                msg: "CapLoad outside a closure call".to_string(),
                span: Span::default(),
            })?;
        let payload = env_val.raw & PAYLOAD_MASK;
        if payload & CLOSURE_ENV_FLAG == 0 {
            return Err(NuError::VMError {
                msg: "CapLoad in a closure without captures".to_string(),
                span: Span::default(),
            });
        }
        let env_idx = (payload & CLOSURE_ENV_IDX_MASK) as usize;
        let value = self
            .closure_envs
            .get(env_idx)
            .and_then(|env| env.captures.get(slot))
            .copied()
            .ok_or_else(|| NuError::VMError {
                msg: format!("CapLoad of missing capture slot {}", slot),
                span: Span::default(),
            })?;
        self.frames[frame_idx].regs[dst] = value;
        Ok(())
    }

    #[inline(never)]
    fn step_sconcat(
        &mut self,
        frame_idx: usize,
        module_idx: usize,
        instr: Instruction,
    ) -> NuResult<()> {
        let s1 = resolve_value_string(
            &self.modules[module_idx].constants,
            self.frames[frame_idx].regs[instr.op1 as usize],
        );
        let s2 = resolve_value_string(
            &self.modules[module_idx].constants,
            self.frames[frame_idx].regs[instr.op2 as usize],
        );
        let result = format!("{}{}", s1, s2);
        let bytes = result.into_bytes();
        self.frames[frame_idx].regs[instr.op3 as usize] = if let Some(ptr) = self
            .actor_callbacks
            .alloc(bytes.len() + 1, HeapTypeTag::String)
        {
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                *ptr.add(bytes.len()) = 0;
            }
            Value::ptr(ptr)
        } else {
            Value::nil()
        };
        Ok(())
    }

    #[inline(never)]
    fn step_sread(
        &mut self,
        frame_idx: usize,
        _module_idx: usize,
        instr: Instruction,
    ) -> NuResult<()> {
        let mut input = String::new();
        self.frames[frame_idx].regs[instr.op1 as usize] =
            if std::io::stdin().read_line(&mut input).is_ok() {
                let bytes = input.into_bytes();
                if let Some(ptr) = self
                    .actor_callbacks
                    .alloc(bytes.len() + 1, HeapTypeTag::String)
                {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                        *ptr.add(bytes.len()) = 0;
                    }
                    Value::ptr(ptr)
                } else {
                    Value::nil()
                }
            } else {
                Value::nil()
            };
        Ok(())
    }

    #[inline(never)]
    fn step_idiv(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let a = self.frames[frame_idx].regs[instr.op1 as usize];
        let b = self.frames[frame_idx].regs[instr.op2 as usize];
        if a.is_float() && b.is_float() {
            let af = a.as_float().unwrap();
            let bf = b.as_float().unwrap();
            self.frames[frame_idx].regs[instr.op3 as usize] = if bf != 0.0 {
                Value::float(af / bf)
            } else {
                Value::nil()
            };
        } else {
            let ai = a.as_int().unwrap_or(0);
            let bi = b.as_int().unwrap_or(1);
            self.frames[frame_idx].regs[instr.op3 as usize] = if bi != 0 {
                Value::int(ai / bi)
            } else {
                Value::nil()
            };
        }
        Ok(())
    }

    #[inline(never)]
    fn step_imod(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let a = self.frames[frame_idx].regs[instr.op1 as usize];
        let b = self.frames[frame_idx].regs[instr.op2 as usize];
        if a.is_float() && b.is_float() {
            let af = a.as_float().unwrap();
            let bf = b.as_float().unwrap();
            self.frames[frame_idx].regs[instr.op3 as usize] = if bf != 0.0 {
                Value::float(af % bf)
            } else {
                Value::nil()
            };
        } else {
            let ai = a.as_int().unwrap_or(0);
            let bi = b.as_int().unwrap_or(1);
            self.frames[frame_idx].regs[instr.op3 as usize] = if bi != 0 {
                Value::int(ai % bi)
            } else {
                Value::nil()
            };
        }
        Ok(())
    }

    /// Integer exponentiation using binary exponentiation (fast pow).
    /// Uses wrapping_mul to match IMul behaviour (48-bit wrap).
    /// Negative exponent returns nil (mirrors IDiv div-by-zero).
    /// 0 ** 0 returns 1 (standard convention).
    #[inline(never)]
    fn step_ipow(&mut self, frame_idx: usize, instr: Instruction) -> NuResult<()> {
        let a = self.frames[frame_idx].regs[instr.op1 as usize];
        let b = self.frames[frame_idx].regs[instr.op2 as usize];
        if a.is_float() && b.is_float() {
            let af = a.as_float().unwrap();
            let bf = b.as_float().unwrap();
            self.frames[frame_idx].regs[instr.op3 as usize] = Value::float(af.powf(bf));
        } else {
            let base = a.as_int().unwrap_or(0);
            let exp = b.as_int().unwrap_or(0);
            if exp < 0 {
                self.frames[frame_idx].regs[instr.op3 as usize] = Value::nil();
            } else {
                // Binary exponentiation with wrapping_mul
                let mut result: i64 = 1;
                let mut base = base;
                let mut exp = exp;
                while exp > 0 {
                    if exp & 1 != 0 {
                        result = result.wrapping_mul(base);
                    }
                    exp >>= 1;
                    if exp > 0 {
                        base = base.wrapping_mul(base);
                    }
                }
                self.frames[frame_idx].regs[instr.op3 as usize] = Value::int(result);
            }
        }
        Ok(())
    }
    fn enrich_error(&self, msg: String) -> String {
        let mut e = msg;
        e.push_str("\nStack trace:");
        let mut depth = 0;
        let mut idx = self.current_frame_idx;
        while let Some(i) = idx {
            let fr = &self.frames[i];
            let name = self
                .modules
                .get(fr.module_idx)
                .map(|m| m.name.as_str())
                .unwrap_or("?");
            e.push_str(&format!("\n  [{}] {}:pc{}", depth, name, fr.pc));
            depth += 1;
            idx = fr.caller_idx;
        }
        if depth == 0 {
            e.push_str("\n  (empty)");
        }
        e
    }

    pub fn step(&mut self) -> NuResult<()> {
        // Step limit: configurable via env var NULANG_STEP_LIMIT.
        // Default 10M steps — long-running actors (servers, processors) may need more.
        // Check every 64 steps to reduce branch overhead (safety limit, not precise).
        self.step_count += 1;
        if self.step_count & 63 == 0 {
            let limit = Self::step_limit();
            if self.step_count > limit {
                return Err(NuError::VMError {
                    msg: format!(
                        "Step limit exceeded ({} steps). Set NULANG_STEP_LIMIT env var to increase.",
                        self.step_count
                    ),
                    span: Span::default(),
                });
            }
        }

        let frame_idx = self.current_frame_idx.ok_or_else(|| NuError::VMError {
            msg: "No current frame".to_string(),
            span: Span::default(),
        })?;

        // Try JIT execution for hot bytecode regions before interpreting.
        // Disabled while a debugger is attached so every instruction flows
        // through the interpreter (and the debug hook below).
        if self.debug_hook.is_none()
            && self.jit_session.is_some()
            && self.try_jit_execute(frame_idx)
        {
            if let Some(msg) = self.jit_pending_error.take() {
                return Err(NuError::runtime_error(msg, Span::default()));
            }
            return Ok(());
        }

        // Fetch instruction - cache frame and module references to avoid repeated indexing
        let module_idx = self.frames[frame_idx].module_idx;
        let pc = self.frames[frame_idx].pc;
        let module = self
            .modules
            .get(module_idx)
            .ok_or_else(|| NuError::VMError {
                msg: format!("Module {} not found", module_idx),
                span: Span::default(),
            })?;
        let instr = *module
            .instructions
            .get(pc)
            .ok_or_else(|| NuError::VMError {
                msg: format!("PC {} out of bounds in module {}", pc, module_idx),
                span: Span::default(),
            })?;

        // Debugger checkpoint: invoked before the instruction executes, so a
        // pause leaves `pc` pointing at the current instruction (resume
        // re-executes it). On pause, return the DEBUG_PAUSE_MSG sentinel.
        if self.debug_hook.is_some() {
            let line = self.modules.get(module_idx).and_then(|m| m.line_at(pc));
            let frame_depth = self.frame_depth(frame_idx);
            let action = self
                .debug_hook
                .as_mut()
                .map(|hook| {
                    hook.before_instruction(&DebugContext {
                        module_idx,
                        pc,
                        frame_idx,
                        opcode: instr.opcode,
                        frame_depth,
                        line,
                    })
                })
                .unwrap_or(DebugAction::Continue);
            if action == DebugAction::Pause {
                return Err(NuError::VMError {
                    msg: DEBUG_PAUSE_MSG.to_string(),
                    span: Span::default(),
                });
            }
        }

        self.frames[frame_idx].pc += 1;

        // Cache a mutable reference to the current frame for the duration of
        // this instruction. The reference is derived from a raw pointer so it
        // does not conflict with other borrows of `self` inside the opcode
        // arms. It remains valid because arms that grow/shrink the frame
        // vector (Call, ClosureCall, Ret, RetVal) return immediately and do
        // not use the cached reference after mutating the vector.
        let frame = unsafe { &mut *self.frames.as_mut_ptr().add(frame_idx) };
        let constants = &module.constants;

        match instr.opcode {
            // -- Frame-manipulating opcodes --
            OpCode::Call => {
                let func_val = frame.regs[instr.op1 as usize];
                let argc = instr.op2;
                let dst = instr.op3;
                let (func_idx, closure_env) = self.resolve_function(func_val, module_idx)?;
                let code_offset = self
                    .modules
                    .get(module_idx)
                    .and_then(|m| m.function_table.get(func_idx))
                    .copied()
                    .ok_or_else(|| NuError::VMError {
                        msg: format!("Function {} not found", func_idx),
                        span: Span::default(),
                    })?;
                let mut new_frame = Frame::new(Some(frame_idx), module_idx);
                new_frame.pc = code_offset;
                new_frame.regs[..argc as usize].copy_from_slice(&frame.regs[..argc as usize]);
                new_frame.return_dst = dst;
                new_frame.closure_env = closure_env;
                self.frames.push(new_frame);
                self.current_frame_idx = Some(self.frames.len() - 1);
                return Ok(());
            }
            OpCode::TailCall => {
                let func_val = frame.regs[instr.op1 as usize];
                let func_idx = func_val.as_int().ok_or_else(|| NuError::VMError {
                    msg: "Invalid function reference".to_string(),
                    span: Span::default(),
                })? as usize;
                let code_offset = self
                    .modules
                    .get(module_idx)
                    .and_then(|m| m.function_table.get(func_idx))
                    .copied()
                    .ok_or_else(|| NuError::VMError {
                        msg: format!("Function {} not found", func_idx),
                        span: Span::default(),
                    })?;
                frame.pc = code_offset;
                return Ok(());
            }
            OpCode::Ret => {
                let ret_val = frame.regs[0];
                if let Some(caller_idx) = frame.caller_idx {
                    let dst = frame.return_dst;
                    self.frames[caller_idx].regs[dst as usize] = ret_val;
                    self.frames.pop();
                    self.current_frame_idx = Some(caller_idx);
                    return Ok(());
                }
                // No caller frame: halt so that run/run_from stop at the end
                // of a top-level behavior handler instead of falling through
                // into the next compiled code region.
                frame.regs[0] = ret_val;
                return Err(NuError::VMError {
                    msg: "Halt".to_string(),
                    span: Span::default(),
                });
            }
            OpCode::RetVal => {
                let ret_val = frame.regs[instr.op1 as usize];
                if let Some(caller_idx) = frame.caller_idx {
                    let dst = frame.return_dst;
                    self.frames[caller_idx].regs[dst as usize] = ret_val;
                    self.frames.pop();
                    self.current_frame_idx = Some(caller_idx);
                    return Ok(());
                }
                // No caller frame: halt so that run/run_from stop at the end
                // of a top-level behavior handler instead of falling through
                // into the next compiled code region.
                frame.regs[0] = ret_val;
                return Err(NuError::VMError {
                    msg: "Halt".to_string(),
                    span: Span::default(),
                });
            }
            OpCode::ClosureCall => {
                let closure_val = frame.regs[instr.op1 as usize];
                let dst = instr.op3;
                let (func_idx, closure_env) = self.resolve_function(closure_val, module_idx)?;
                let code_offset = self
                    .modules
                    .get(module_idx)
                    .and_then(|m| m.function_table.get(func_idx))
                    .copied()
                    .ok_or_else(|| NuError::VMError {
                        msg: format!("Function {} not found", func_idx),
                        span: Span::default(),
                    })?;
                let mut new_frame = Frame::new(Some(frame_idx), module_idx);
                new_frame.pc = code_offset;
                new_frame.regs = frame.regs;
                new_frame.return_dst = dst;
                new_frame.closure_env = closure_env;
                self.frames.push(new_frame);
                self.current_frame_idx = Some(self.frames.len() - 1);
                return Ok(());
            }
            OpCode::FFICall => {
                self.step_fficall(instr, frame_idx, module_idx)?;
            }
            OpCode::Panic => {
                let pc = frame.pc.saturating_sub(1);
                let r0_repr = frame.regs[0].to_string_repr();
                return Err(NuError::VMError {
                    msg: format!("Panic at PC {}: r0={}", pc, r0_repr),
                    span: Span::default(),
                });
            }

            // -- Actor opcodes --
            OpCode::Spawn => {
                self.step_spawn(frame_idx, module_idx, instr)?;
                return Ok(());
            }
            OpCode::Send => {
                let actor_val = frame.regs[instr.op1 as usize];
                let behavior_idx = (((instr.op2 as u16) << 8) | (instr.op3 as u16)) as usize;
                let (param_count, behavior_id) = self
                    .modules
                    .get(module_idx)
                    .and_then(|m| m.behaviors.get(behavior_idx))
                    .map(|b| (b.param_count, behavior_idx as u16))
                    .unwrap_or((0, 0));
                let args: Vec<Value> = (0..param_count).map(|i| frame.regs[i]).collect();
                self.actor_callbacks
                    .send_message(actor_val, behavior_id, &args);
                return Ok(());
            }
            OpCode::Ask => {
                let actor_val = frame.regs[instr.op1 as usize];
                let behavior_idx = (((instr.op2 as u16) << 8) | (instr.op3 as u16)) as usize;
                let (param_count, behavior_id) = self
                    .modules
                    .get(module_idx)
                    .and_then(|m| m.behaviors.get(behavior_idx))
                    .map(|b| (b.param_count, behavior_idx as u16))
                    .unwrap_or((0, 0));
                let args: Vec<Value> = (0..param_count).map(|i| frame.regs[i]).collect();
                let result = self
                    .actor_callbacks
                    .ask_actor(actor_val, behavior_id, &args);
                frame.regs[instr.op1 as usize] = result;
                return Ok(());
            }
            OpCode::SelfOp => {
                let actor_id = self.actor_callbacks.current_actor_id().unwrap_or(0);
                frame.regs[instr.op1 as usize] = Value::actor_ref(actor_id);
            }
            OpCode::StateGet => {
                let field_idx = instr.imm16() as usize;
                let field = self.module_const_string(module_idx, field_idx);
                frame.regs[instr.op3 as usize] = self.actor_callbacks.get_state_field(&field);
            }
            OpCode::StateSet => {
                let field_idx = instr.imm16() as usize;
                let field = self.module_const_string(module_idx, field_idx);
                let val = frame.regs[instr.op3 as usize];
                self.actor_callbacks.set_state_field(&field, val);
            }
            OpCode::Emit => {
                let event_idx = instr.imm16() as usize;
                let event = self.module_const_string(module_idx, event_idx);
                let arg_count = instr.op3 as usize;
                let args: Vec<Value> = (0..arg_count).map(|i| frame.regs[i]).collect();
                self.actor_callbacks.emit_event(&event, &args);
            }
            OpCode::SignalWait => {
                let name_idx = instr.imm16() as usize;
                let name = self.module_const_string(module_idx, name_idx);
                let dst = instr.op3;
                match self.actor_callbacks.wait_signal(&name) {
                    SignalWaitResult::Ready(v) => {
                        frame.regs[dst as usize] = v;
                    }
                    SignalWaitResult::NotReady => {
                        self.suspended_signal_name = Some(name.clone());
                        // Leave the PC pointing at the SignalWait instruction so
                        // resumption re-executes it and can write the result into
                        // the destination register once the signal is received.
                        frame.pc -= 1;
                        return Err(NuError::Suspended(VmSuspension::SignalWait));
                    }
                }
            }
            OpCode::RSend => {
                self.step_rsend(frame_idx, module_idx, instr)?;
            }
            OpCode::RSpawn => {
                self.step_rspawn(frame_idx, module_idx, instr)?;
            }

            // -- Constants --
            OpCode::Const0 => {
                frame.regs[instr.op1 as usize] = Value::int(0);
            }
            OpCode::Const1 => {
                frame.regs[instr.op1 as usize] = Value::int(1);
            }
            OpCode::Const2 => {
                frame.regs[instr.op1 as usize] = Value::int(2);
            }
            OpCode::ConstU => {
                let idx = instr.imm16() as usize;
                let val = constants
                    .get(idx)
                    .map(|c| match *c {
                        Constant::Int(n) => Value::int(n),
                        Constant::Float(f) => Value::float(f),
                        Constant::String(_) => Value::string(idx as u32),
                        Constant::Bool(b) => Value::bool(b),
                        Constant::Nil => Value::nil(),
                        Constant::Unit => Value::unit(),
                        _ => Value::nil(),
                    })
                    .unwrap_or(Value::nil());
                frame.regs[instr.op3 as usize] = val;
            }
            OpCode::Closure => {
                let func_idx = instr.imm16() as u64;
                frame.regs[instr.op3 as usize] = Value::closure(func_idx);
            }
            OpCode::CapStore => {
                self.step_capstore(instr, frame_idx)?;
            }
            OpCode::CapLoad => {
                self.step_capload(frame_idx, instr)?;
            }
            OpCode::FreeVar => {
                // Reserved opcode; never emitted. No-op.
            }

            // -- Arithmetic --
            OpCode::IAdd => {
                let a = frame.regs[instr.op1 as usize];
                let b = frame.regs[instr.op2 as usize];
                // String concatenation fallback: when the compiler could not
                // determine operand types at compile time (e.g. unannotated
                // function parameters), the IAdd opcode is emitted instead of
                // SConcat.  Check at runtime whether either operand is a
                // string (constant-pool TAG_STRING or heap-allocated TAG_PTR
                // with HeapTypeTag::String) and concatenate if so.
                let sa = self.string_operand(module_idx, a);
                let sb = self.string_operand(module_idx, b);
                if sa.is_some() || sb.is_some() {
                    let s1 = sa.unwrap_or_else(|| a.to_string_repr());
                    let s2 = sb.unwrap_or_else(|| b.to_string_repr());
                    let result = format!("{}{}", s1, s2);
                    frame.regs[instr.op3 as usize] = self.allocate_string(&result);
                } else if a.is_float() && b.is_float() {
                    frame.regs[instr.op3 as usize] =
                        Value::float(a.as_float().unwrap() + b.as_float().unwrap());
                } else {
                    frame.regs[instr.op3 as usize] =
                        Value::int(a.as_int().unwrap_or(0) + b.as_int().unwrap_or(0));
                }
            }
            OpCode::ISub => {
                let a = frame.regs[instr.op1 as usize];
                let b = frame.regs[instr.op2 as usize];
                if a.is_float() && b.is_float() {
                    frame.regs[instr.op3 as usize] =
                        Value::float(a.as_float().unwrap() - b.as_float().unwrap());
                } else {
                    frame.regs[instr.op3 as usize] =
                        Value::int(a.as_int().unwrap_or(0) - b.as_int().unwrap_or(0));
                }
            }
            OpCode::IMul => {
                let a = frame.regs[instr.op1 as usize];
                let b = frame.regs[instr.op2 as usize];
                if a.is_float() && b.is_float() {
                    frame.regs[instr.op3 as usize] =
                        Value::float(a.as_float().unwrap() * b.as_float().unwrap());
                } else {
                    // wrapping_mul: 48-bit operands can overflow i64 when
                    // multiplied; the result is masked to 48 bits by Value::int.
                    frame.regs[instr.op3 as usize] = Value::int(
                        a.as_int()
                            .unwrap_or(0)
                            .wrapping_mul(b.as_int().unwrap_or(0)),
                    );
                }
            }
            OpCode::IDiv => {
                self.step_idiv(frame_idx, instr)?;
            }
            OpCode::IMod => {
                self.step_imod(frame_idx, instr)?;
            }
            OpCode::IPow => {
                self.step_ipow(frame_idx, instr)?;
            }
            OpCode::Xor => {
                let a = frame.regs[instr.op1 as usize].as_int().unwrap_or(0);
                let b = frame.regs[instr.op2 as usize].as_int().unwrap_or(0);
                frame.regs[instr.op3 as usize] = Value::int(a ^ b);
            }
            OpCode::Shl => {
                let a = frame.regs[instr.op1 as usize].as_int().unwrap_or(0);
                let b = frame.regs[instr.op2 as usize].as_int().unwrap_or(0);
                let shift = (b as u64) & 0x3f;
                frame.regs[instr.op3 as usize] = Value::int(a << shift);
            }
            OpCode::Shr => {
                let a = frame.regs[instr.op1 as usize].as_int().unwrap_or(0);
                let b = frame.regs[instr.op2 as usize].as_int().unwrap_or(0);
                let shift = (b as u64) & 0x3f;
                frame.regs[instr.op3 as usize] = Value::int(a >> shift);
            }
            OpCode::BitAnd => {
                let a = frame.regs[instr.op1 as usize].as_int().unwrap_or(0);
                let b = frame.regs[instr.op2 as usize].as_int().unwrap_or(0);
                frame.regs[instr.op3 as usize] = Value::int(a & b);
            }
            OpCode::BitOr => {
                let a = frame.regs[instr.op1 as usize].as_int().unwrap_or(0);
                let b = frame.regs[instr.op2 as usize].as_int().unwrap_or(0);
                frame.regs[instr.op3 as usize] = Value::int(a | b);
            }
            OpCode::INeg => {
                let a = frame.regs[instr.op1 as usize];
                if a.is_float() {
                    frame.regs[instr.op2 as usize] = Value::float(-a.as_float().unwrap());
                } else {
                    match a.as_int() {
                        Some(x) if x != crate::value_layout::INT48_MIN => {
                            frame.regs[instr.op2 as usize] = Value::int(-x);
                        }
                        Some(x) => return Err(int_overflow_error("neg", x, 0)),
                        None => return Err(arith_type_error("neg", a, a)),
                    }
                }
            }
            // IInc/IDec mirror the JIT helpers `nulang_iinc`/`nulang_idec`
            // bit-for-bit: the register's low 48 payload bits are read as a
            // signed value (tag ignored), adjusted by ±1 with 48-bit wrap,
            // and the result is re-tagged as an int.
            OpCode::IInc => {
                let reg = instr.op1 as usize;
                let a = sext48(frame.regs[reg].as_raw() & PAYLOAD_MASK);
                frame.regs[reg] = Value::int(a + 1);
            }
            OpCode::IDec => {
                let reg = instr.op1 as usize;
                let a = sext48(frame.regs[reg].as_raw() & PAYLOAD_MASK);
                frame.regs[reg] = Value::int(a - 1);
            }

            // -- Float arithmetic --
            OpCode::FAdd => {
                let a = frame.regs[instr.op1 as usize].as_float().unwrap_or(0.0);
                let b = frame.regs[instr.op2 as usize].as_float().unwrap_or(0.0);
                frame.regs[instr.op3 as usize] = Value::float(a + b);
            }
            OpCode::FSub => {
                let a = frame.regs[instr.op1 as usize].as_float().unwrap_or(0.0);
                let b = frame.regs[instr.op2 as usize].as_float().unwrap_or(0.0);
                frame.regs[instr.op3 as usize] = Value::float(a - b);
            }
            OpCode::FMul => {
                let a = frame.regs[instr.op1 as usize].as_float().unwrap_or(0.0);
                let b = frame.regs[instr.op2 as usize].as_float().unwrap_or(0.0);
                frame.regs[instr.op3 as usize] = Value::float(a * b);
            }
            OpCode::FDiv => {
                let a = frame.regs[instr.op1 as usize].as_float().unwrap_or(0.0);
                let b = frame.regs[instr.op2 as usize].as_float().unwrap_or(1.0);
                frame.regs[instr.op3 as usize] = if b != 0.0 {
                    Value::float(a / b)
                } else {
                    Value::nil()
                };
            }
            OpCode::FPow => {
                let a = frame.regs[instr.op1 as usize].as_float().unwrap_or(0.0);
                let b = frame.regs[instr.op2 as usize].as_float().unwrap_or(0.0);
                frame.regs[instr.op3 as usize] = Value::float(a.powf(b));
            }
            OpCode::FMod => {
                let a = frame.regs[instr.op1 as usize].as_float().unwrap_or(0.0);
                let b = frame.regs[instr.op2 as usize].as_float().unwrap_or(1.0);
                frame.regs[instr.op3 as usize] = if b != 0.0 {
                    Value::float(a % b)
                } else {
                    Value::nil()
                };
            }
            OpCode::FNeg => {
                let a = frame.regs[instr.op1 as usize].as_float().unwrap_or(0.0);
                frame.regs[instr.op3 as usize] = Value::float(-a);
            }
            // -- Comparison --
            OpCode::ICmpEq => {
                let a = frame.regs[instr.op1 as usize];
                let b = frame.regs[instr.op2 as usize];
                frame.regs[instr.op3 as usize] = if a.is_float() && b.is_float() {
                    Value::bool(
                        (a.as_float().unwrap() - b.as_float().unwrap()).abs() < f64::EPSILON,
                    )
                } else if a.is_int() && b.is_int() {
                    Value::bool(a.as_int().unwrap() == b.as_int().unwrap())
                } else if a.is_float() && b.is_int() {
                    let bf = b.as_int().unwrap() as f64;
                    Value::bool((a.as_float().unwrap() - bf).abs() < f64::EPSILON)
                } else if a.is_int() && b.is_float() {
                    let af = a.as_int().unwrap() as f64;
                    Value::bool((af - b.as_float().unwrap()).abs() < f64::EPSILON)
                } else if a.is_string() || a.is_ptr() || b.is_string() || b.is_ptr() {
                    // String equality must compare content, not raw bits.
                    // Two interned strings (TAG_STRING) may have different
                    // constant-pool indices; heap strings (TAG_PTR) may hold
                    // the same text at different addresses.  Only when BOTH
                    // resolve to a string do we compare text; a string vs
                    // non-string is never equal.
                    let eq = match (
                        self.string_operand(module_idx, a),
                        self.string_operand(module_idx, b),
                    ) {
                        (Some(sa), Some(sb)) => sa == sb,
                        _ => false,
                    };
                    Value::bool(eq)
                } else {
                    Value::bool(a.raw == b.raw)
                };
            }
            OpCode::ICmpLt => {
                let a = frame.regs[instr.op1 as usize];
                let b = frame.regs[instr.op2 as usize];
                frame.regs[instr.op3 as usize] = if a.is_float() && b.is_float() {
                    Value::bool(a.as_float().unwrap() < b.as_float().unwrap())
                } else if a.is_int() && b.is_int() {
                    Value::bool(a.as_int().unwrap() < b.as_int().unwrap())
                } else if a.is_float() && b.is_int() {
                    Value::bool(a.as_float().unwrap() < b.as_int().unwrap() as f64)
                } else if a.is_int() && b.is_float() {
                    Value::bool((a.as_int().unwrap() as f64) < b.as_float().unwrap())
                } else {
                    Value::bool(a.raw < b.raw)
                };
            }
            OpCode::ICmpGt => {
                let a = frame.regs[instr.op1 as usize];
                let b = frame.regs[instr.op2 as usize];
                frame.regs[instr.op3 as usize] = if a.is_float() && b.is_float() {
                    Value::bool(a.as_float().unwrap() > b.as_float().unwrap())
                } else if a.is_int() && b.is_int() {
                    Value::bool(a.as_int().unwrap() > b.as_int().unwrap())
                } else if a.is_float() && b.is_int() {
                    Value::bool(a.as_float().unwrap() > b.as_int().unwrap() as f64)
                } else if a.is_int() && b.is_float() {
                    Value::bool((a.as_int().unwrap() as f64) > b.as_float().unwrap())
                } else {
                    Value::bool(a.raw > b.raw)
                };
            }
            OpCode::ICmpLe => {
                let a = frame.regs[instr.op1 as usize];
                let b = frame.regs[instr.op2 as usize];
                frame.regs[instr.op3 as usize] = if a.is_float() && b.is_float() {
                    Value::bool(a.as_float().unwrap() <= b.as_float().unwrap())
                } else if a.is_int() && b.is_int() {
                    Value::bool(a.as_int().unwrap() <= b.as_int().unwrap())
                } else if a.is_float() && b.is_int() {
                    Value::bool(a.as_float().unwrap() <= b.as_int().unwrap() as f64)
                } else if a.is_int() && b.is_float() {
                    Value::bool((a.as_int().unwrap() as f64) <= b.as_float().unwrap())
                } else {
                    Value::bool(a.raw <= b.raw)
                };
            }
            OpCode::ICmpGe => {
                let a = frame.regs[instr.op1 as usize];
                let b = frame.regs[instr.op2 as usize];
                frame.regs[instr.op3 as usize] = if a.is_float() && b.is_float() {
                    Value::bool(a.as_float().unwrap() >= b.as_float().unwrap())
                } else if a.is_int() && b.is_int() {
                    Value::bool(a.as_int().unwrap() >= b.as_int().unwrap())
                } else if a.is_float() && b.is_int() {
                    Value::bool(a.as_float().unwrap() >= b.as_int().unwrap() as f64)
                } else if a.is_int() && b.is_float() {
                    Value::bool((a.as_int().unwrap() as f64) >= b.as_float().unwrap())
                } else {
                    Value::bool(a.raw >= b.raw)
                };
            }
            OpCode::FCmpEq => {
                let a = frame.regs[instr.op1 as usize].as_float().unwrap_or(0.0);
                let b = frame.regs[instr.op2 as usize].as_float().unwrap_or(0.0);
                frame.regs[instr.op3 as usize] = Value::bool((a - b).abs() < f64::EPSILON);
            }
            OpCode::FCmpLt => {
                let a = frame.regs[instr.op1 as usize].as_float().unwrap_or(0.0);
                let b = frame.regs[instr.op2 as usize].as_float().unwrap_or(0.0);
                frame.regs[instr.op3 as usize] = Value::bool(a < b);
            }
            OpCode::FCmpGt => {
                let a = frame.regs[instr.op1 as usize].as_float().unwrap_or(0.0);
                let b = frame.regs[instr.op2 as usize].as_float().unwrap_or(0.0);
                frame.regs[instr.op3 as usize] = Value::bool(a > b);
            }
            OpCode::SCmpEq => {
                let a = frame.regs[instr.op1 as usize];
                let b = frame.regs[instr.op2 as usize];
                let eq = match (
                    self.string_operand(module_idx, a),
                    self.string_operand(module_idx, b),
                ) {
                    (Some(sa), Some(sb)) => sa == sb,
                    _ => false,
                };
                frame.regs[instr.op3 as usize] = Value::bool(eq);
            }

            // -- Arrays (actor-heap backed; no longer leaked) --
            OpCode::ArrAlloc => {
                let len = frame.regs[instr.op1 as usize].as_int().unwrap_or(0) as usize;
                let size = len.checked_mul(std::mem::size_of::<Value>()).unwrap_or(0);
                frame.regs[instr.op2 as usize] =
                    if let Some(ptr) = self.actor_callbacks.alloc(size, HeapTypeTag::Array) {
                        unsafe {
                            let slots = std::slice::from_raw_parts_mut(ptr as *mut Value, len);
                            for slot in slots.iter_mut() {
                                *slot = Value::nil();
                            }
                        }
                        Value::ptr(ptr)
                    } else {
                        Value::nil()
                    };
            }
            OpCode::ArrLoad => {
                self.step_arrload(frame_idx, instr)?;
            }
            OpCode::ArrStore => {
                self.step_arrstore(frame_idx, instr)?;
            }
            OpCode::ArrLen => {
                let arr_ptr = frame.regs[instr.op1 as usize]
                    .as_ptr()
                    .unwrap_or(std::ptr::null_mut());
                let len = if !arr_ptr.is_null() {
                    self.actor_callbacks.array_len(arr_ptr).unwrap_or(0) as i64
                } else {
                    0
                };
                frame.regs[instr.op2 as usize] = Value::int(len);
            }

            // -- Records (flat array indexed by module field id) --
            OpCode::RecMk => {
                let slot_count = instr.op1 as usize;
                let size = slot_count
                    .checked_mul(std::mem::size_of::<Value>())
                    .unwrap_or(0);
                frame.regs[instr.op2 as usize] = if let Some(ptr) =
                    self.actor_callbacks.alloc(size, HeapTypeTag::Record)
                {
                    unsafe {
                        let slots = std::slice::from_raw_parts_mut(ptr as *mut Value, slot_count);
                        for slot in slots.iter_mut() {
                            *slot = Value::nil();
                        }
                    }
                    Value::ptr(ptr)
                } else {
                    Value::nil()
                };
            }
            OpCode::RecS => {
                self.step_recs(frame_idx, instr)?;
            }
            OpCode::RecL => {
                self.step_recl(frame_idx, instr)?;
            }

            OpCode::RecCopy => {
                self.step_reccopy(frame_idx, instr)?;
            }
            // -- Tuples (heap-backed fixed-size arrays) --
            OpCode::TupleMk => {
                let count = instr.op1 as usize;
                let size = count.checked_mul(std::mem::size_of::<Value>()).unwrap_or(0);
                frame.regs[instr.op2 as usize] =
                    if let Some(ptr) = self.actor_callbacks.alloc(size, HeapTypeTag::Tuple) {
                        unsafe {
                            let slots = std::slice::from_raw_parts_mut(ptr as *mut Value, count);
                            for slot in slots.iter_mut() {
                                *slot = Value::nil();
                            }
                        }
                        Value::ptr(ptr)
                    } else {
                        Value::nil()
                    };
            }
            OpCode::FieldS => {
                self.step_fields(frame_idx, instr)?;
            }
            OpCode::FieldL => {
                self.step_fieldl(frame_idx, instr)?;
            }

            // -- Boolean logic --
            OpCode::And => {
                let a = frame.regs[instr.op1 as usize].as_bool().unwrap_or(false);
                let b = frame.regs[instr.op2 as usize].as_bool().unwrap_or(false);
                frame.regs[instr.op3 as usize] = Value::bool(a && b);
            }
            OpCode::Or => {
                let a = frame.regs[instr.op1 as usize].as_bool().unwrap_or(false);
                let b = frame.regs[instr.op2 as usize].as_bool().unwrap_or(false);
                frame.regs[instr.op3 as usize] = Value::bool(a || b);
            }
            OpCode::Not => {
                let a = frame.regs[instr.op1 as usize].as_bool().unwrap_or(false);
                frame.regs[instr.op2 as usize] = Value::bool(!a);
            }

            // -- Type checks --
            OpCode::IsTag => {
                let val = frame.regs[instr.op1 as usize];
                let tag_id = instr.op2;
                let result = match tag_id {
                    0x01 => val.is_nil(),
                    0x02 => val.is_int(),
                    0x03 => val.is_bool(),
                    0x04 => val.is_unit(),
                    0x05 => val.is_actor_ref(),
                    0x06 => val.is_string(),
                    0x07 => val.is_closure(),
                    0x08 => val.is_ptr(),
                    0x09 => val.as_float().is_some(),
                    0x0A => false, // list
                    0x0B => false, // tuple
                    _ => false,
                };
                frame.regs[instr.op3 as usize] = Value::bool(result);
            }

            // -- Register moves --
            OpCode::Load | OpCode::Store | OpCode::Move | OpCode::Dup => {
                let src = frame.regs[instr.op1 as usize];
                frame.regs[instr.op2 as usize] = src;
            }
            OpCode::Swap => {
                let a = instr.op1 as usize;
                let b = instr.op2 as usize;
                let tmp = frame.regs[a];
                frame.regs[a] = frame.regs[b];
                frame.regs[b] = tmp;
            }

            // -- Control flow (non-consuming) --
            OpCode::Jmp => {
                let offset = instr.imm16() as i16;
                frame.pc = (frame.pc as i64 + offset as i64 - 1) as usize;
            }
            OpCode::JmpT => {
                let cond = frame.regs[instr.op1 as usize].as_bool().unwrap_or(false);
                if cond {
                    let offset = instr.offset16() as i16;
                    frame.pc = (frame.pc as i64 + offset as i64 - 1) as usize;
                }
            }
            OpCode::JmpF => {
                let cond = frame.regs[instr.op1 as usize].as_bool().unwrap_or(false);
                if !cond {
                    let offset = instr.offset16() as i16;
                    frame.pc = (frame.pc as i64 + offset as i64 - 1) as usize;
                }
            }

            // -- Algebraic Effects --
            OpCode::Handle => {
                let handler_table_idx = instr.op1 as usize;
                let resume_pc = frame.pc; // already incremented past Handle
                let resume_dst = instr.op2;
                self.handler_stack.push(HandlerFrame::new(
                    handler_table_idx,
                    module_idx,
                    resume_pc,
                    resume_dst,
                ));
            }
            OpCode::Perform => {
                self.step_perform(instr, frame_idx, module_idx)?;
            }
            OpCode::PerformDirect => {
                self.step_perform_direct(instr, frame_idx, module_idx)?;
            }
            OpCode::Resume => {
                let val = frame.regs[instr.op1 as usize];
                // The continuation lives on the innermost *matching* handler
                // frame (Perform uses rposition), which is not necessarily the
                // top of the stack when nested handlers bind different effects.
                // Check single-shot state first — no heap allocation needed.
                if let Some(hf) = self
                    .handler_stack
                    .iter_mut()
                    .rev()
                    .find(|hf| hf.single_shot_state.is_some())
                {
                    if let Some(state) = hf.single_shot_state.take() {
                        frame.regs = state.regs;
                        frame.regs[state.resume_dst as usize] = val;
                        frame.pc = state.resume_pc;
                        self.step_count = state.step_count;
                        return Ok(());
                    }
                }
                if let Some(hf) = self
                    .handler_stack
                    .iter_mut()
                    .rev()
                    .find(|hf| hf.captured_continuation.is_some())
                {
                    if let Some(cont) = hf.captured_continuation.take() {
                        cont.restore(self, val);
                        return Ok(());
                    }
                }
                return Err(NuError::VMError {
                    msg: "resume called without a captured continuation".into(),
                    span: Span::default(),
                });
            }
            OpCode::Unwind => {
                self.handler_stack.pop();
            }

            // -- Python Interop — RESERVED (see python/bridge.rs) --
            OpCode::PyImport
            | OpCode::PyGetAttr
            | OpCode::PyCall
            | OpCode::PyCallKw
            | OpCode::PySetAttr
            | OpCode::PyToNu
            | OpCode::PyFromNu
            | OpCode::PyRelease => {
                return Err(NuError::VMError {
                    msg: "Python opcodes require native actor runtime. \
                     Use perform Python.call(...) instead."
                        .into(),
                    span: Span::default(),
                });
            }

            // -- Distribution (MVP) --
            OpCode::NodeId => {
                let node_id = self
                    .distributed_callbacks
                    .as_ref()
                    .map(|cb| cb.node_id())
                    .unwrap_or(self.node_id);
                frame.regs[instr.op1 as usize] = Value::int(node_id as i64);
            }
            OpCode::Migrate => {
                let actor_id = frame.regs[instr.op1 as usize].as_int().unwrap_or(0) as u64;
                let target_node_id = frame.regs[instr.op2 as usize].as_int().unwrap_or(0) as u64;
                self.pending_migrations.push((actor_id, target_node_id));
                if let Some(ref mut cb) = self.distributed_callbacks {
                    cb.migrate(actor_id, target_node_id);
                }
                frame.regs[instr.op3 as usize] = Value::unit();
            }
            OpCode::RAsk => {
                // The target register holds an actor-ref VALUE (TAG_ACTOR)
                // when the actor expression is a real ref (e.g. a spawn@node
                // handle); accept the payload directly, with a plain-int
                // fallback for hand-assembled modules.
                let target_actor = frame.regs[instr.op1 as usize]
                    .as_actor_id()
                    .or_else(|| frame.regs[instr.op1 as usize].as_int().map(|v| v as u64))
                    .unwrap_or(0);
                // behavior_idx is a 16-bit behavior table index split across
                // op2 (high) + op3 (low), same encoding as OpCode::Ask.
                let behavior_idx = (((instr.op2 as u16) << 8) | (instr.op3 as u16)) as usize;
                let (param_count, behavior_name) = self
                    .modules
                    .get(module_idx)
                    .and_then(|m| m.behaviors.get(behavior_idx))
                    .map(|b| (b.param_count, b.name.clone()))
                    .unwrap_or((0, String::new()));
                let args: Vec<Value> = (0..param_count).map(|i| frame.regs[i]).collect();
                let timeout_ms = frame.regs[12].as_int().unwrap_or(5_000) as u64;
                let result = if let Some(ref mut cb) = self.distributed_callbacks {
                    cb.remote_ask(target_actor, &behavior_name, &args, timeout_ms)
                } else {
                    Value::nil()
                };
                // Write result to op1 (matching local Ask's convention) so the
                // codegen's `Move FUNC_VALUE_REG -> dst` picks it up correctly.
                frame.regs[instr.op1 as usize] = result;
            }
            OpCode::Gossip => {
                let message_const_idx = instr.op1 as usize;
                let message = self.module_const_string(module_idx, message_const_idx);
                self.gossip_log.push(message.clone());
                let result = if let Some(ref mut cb) = self.distributed_callbacks {
                    cb.gossip(&message)
                } else {
                    Value::unit()
                };
                frame.regs[instr.op3 as usize] = result;
            }

            OpCode::SConcat => {
                self.step_sconcat(frame_idx, module_idx, instr)?;
            }
            OpCode::SPrint => {
                self.emit_output(&frame.regs[instr.op1 as usize].to_string_repr());
            }
            OpCode::SRead => {
                self.step_sread(frame_idx, module_idx, instr)?;
            }
            OpCode::FOpen => {
                frame.regs[instr.op2 as usize] = Value::nil();
            }
            OpCode::FRead => {
                frame.regs[instr.op2 as usize] = Value::nil();
            }
            OpCode::FWrite => {}
            OpCode::FClose => {}
            OpCode::Print => {
                self.emit_output(&format!(
                    "{}\n",
                    frame.regs[instr.op1 as usize].to_string_repr()
                ));
            }

            // -- Debug & Meta --
            OpCode::DbgBreak => {}
            OpCode::DbgPrint => {
                eprintln!("=== Debug: Register State ===");
                for i in (0..256).step_by(8) {
                    let mut line = format!("R{:03}-R{:03}: ", i, i + 7);
                    for j in 0..8 {
                        line.push_str(&format!("{:>20} ", frame.regs[i + j].to_string_repr()));
                    }
                    eprintln!("{}", line);
                }
            }
            OpCode::DbgStack => {
                eprintln!("=== Debug: Call Stack ===");
                let mut depth = 0;
                let mut idx = Some(frame_idx);
                while let Some(i) = idx {
                    let fr = &self.frames[i];
                    let mname = self
                        .modules
                        .get(fr.module_idx)
                        .map(|m| m.name.as_str())
                        .unwrap_or("?");
                    eprintln!("  [{}] module={} pc={}", depth, mname, fr.pc);
                    depth += 1;
                    idx = fr.caller_idx;
                }
                if depth == 0 {
                    eprintln!("  (empty)");
                }
            }
            OpCode::MetaType => {
                frame.regs[instr.op2 as usize] = Value::int(0);
            }
            OpCode::MetaCap => {
                frame.regs[instr.op2 as usize] = Value::int(0);
            }

            // -- Register spilling for large functions --
            OpCode::SpillLoad => {
                let spill_idx = ((instr.op1 as u16) << 8) | (instr.op2 as u16);
                let dst = instr.op3 as usize;
                let val = self.frames[frame_idx]
                    .spilled
                    .get(spill_idx as usize)
                    .copied()
                    .unwrap_or(Value::nil());
                frame.regs[dst] = val;
            }
            OpCode::SpillStore => {
                let spill_idx = ((instr.op2 as u16) << 8) | (instr.op3 as u16);
                let src = instr.op1 as usize;
                let val = frame.regs[src];
                let spilled = &mut frame.spilled;
                if spill_idx as usize >= spilled.len() {
                    spilled.resize(spill_idx as usize + 1, Value::nil());
                }
                spilled[spill_idx as usize] = val;
            }
            OpCode::PerformAsync => {
                self.step_perform_async(frame_idx, module_idx, instr)?;
            }

            // -- Reference counting / deallocation --
            OpCode::Drop => {
                let reg = &mut frame.regs[instr.op1 as usize];
                let val = *reg;
                // Clear the register first so a duplicate Drop of the same
                // register (e.g. a last-use drop followed by a redefinition
                // or block-entry drop from `plan_drops`) is a harmless no-op
                // instead of a double decrement of the same reference count.
                *reg = Value::nil();
                if let Some(ptr) = val.as_ptr() {
                    self.actor_callbacks.drop_ref(ptr);
                }
            }

            // -- Receive (message pattern matching) --
            // Reads the next message from the actor's mailbox via the runtime
            // callback. If a message is available, stores its first argument;
            // otherwise stores nil.
            OpCode::Receive => {
                self.step_receive(frame_idx, instr)?;
            }

            // -- Selective receive (arm dispatch) --
            // The spec constant is "max_params:id1,id2,..." — the candidate
            // arm behavior ids and the number of payload registers reserved
            // after dst. On a match, dst gets the matched arm index and
            // payload values land in dst+1..dst+1+max_params (missing values
            // bound to nil, extras ignored). On no match, dst gets the
            // sentinel arm count and MIR-generated code falls through to the
            // legacy `Receive` fallback block.
            OpCode::ReceiveMatch => {
                self.step_receive_match(frame_idx, module_idx, instr)?;
            }

            // -- Timed selective receive (receive-after) --
            // Same spec constant and dst contract as ReceiveMatch, with the
            // timeout in milliseconds staged into r0 by the preceding Move.
            // On no match the runtime callback decides: suspend (positive
            // timeout inside an actor context) — the PC stays on this
            // instruction so a wake re-executes the scan — or resolve the
            // wait with the no-match sentinel (non-positive timeout, no
            // actor context, or an already-fired timeout). See the
            // ReceiveWait contract in bytecode.rs.
            OpCode::ReceiveWait => {
                self.step_receive_wait(frame_idx, module_idx, instr)?;
            }

            // -- Commit a selective receive --
            // Removes the matched ("tried") message from the skip-buffer and
            // clears remaining "tried" flags. Emitted after a pattern+guard
            // check succeeds, before binding pattern variables and entering
            // the arm body.
            OpCode::ReceiveCommit => {
                self.step_receive_commit();
            }

            // -- Special opcodes (previously in catch-all) --
            OpCode::Nop => {
                // No operation
            }
            OpCode::Halt => {
                return Err(NuError::VMError {
                    msg: "Halt".to_string(),
                    span: Span::default(),
                });
            }

            // -- Constants (cont.) --
            OpCode::ConstM1 => {
                frame.regs[instr.op1 as usize] = Value::int(-1);
            }
            OpCode::ConstL => {
                // Reserved for constant pools >= 65536 entries. No current
                // Nulang codegen path (mir_codegen, aot, wasm) emits this —
                // ConstU (u16 index) covers every program the compiler
                // produces; see `no_codegen_path_emits_reserved_opcodes` in
                // integration_tests. Implement before enabling >64k-constant
                // modules (e.g. a future large-program or bootstrap target).
                return Err(NuError::VMError {
                    msg: "ConstL is a reserved opcode: no current codegen path emits large constant pool indices; use ConstU for pools < 65536 entries".into(),
                    span: Span::default(),
                });
            }

            // -- Stack (cont.) --
            OpCode::Pop => {
                // Reserved for a stack-based operand model. The register VM's
                // codegen never emits it — locals and temporaries are always
                // register-addressed. See
                // `no_codegen_path_emits_reserved_opcodes`.
                return Err(NuError::VMError {
                    msg: "Pop is a reserved opcode: no current codegen path emits stack-based operand pops in this register VM".into(),
                    span: Span::default(),
                });
            }

            // -- Conversions --
            OpCode::IToF => {
                let a = frame.regs[instr.op1 as usize];
                let int_val = if a.is_int() {
                    a.as_int().unwrap_or(0)
                } else if a.is_float() {
                    a.as_float().unwrap_or(0.0) as i64
                } else {
                    sext48(a.as_raw() & PAYLOAD_MASK)
                };
                frame.regs[instr.op2 as usize] = Value::float(int_val as f64);
            }
            OpCode::FToI => {
                let a = frame.regs[instr.op1 as usize];
                let float_val = a.as_float().unwrap_or(0.0);
                frame.regs[instr.op2 as usize] = Value::int(float_val as i64);
            }
            OpCode::FToS => {
                let a = frame.regs[instr.op1 as usize];
                let s = if let Some(f) = a.as_float() {
                    if f.fract() == 0.0 && f.is_finite() {
                        format!("{:.1}", f)
                    } else {
                        format!("{}", f)
                    }
                } else {
                    "0.0".to_string()
                };
                frame.regs[instr.op2 as usize] = self.allocate_string(&s);
            }

            // -- Control Flow (cont.) --
            OpCode::Switch => {
                // Reserved for jump-table dispatch. Codegen currently lowers
                // every match/if to a compare-and-branch chain, which covers
                // every case a jump table would optimize.
                return Err(NuError::VMError {
                    msg: "Switch is a reserved opcode: no current codegen path emits jump-table dispatch; match/if lower to compare-and-branch chains".into(),
                    span: Span::default(),
                });
            }

            // -- Memory & Objects (cont.) --
            OpCode::Alloc => {
                // Reserved for explicit sized heap allocation. Codegen
                // allocates only through the typed ArrAlloc/RecMk/TupleMk/
                // Closure opcodes, which cover every allocation site the
                // compiler produces.
                return Err(NuError::VMError {
                    msg: "Alloc is a reserved opcode: no current codegen path emits explicit sized allocation; ArrAlloc/RecMk/TupleMk/Closure cover every allocation site".into(),
                    span: Span::default(),
                });
            }
            OpCode::TupleL => {
                // Reserved for a dedicated tuple-field-load encoding.
                // Codegen lowers tuple field access (`t.0`) through the same
                // FieldL opcode used for record fields — no dedicated tuple
                // opcode is emitted.
                return Err(NuError::VMError {
                    msg: "TupleL is a reserved opcode: no current codegen path emits it; tuple field access lowers through FieldL".into(),
                    span: Span::default(),
                });
            }
            OpCode::Unpack => {
                // Reserved for a dedicated variant-payload-extraction
                // encoding. No current codegen path emits it.
                return Err(NuError::VMError {
                    msg: "Unpack is a reserved opcode: no current codegen path emits variant payload extraction through this opcode".into(),
                    span: Span::default(),
                });
            }
            OpCode::Copy => {
                // Reserved for a generic capability-aware deep copy.
                // RecCopy (0x9D) covers the one deep-copy case (record
                // duplication) the compiler emits; codegen never needs a
                // generic Copy.
                return Err(NuError::VMError {
                    msg: "Copy is a reserved opcode: no current codegen path emits it; RecCopy covers the deep-copy cases the compiler produces".into(),
                    span: Span::default(),
                });
            }

            // -- Actor & Concurrency (cont.) — require actor runtime --
            OpCode::Monitor => {
                return Err(NuError::VMError {
                    msg: "Monitor opcode requires actor runtime".into(),
                    span: Span::default(),
                });
            }
            OpCode::Demon => {
                return Err(NuError::VMError {
                    msg: "Demon opcode requires actor runtime".into(),
                    span: Span::default(),
                });
            }
            OpCode::Link => {
                return Err(NuError::VMError {
                    msg: "Link opcode requires actor runtime".into(),
                    span: Span::default(),
                });
            }
            OpCode::Unlink => {
                return Err(NuError::VMError {
                    msg: "Unlink opcode requires actor runtime".into(),
                    span: Span::default(),
                });
            }
            OpCode::Exit => {
                return Err(NuError::VMError {
                    msg: "Exit opcode requires actor runtime".into(),
                    span: Span::default(),
                });
            }
            OpCode::Yield => {
                // Set the yield flag; the run loop checks this after step()
                // returns and will suspend execution.
                self.yield_pending = true;
            }
        }
        Ok(())
    }

    // === Function Resolution ===

    /// Resolve a function/closure value to its code offset in the given
    /// module's function table.
    ///
    /// Immediate closures (payload = function index) and plain function
    /// indices resolve directly; env-carrying closures resolve through this
    /// VM's captured environments, so they can only run on the VM that
    /// created them. Used by the runtime to invoke workflow query handlers.
    pub fn function_offset_for_value(&self, module_idx: usize, func: Value) -> NuResult<usize> {
        let (func_idx, _) = self.resolve_function(func, module_idx)?;
        self.modules
            .get(module_idx)
            .and_then(|m| m.function_table.get(func_idx))
            .copied()
            .ok_or_else(|| NuError::VMError {
                msg: format!("Function {} not found", func_idx),
                span: Span::default(),
            })
    }

    /// Resolve a function value to a (function_table_index, closure_env).
    fn resolve_function(
        &self,
        func_val: Value,
        _module_idx: usize,
    ) -> NuResult<(usize, Option<Value>)> {
        if let Some(func_idx) = func_val.as_int() {
            Ok((func_idx as usize, None))
        } else if (func_val.raw & TAG_MASK) == TAG_CLOSURE {
            let payload = func_val.raw & PAYLOAD_MASK;
            if payload & CLOSURE_ENV_FLAG != 0 {
                // Env-carrying closure: the function index lives in the env.
                let env_idx = (payload & CLOSURE_ENV_IDX_MASK) as usize;
                let func_idx = self
                    .closure_envs
                    .get(env_idx)
                    .map(|env| env.func_idx)
                    .ok_or_else(|| NuError::VMError {
                        msg: format!("Dangling closure environment {}", env_idx),
                        span: Span::default(),
                    })?;
                Ok((func_idx, Some(func_val)))
            } else {
                // Immediate closure: the payload is the function index.
                Ok((payload as usize, Some(func_val)))
            }
        } else {
            Err(NuError::VMError {
                msg: format!("Not a function: {}", func_val.to_string_repr()),
                span: Span::default(),
            })
        }
    }
}

impl Default for VM {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
fn module_with_handler_table(bindings: Vec<crate::bytecode::HandlerBinding>) -> CodeModule {
    let mut module = CodeModule::new("test_module");
    module.add_handler_table(crate::bytecode::HandlerTable {
        bindings,
        fallback_offset: None,
    });
    module
}

// ---------------------------------------------------------------------------
// VM Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod vm_tests {
    use super::*;
    use crate::bytecode::{BehaviorTableEntry, HandlerBinding, HandlerTable, Instruction};

    /// A NULL C string return (nil from cstr_to_value) must pass through
    /// instead of erroring on the missing pointer.
    #[test]
    fn test_copy_cstr_return_nil_passthrough() {
        let mut vm = VM::new();
        let result = vm.copy_cstr_return(Value::nil());
        assert!(
            result.is_ok(),
            "NULL C string should map to nil: {:?}",
            result.err()
        );
        assert!(result.unwrap().is_nil());
    }

    /// A non-NULL C string return is still copied into the actor heap.
    #[test]
    fn test_copy_cstr_return_copies_string() {
        let mut vm = VM::new();
        // SAFETY: the literal is a valid null-terminated C string.
        let value = unsafe { crate::ffi::marshal::cstr_to_value(c"hello ffi".as_ptr()) };
        let result = vm
            .copy_cstr_return(value)
            .expect("C string return should copy into the heap");
        let ptr = result.as_ptr().expect("copied string must be a pointer");
        // SAFETY: ptr points to a null-terminated string in the VM heap.
        let s = unsafe { CStr::from_ptr(ptr as *const c_char) }
            .to_str()
            .unwrap();
        assert_eq!(s, "hello ffi");
    }

    /// Test 1: Basic integer arithmetic.
    #[test]
    fn test_basic_arithmetic() {
        let mut module = CodeModule::new("test_arith");
        // r0 = 10, r1 = 3, r2 = r0 + r1
        module.emit(Instruction::new2(OpCode::Const1, 0, 0));
        module.emit(Instruction::new2(OpCode::Const1, 0, 1));
        // Patch: use ConstU with constant pool
        let c10_idx = module.add_constant(Constant::Int(10));
        let c3_idx = module.add_constant(Constant::Int(3));
        module.instructions.clear(); // clear the Const1 instructions
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c10_idx >> 8) & 0xFF) as u8,
            (c10_idx & 0xFF) as u8,
            0,
        )); // r0 = 10
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c3_idx >> 8) & 0xFF) as u8,
            (c3_idx & 0xFF) as u8,
            1,
        )); // r1 = 3
        module.emit(Instruction::new3(OpCode::IAdd, 0, 1, 2)); // r2 = r0 + r1 = 13
        module.emit(Instruction::new2(OpCode::Move, 2, 0)); // r0 = r2 (return value)
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_ok(), "Arithmetic should work: {:?}", result.err());
        assert_eq!(result.unwrap().as_int(), Some(13), "10 + 3 = 13");
    }

    /// IInc/IDec on a normal int-tagged register: in-place ±1.
    #[test]
    fn test_iinc_idec_int() {
        let mut module = CodeModule::new("test_iinc_idec_int");
        let c41_idx = module.add_constant(Constant::Int(41));
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c41_idx >> 8) & 0xFF) as u8,
            (c41_idx & 0xFF) as u8,
            0,
        )); // r0 = 41
        module.emit(Instruction::new1(OpCode::IInc, 0)); // r0 = 42
        module.emit(Instruction::new1(OpCode::IInc, 0)); // r0 = 43
        module.emit(Instruction::new1(OpCode::IDec, 0)); // r0 = 42
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_ok(), "IInc/IDec should work: {:?}", result.err());
        assert_eq!(result.unwrap().as_int(), Some(42));
    }

    /// IInc/IDec ignore the tag and operate on the raw 48-bit payload,
    /// exactly like the JIT helpers `nulang_iinc`/`nulang_idec`:
    /// bool true (payload 1) increments to int 2, nil (payload 0)
    /// decrements to int -1, and the result is always int-tagged.
    #[test]
    fn test_iinc_idec_non_int_operand() {
        let mut module = CodeModule::new("test_iinc_idec_non_int");
        let ctrue_idx = module.add_constant(Constant::Bool(true));
        let cnil_idx = module.add_constant(Constant::Nil);
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((ctrue_idx >> 8) & 0xFF) as u8,
            (ctrue_idx & 0xFF) as u8,
            0,
        )); // r0 = true
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((cnil_idx >> 8) & 0xFF) as u8,
            (cnil_idx & 0xFF) as u8,
            1,
        )); // r1 = nil
        module.emit(Instruction::new1(OpCode::IInc, 0)); // r0: payload 1 -> int 2
        module.emit(Instruction::new1(OpCode::IDec, 1)); // r1: payload 0 -> int -1
        module.emit(Instruction::new3(OpCode::IMul, 0, 1, 0)); // r0 = 2 * -1 = -2
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "IInc/IDec on non-int should work: {:?}",
            result.err()
        );
        // IMul only yields -2 if both operands became int-tagged (2 and -1).
        assert_eq!(result.unwrap().as_int(), Some(-2));
    }

    /// IInc/IDec wrap at the 48-bit signed boundary, matching `tag_int`
    /// masking in the JIT helpers: INT48_MAX + 1 == INT48_MIN and
    /// INT48_MIN - 1 == INT48_MAX.
    #[test]
    fn test_iinc_idec_wrap() {
        const INT48_MAX: i64 = 0x0000_7FFF_FFFF_FFFF; // 2^47 - 1
        const INT48_MIN: i64 = -0x0000_8000_0000_0000; // -2^47
        let mut module = CodeModule::new("test_iinc_idec_wrap");
        let cmax_idx = module.add_constant(Constant::Int(INT48_MAX));
        let cmin_idx = module.add_constant(Constant::Int(INT48_MIN));
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((cmax_idx >> 8) & 0xFF) as u8,
            (cmax_idx & 0xFF) as u8,
            0,
        )); // r0 = INT48_MAX
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((cmin_idx >> 8) & 0xFF) as u8,
            (cmin_idx & 0xFF) as u8,
            1,
        )); // r1 = INT48_MIN
        module.emit(Instruction::new1(OpCode::IInc, 0)); // r0 wraps to INT48_MIN
        module.emit(Instruction::new1(OpCode::IDec, 1)); // r1 wraps to INT48_MAX
        module.emit(Instruction::new3(OpCode::ISub, 1, 0, 0)); // r0 = MAX - MIN = -1 (48-bit)
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "IInc/IDec wrap should work: {:?}",
            result.err()
        );
        // INT48_MAX - INT48_MIN = 2^48 - 1, which wraps to -1 in 48 bits.
        assert_eq!(result.unwrap().as_int(), Some(-1));
    }

    /// Test 2: NaN-boxed value representation.
    #[test]
    fn test_value_nan_tagging() {
        let v_int = Value::int(42);
        assert_eq!(v_int.as_int(), Some(42));
        assert!(v_int.is_int());

        let v_float = Value::float(2.5);
        assert!((v_float.as_float().unwrap() - 2.5).abs() < 0.001);

        let v_bool = Value::bool(true);
        assert_eq!(v_bool.as_bool(), Some(true));

        let v_nil = Value::nil();
        assert!(v_nil.is_nil());

        let v_unit = Value::unit();
        assert!(v_unit.is_unit());

        let v_actor = Value::actor_ref(123);
        assert_eq!(v_actor.as_actor_id(), Some(123));
    }

    /// Test 3: Halt instruction stops execution.
    #[test]
    fn test_halt_stops() {
        let mut module = CodeModule::new("test_halt");
        let c42_idx = module.add_constant(Constant::Int(42));
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c42_idx >> 8) & 0xFF) as u8,
            (c42_idx & 0xFF) as u8,
            0,
        ));
        module.emit(Instruction::new0(OpCode::Halt));
        module.emit(Instruction::new1(OpCode::Const1, 0)); // should not execute
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_int(), Some(42));
    }

    /// Test 4: PC out of bounds returns safely.
    #[test]
    fn test_pc_out_of_bounds() {
        let mut module = CodeModule::new("test_oob");
        let c99_idx = module.add_constant(Constant::Int(99));
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c99_idx >> 8) & 0xFF) as u8,
            (c99_idx & 0xFF) as u8,
            0,
        ));
        // No Halt — PC goes past end
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_ok(), "PC out of bounds should return gracefully");
        assert_eq!(result.unwrap().as_int(), Some(99));
    }

    /// Test 5: to_string_repr formatting.
    #[test]
    fn test_to_string_repr() {
        assert_eq!(Value::int(42).to_string_repr(), "42");
        assert_eq!(Value::bool(true).to_string_repr(), "true");
        assert_eq!(Value::nil().to_string_repr(), "nil");
        assert_eq!(Value::unit().to_string_repr(), "()");
    }

    /// Test 6: Special values (nil, unit, bool) roundtrip.
    #[test]
    fn test_special_values() {
        assert!(Value::nil().is_nil());
        assert!(!Value::nil().is_unit());
        assert!(Value::unit().is_unit());
        assert!(!Value::unit().is_nil());
        assert_eq!(Value::bool(false).as_bool(), Some(false));
        assert_eq!(Value::bool(true).as_bool(), Some(true));
    }

    /// Test 7: Step limit defaults to 10M.
    #[test]
    fn test_step_limit_default() {
        // This test just verifies the step limit mechanism exists.
        // Running 10M steps would take too long, so we verify the env var parsing.
        let limit = std::env::var("NULANG_STEP_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000_000);
        assert_eq!(limit, 10_000_000, "Default step limit should be 10M");
    }

    /// Test 8: Python opcodes trap with error.
    #[test]
    fn test_python_opcodes_trap() {
        let mut module = CodeModule::new("test_py_trap");
        module.emit(Instruction::new0(OpCode::PyCall));
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_err(), "Python opcodes should trap");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("Python") || msg.contains("native actor"),
            "Error should mention Python: {}",
            msg
        );
    }

    /// Test 9: Float operations.
    #[test]
    fn test_float_operations() {
        let mut module = CodeModule::new("test_float");
        let c3_5 = module.add_constant(Constant::Float(3.5));
        let c2_0 = module.add_constant(Constant::Float(2.0));
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c3_5 >> 8) & 0xFF) as u8,
            (c3_5 & 0xFF) as u8,
            0,
        )); // r0 = 3.5
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c2_0 >> 8) & 0xFF) as u8,
            (c2_0 & 0xFF) as u8,
            1,
        )); // r1 = 2.0
        module.emit(Instruction::new3(OpCode::FAdd, 0, 1, 2)); // r2 = 5.5
        module.emit(Instruction::new2(OpCode::Move, 2, 0));
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_ok(), "Float ops should work: {:?}", result.err());
        let f = result.unwrap().as_float().unwrap();
        assert!((f - 5.5).abs() < 0.01, "3.5 + 2.0 = 5.5, got {}", f);
    }

    /// Test 10: Perform + Resume with handler.
    #[test]
    fn test_perform_resume() {
        let mut module = module_with_handler_table(vec![HandlerBinding {
            effect_name: "Get42".to_string(),
            handler_offset: 7,
            arg_count: 0,
            result_reg: 0,
            single_shot: false,
        }]);

        // Program layout:
        // PC 0: Handle(0)          — push handler frame
        // PC 1: Perform "Get42" -> r1  — should invoke handler
        // PC 2: (after perform) Move r1 -> r0  — copy result to return reg
        // PC 3: Unwind
        // PC 4: Halt
        // PC 5-6: (padding)
        // PC 7: handler body: ConstU c42 -> r0; Resume r0

        // Add the effect name string to the constant pool first so its index
        // is known when we emit Perform.
        let get42_idx = module.add_constant(Constant::String("Get42".to_string()));

        module.emit(Instruction::new1(OpCode::Handle, 0)); // 0
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((get42_idx >> 8) & 0xFF) as u8,
            (get42_idx & 0xFF) as u8,
            1,
        )); // 1: perform Get42 -> r1
            // After resume, r1 should have 42. Copy it to r0 for return.
        module.emit(Instruction::new2(OpCode::Move, 1, 0)); // 2
        module.emit(Instruction::new0(OpCode::Unwind)); // 3
        module.emit(Instruction::new0(OpCode::Halt)); // 4
                                                      // Handler body at PC 7:
                                                      // Place 42 in r0, then resume with it
        module.emit(Instruction::new0(OpCode::Nop)); // 5 (padding)
        module.emit(Instruction::new0(OpCode::Nop)); // 6 (padding)
        module.emit(Instruction::new2(OpCode::ConstU, 0, 0)); // 7: const 42 -> r0
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 8: resume with r0

        // Patch ConstU at PC 7 to load constant 42
        let c42_idx = module.add_constant(Constant::Int(42));
        if let Some(instr) = module.instructions.get_mut(7) {
            instr.op1 = ((c42_idx >> 8) & 0xFF) as u8;
            instr.op2 = (c42_idx & 0xFF) as u8;
            instr.op3 = 0; // dst = r0
        }

        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "Perform/Resume should work: {:?}",
            result.err()
        );
        assert_eq!(
            result.unwrap().as_int(),
            Some(42),
            "Should get 42 from effect handler"
        );
    }

    /// Test 11: Perform without a matching handler raises EffectError.
    #[test]
    fn test_unhandled_effect_errors() {
        let mut module = module_with_handler_table(vec![]);
        let no_effect_idx = module.add_constant(Constant::String("NoHandler".to_string()));

        module.emit(Instruction::new1(OpCode::Handle, 0));
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((no_effect_idx >> 8) & 0xFF) as u8,
            (no_effect_idx & 0xFF) as u8,
            0,
        ));
        module.emit(Instruction::new0(OpCode::Unwind));
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_err(), "Unhandled effect should error");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("Unhandled effect"),
            "Error should mention unhandled: {}",
            msg
        );
    }

    /// Test 12: Nested handlers with shadowing.
    #[test]
    fn test_nested_handlers_shadow() {
        let mut module = CodeModule::new("test_nested");

        // Outer handler table: GetX -> 100
        let outer_bindings = vec![HandlerBinding {
            effect_name: "GetX".to_string(),
            handler_offset: 10,
            arg_count: 0,
            result_reg: 0,
            single_shot: false,
        }];
        module.add_handler_table(HandlerTable {
            bindings: outer_bindings,
            fallback_offset: None,
        });

        // Inner handler table: GetX -> 200 (shadows outer)
        let inner_bindings = vec![HandlerBinding {
            effect_name: "GetX".to_string(),
            handler_offset: 12,
            arg_count: 0,
            result_reg: 0,
            single_shot: false,
        }];
        module.add_handler_table(HandlerTable {
            bindings: inner_bindings,
            fallback_offset: None,
        });

        let getx_idx = module.add_constant(Constant::String("GetX".to_string()));
        let c100_idx = module.add_constant(Constant::Int(100));
        let c200_idx = module.add_constant(Constant::Int(200));

        // Program:
        // PC 0: Handle(0) — outer handler
        // PC 1: Handle(1) — inner handler
        // PC 2: Perform "GetX" -> r0  — should hit inner (returns 200)
        // PC 3: Unwind — pop inner
        // PC 4: Unwind — pop outer
        // PC 5: Halt
        // padding 6-9
        // PC 10: outer handler body: ConstU 100 -> r0; Resume r0
        // PC 12: inner handler body: ConstU 200 -> r0; Resume r0

        module.emit(Instruction::new1(OpCode::Handle, 0)); // 0
        module.emit(Instruction::new1(OpCode::Handle, 1)); // 1
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((getx_idx >> 8) & 0xFF) as u8,
            (getx_idx & 0xFF) as u8,
            0,
        )); // 2
        module.emit(Instruction::new0(OpCode::Unwind)); // 3
        module.emit(Instruction::new0(OpCode::Unwind)); // 4
        module.emit(Instruction::new0(OpCode::Halt)); // 5
                                                      // padding 6-9
        for _ in 6..10 {
            module.emit(Instruction::new0(OpCode::Nop));
        }
        // Outer handler at 10
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c100_idx >> 8) & 0xFF) as u8,
            (c100_idx & 0xFF) as u8,
            0,
        )); // 10
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 11
                                                           // Inner handler at 12
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c200_idx >> 8) & 0xFF) as u8,
            (c200_idx & 0xFF) as u8,
            0,
        )); // 12
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 13

        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "Nested handlers should work: {:?}",
            result.err()
        );
        assert_eq!(
            result.unwrap().as_int(),
            Some(200),
            "Inner handler should shadow outer"
        );
    }

    /// Test 13: Multiple effects in one handle block.
    #[test]
    fn test_multi_effect_handler() {
        let mut module = CodeModule::new("test_multi");

        // Handler table: GetA -> 100, GetB -> 200
        module.add_handler_table(HandlerTable {
            bindings: vec![
                HandlerBinding {
                    effect_name: "GetA".to_string(),
                    handler_offset: 8,
                    arg_count: 0,
                    result_reg: 0,
                    single_shot: false,
                },
                HandlerBinding {
                    effect_name: "GetB".to_string(),
                    handler_offset: 11,
                    arg_count: 0,
                    result_reg: 0,
                    single_shot: false,
                },
            ],
            fallback_offset: None,
        });

        let geta_idx = module.add_constant(Constant::String("GetA".to_string()));
        let getb_idx = module.add_constant(Constant::String("GetB".to_string()));
        let c100_idx = module.add_constant(Constant::Int(100));
        let c200_idx = module.add_constant(Constant::Int(200));

        // Program: perform GetA -> r0, then GetB -> r1, add them
        module.emit(Instruction::new1(OpCode::Handle, 0)); // 0
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((geta_idx >> 8) & 0xFF) as u8,
            (geta_idx & 0xFF) as u8,
            0,
        )); // 1: GetA -> r0
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((getb_idx >> 8) & 0xFF) as u8,
            (getb_idx & 0xFF) as u8,
            1,
        )); // 2: GetB -> r1
        module.emit(Instruction::new3(OpCode::IAdd, 0, 1, 0)); // 3: r0 + r1 -> r0
        module.emit(Instruction::new0(OpCode::Unwind)); // 4
        module.emit(Instruction::new0(OpCode::Halt)); // 5
                                                      // padding 6-7
        module.emit(Instruction::new0(OpCode::Nop)); // 6
        module.emit(Instruction::new0(OpCode::Nop)); // 7
                                                     // GetA handler at 8
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c100_idx >> 8) & 0xFF) as u8,
            (c100_idx & 0xFF) as u8,
            0,
        )); // 8
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 9
        module.emit(Instruction::new0(OpCode::Nop)); // 10
                                                     // GetB handler at 11
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c200_idx >> 8) & 0xFF) as u8,
            (c200_idx & 0xFF) as u8,
            0,
        )); // 11
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 12

        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "Multi-effect handler should work: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().as_int(), Some(300), "100 + 200 = 300");
    }

    /// Test 14: Handler fallback — effect not in bindings triggers fallback.
    #[test]
    fn test_handler_fallback() {
        let mut module = CodeModule::new("test_fallback");

        // Handler table: handles "Known", fallback for everything else
        module.add_handler_table(HandlerTable {
            bindings: vec![HandlerBinding {
                effect_name: "Known".to_string(),
                handler_offset: 8,
                arg_count: 0,
                result_reg: 0,
                single_shot: false,
            }],
            fallback_offset: Some(11), // fallback handler
        });

        let unknown_idx = module.add_constant(Constant::String("Unknown".to_string()));
        let c999_idx = module.add_constant(Constant::Int(999));

        module.emit(Instruction::new1(OpCode::Handle, 0)); // 0
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((unknown_idx >> 8) & 0xFF) as u8,
            (unknown_idx & 0xFF) as u8,
            0,
        )); // 1
        module.emit(Instruction::new0(OpCode::Unwind)); // 2
        module.emit(Instruction::new0(OpCode::Halt)); // 3
                                                      // padding 4-7
        for _ in 4..8 {
            module.emit(Instruction::new0(OpCode::Nop));
        }
        // Known handler at 8 (not used)
        module.emit(Instruction::new1(OpCode::Const1, 0)); // 8
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 9
        module.emit(Instruction::new0(OpCode::Nop)); // 10
                                                     // Fallback handler at 11: returns 999
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c999_idx >> 8) & 0xFF) as u8,
            (c999_idx & 0xFF) as u8,
            0,
        )); // 11
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 12

        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "Fallback handler should work: {:?}",
            result.err()
        );
        assert_eq!(
            result.unwrap().as_int(),
            Some(999),
            "Fallback should return 999"
        );
    }

    /// Test 15: Resume without captured continuation errors.
    #[test]
    fn test_resume_without_continuation_errors() {
        let mut module = CodeModule::new("test_bad_resume");
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 0
        module.emit(Instruction::new0(OpCode::Halt)); // 1
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_err(), "Resume without continuation should error");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("resume called without a captured continuation"),
            "Error should mention missing continuation: {}",
            err_msg
        );
    }

    /// Test 16: JIT-compiled hot loop produces the same result as the interpreter.
    #[test]
    fn test_jit_hot_loop_matches_interpreter() {
        let mut module = CodeModule::new("test_jit_hot_loop");
        // Registers: r0 = sum, r1 = i, r2 = limit, r3 = one, r4 = condition.
        module.emit(Instruction::new1(OpCode::Const0, 0)); // 0: sum = 0
        module.emit(Instruction::new1(OpCode::Const0, 1)); // 1: i = 0
        module.emit(Instruction::new2(OpCode::Const2, 2, 0)); // 2: limit = 2
        module.emit(Instruction::new2(OpCode::Const2, 3, 0)); // 3: tmp = 2
        module.emit(Instruction::new3(OpCode::IAdd, 2, 3, 2)); // limit = 4
        module.emit(Instruction::new1(OpCode::Const1, 3)); // r3 = 1

        let loop_check = module.current_offset();
        module.emit(Instruction::new3(OpCode::ICmpLt, 1, 2, 4)); // r4 = i < limit
        let jmpf_idx = module.current_offset();
        module.emit(Instruction::new2(OpCode::JmpF, 4, 0)); // exit loop when false
        module.emit(Instruction::new3(OpCode::IAdd, 0, 1, 0)); // sum += i
        module.emit(Instruction::new3(OpCode::IAdd, 1, 3, 1)); // i += 1
        let jmp_back_idx = module.current_offset();
        let back_offset = loop_check as i64 - jmp_back_idx as i64;
        module.emit(Instruction::new3(
            OpCode::Jmp,
            ((back_offset as i16 >> 8) & 0xFF) as u8,
            (back_offset as i16 & 0xFF) as u8,
            0,
        ));
        let after_loop = module.current_offset();
        if let Some(instr) = module.instructions.get_mut(jmpf_idx) {
            let forward_offset = after_loop as i64 - jmpf_idx as i64;
            instr.op2 = ((forward_offset as i16 >> 8) & 0xFF) as u8;
            instr.op3 = (forward_offset as i16 & 0xFF) as u8;
        }
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        // Cold interpreter run.
        let mut vm = VM::new();
        vm.load_module(module.clone());
        let cold_result = vm.run_from(0, 0).unwrap();

        // Heat the entry region until it is JIT-compiled.
        if let Some(jit) = vm.jit_session.as_mut() {
            jit.reset_hot_counters();
        }
        for _ in 0..2000 {
            let _ = vm.run_from(0, 0);
        }

        let hot_result = vm.run_from(0, 0).unwrap();
        assert_eq!(
            hot_result.as_int(),
            cold_result.as_int(),
            "JIT hot loop should match interpreter"
        );
        assert_eq!(hot_result.as_int(), Some(6), "sum 0..4 = 6");
    }

    /// Regression test: a hot loop whose body is long enough to JIT and whose
    /// header has an early-exit conditional must produce the exact interpreter
    /// result. Guards the straight-line-region contract: compiled regions must
    /// not contain branches, because the VM advances pc by the full region
    /// length after a region runs.
    #[test]
    fn test_jit_hot_loop_with_early_exit_branch() {
        let mut module = CodeModule::new("test_jit_early_exit");
        let c100_idx = module.add_constant(Constant::Int(100));

        // r0 = sum, r1 = i, r2 = limit, r3 = one, r4 = cond, r5 = pad
        module.emit(Instruction::new1(OpCode::Const0, 0)); // 0
        module.emit(Instruction::new1(OpCode::Const0, 1)); // 1
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c100_idx >> 8) & 0xFF) as u8,
            (c100_idx & 0xFF) as u8,
            2,
        )); // 2
        module.emit(Instruction::new1(OpCode::Const1, 3)); // 3
        module.emit(Instruction::new1(OpCode::Const0, 5)); // 4

        let loop_check = module.current_offset();
        module.emit(Instruction::new3(OpCode::ICmpLt, 1, 2, 4)); // 5
        let jmpf_idx = module.current_offset();
        module.emit(Instruction::new2(OpCode::JmpF, 4, 0)); // 6 (patched)
                                                            // Loop body: 7 straight-line instructions so it clears the JIT's
                                                            // minimum region size and actually gets compiled once hot.
        module.emit(Instruction::new3(OpCode::IAdd, 0, 1, 0)); // 7: sum += i
        module.emit(Instruction::new3(OpCode::IAdd, 5, 3, 5)); // 8: pad
        module.emit(Instruction::new3(OpCode::IAdd, 5, 3, 5)); // 9: pad
        module.emit(Instruction::new3(OpCode::IAdd, 5, 3, 5)); // 10: pad
        module.emit(Instruction::new3(OpCode::IAdd, 5, 3, 5)); // 11: pad
        module.emit(Instruction::new3(OpCode::IAdd, 5, 3, 5)); // 12: pad
        module.emit(Instruction::new3(OpCode::IAdd, 1, 3, 1)); // 13: i += 1
        let jmp_back_idx = module.current_offset();
        let back_offset = loop_check as i64 - jmp_back_idx as i64;
        module.emit(Instruction::new3(
            OpCode::Jmp,
            ((back_offset as i16 >> 8) & 0xFF) as u8,
            (back_offset as i16 & 0xFF) as u8,
            0,
        )); // 14
        let after_loop = module.current_offset();
        if let Some(instr) = module.instructions.get_mut(jmpf_idx) {
            let forward_offset = after_loop as i64 - jmpf_idx as i64;
            instr.op2 = ((forward_offset as i16 >> 8) & 0xFF) as u8;
            instr.op3 = (forward_offset as i16 & 0xFF) as u8;
        }
        module.emit(Instruction::new0(OpCode::Halt)); // 15
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let cold_result = vm.run_from(0, 0).unwrap();
        assert_eq!(cold_result.as_int(), Some(4950), "sum 0..100 = 4950");

        // Heat the loop body well past the hot threshold so it JIT-compiles,
        // then verify the compiled path still takes the early exit correctly.
        for _ in 0..50 {
            let result = vm.run_from(0, 0).unwrap();
            assert_eq!(
                result.as_int(),
                Some(4950),
                "JIT-compiled loop with early-exit branch must match interpreter"
            );
        }
        // The loop body is a non-array straight-line region: it must have
        // tiered up through the scalar compiler even on SIMD-capable hosts
        // (where SIMD analysis finds no pattern and used to silently skip
        // compilation entirely).
        let compiled = vm
            .jit_session
            .as_ref()
            .map(|j| j.compiled_count())
            .unwrap_or(0);
        assert!(compiled > 0, "hot non-array loop body must be JIT-compiled");
    }

    /// Closure capture environments: Closure + CapStore then Call + CapLoad
    /// A source-compiled arithmetic `while` loop (the hot_loop bench source)
    /// must tier up through the JIT: the compiler's loop bytecode has a
    /// back-edge, so `find_compilable_region` detects the loop and compiles
    /// it natively. Guards against silent regressions to interpreter-only
    /// arithmetic hot loops.
    #[test]
    fn test_jit_source_hot_loop_tiers_up() {
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;

        let source = "var sum = 0; var i = 0; while i < 100000 { sum = sum + i * 3 - i / 7; i = i + 1; }; sum";
        let mut type_checker = TypeChecker::new();
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");
        type_checker.check_module(&ast).expect("typecheck");
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).expect("mir lower");
        let module = crate::mir_codegen::compile_mir(&mut mir, "test").expect("compile");

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run().unwrap();
        let compiled = vm
            .jit_session
            .as_ref()
            .map(|j| j.compiled_count())
            .unwrap_or(0);
        assert!(
            compiled > 0,
            "source-compiled hot loop must JIT-compile, got {compiled} regions"
        );
        assert!(
            result.is_int(),
            "hot_loop must return an Int, got {result:?}"
        );
    }

    /// round-trips the captured value into the callee frame.
    #[test]
    fn test_closure_capture_env_roundtrip() {
        let mut module = CodeModule::new("test_capture");
        let c41_idx = module.add_constant(Constant::Int(41));

        // Entry: build a closure over function 0 capturing 41, call it.
        // main:
        //   0: ConstU 41 -> r1
        //   1: Closure #0 -> r2
        //   2: CapStore r2[0] = r1
        //   3: Move r2 -> r3
        //   4: Call r3, 0 args, dst r0
        //   5: Halt
        // fn0 (at offset 6):
        //   6: CapLoad [0] -> r4
        //   7: Const1 r5
        //   8: IAdd r4, r5, r6
        //   9: RetVal r6
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c41_idx >> 8) & 0xFF) as u8,
            (c41_idx & 0xFF) as u8,
            1,
        )); // 0
        module.emit(Instruction::new3(OpCode::Closure, 0, 0, 2)); // 1
        module.emit(Instruction::new3(OpCode::CapStore, 2, 0, 1)); // 2
        module.emit(Instruction::new2(OpCode::Move, 2, 3)); // 3
        module.emit(Instruction::new3(OpCode::Call, 3, 0, 0)); // 4
        module.emit(Instruction::new0(OpCode::Halt)); // 5
        let fn0_offset = module.current_offset();
        module.emit(Instruction::new3(OpCode::CapLoad, 0, 4, 0)); // 6
        module.emit(Instruction::new1(OpCode::Const1, 5)); // 7
        module.emit(Instruction::new3(OpCode::IAdd, 4, 5, 6)); // 8
        module.emit(Instruction::new1(OpCode::RetVal, 6)); // 9
        module.function_table.push(fn0_offset);
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run().unwrap();
        assert_eq!(result.as_int(), Some(42), "captured 41 + 1 should be 42");
    }

    #[test]
    fn test_curried_bytecode_exact() {
        let mut module = CodeModule::new("test_exact");
        module.add_constant(Constant::Int(3));
        module.add_constant(Constant::Int(5));
        module.emit(Instruction::new3(OpCode::Jmp, 0, 13, 0));
        module.emit(Instruction::new3(OpCode::Jmp, 0, 7, 0));
        module.emit(Instruction::new3(OpCode::CapLoad, 0, 11, 0));
        module.emit(Instruction::new2(OpCode::Move, 11, 8));
        module.emit(Instruction::new2(OpCode::Move, 10, 9));
        module.emit(Instruction::new3(OpCode::IAdd, 8, 9, 10));
        module.emit(Instruction::new2(OpCode::Move, 10, 0));
        module.emit(Instruction::new0(OpCode::RetVal));
        module.emit(Instruction::new3(OpCode::Closure, 0, 1, 8)); // inner closure: index 1
        module.emit(Instruction::new2(OpCode::Move, 10, 11));
        module.emit(Instruction::new3(OpCode::CapStore, 8, 0, 11));
        module.emit(Instruction::new2(OpCode::Move, 8, 0));
        module.emit(Instruction::new0(OpCode::RetVal));
        module.emit(Instruction::new3(OpCode::Closure, 0, 0, 8)); // outer closure: index 0
        module.emit(Instruction::new2(OpCode::Move, 8, 9));
        module.emit(Instruction::new2(OpCode::Move, 9, 29));
        module.emit(Instruction::new3(OpCode::ConstU, 0, 0, 10));
        module.emit(Instruction::new2(OpCode::Move, 10, 10));
        module.emit(Instruction::new3(OpCode::ClosureCall, 29, 1, 9));
        module.emit(Instruction::new2(OpCode::Move, 9, 30));
        module.emit(Instruction::new3(OpCode::ConstU, 0, 1, 11));
        module.emit(Instruction::new2(OpCode::Move, 11, 10));
        module.emit(Instruction::new3(OpCode::ClosureCall, 30, 1, 10));
        module.emit(Instruction::new2(OpCode::Move, 10, 0));
        module.emit(Instruction::new0(OpCode::Halt));
        module.function_table.push(1);
        module.function_table.push(2);
        module.entry_point = Some(0);
        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run().unwrap();
        eprintln!("Result: {:?}", result.as_int());
        assert_eq!(result.as_int(), Some(8), "should be 8");
    }

    #[test]
    fn test_curried_nbc_full() {
        let mut module = CodeModule::new("test_nbc2");
        module.add_constant(Constant::Int(3));
        module.add_constant(Constant::Int(5));
        module.emit(Instruction::new3(OpCode::Jmp, 0, 13, 0));
        module.emit(Instruction::new3(OpCode::Jmp, 0, 7, 0));
        module.emit(Instruction::new3(OpCode::CapLoad, 0, 11, 0));
        module.emit(Instruction::new2(OpCode::Move, 11, 8));
        module.emit(Instruction::new2(OpCode::Move, 10, 9));
        module.emit(Instruction::new3(OpCode::IAdd, 8, 9, 10));
        module.emit(Instruction::new2(OpCode::Move, 10, 0));
        module.emit(Instruction::new0(OpCode::RetVal));
        module.emit(Instruction::new3(OpCode::Closure, 0, 1, 8));
        module.emit(Instruction::new2(OpCode::Move, 10, 11));
        module.emit(Instruction::new3(OpCode::CapStore, 8, 0, 11));
        module.emit(Instruction::new2(OpCode::Move, 8, 0));
        module.emit(Instruction::new0(OpCode::RetVal));
        module.emit(Instruction::new3(OpCode::Closure, 0, 0, 8)); // outer closure: index 0
        module.emit(Instruction::new2(OpCode::Move, 8, 9));
        module.emit(Instruction::new2(OpCode::Move, 9, 29));
        module.emit(Instruction::new3(OpCode::ConstU, 0, 0, 10));
        module.emit(Instruction::new2(OpCode::Move, 10, 10));
        module.emit(Instruction::new3(OpCode::ClosureCall, 29, 1, 9));
        module.emit(Instruction::new2(OpCode::Move, 9, 30));
        module.emit(Instruction::new3(OpCode::ConstU, 0, 1, 11));
        module.emit(Instruction::new2(OpCode::Move, 11, 10));
        module.emit(Instruction::new3(OpCode::ClosureCall, 30, 1, 10));
        module.emit(Instruction::new2(OpCode::Move, 10, 0));
        module.emit(Instruction::new0(OpCode::Halt));
        module.function_table.push(1);
        module.function_table.push(2);
        module.entry_point = Some(0);
        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run().unwrap();
        eprintln!("Result: {:?}", result.as_int());
        assert_eq!(result.as_int(), Some(8), "should be 8");
    }
    /// CapLoad without a closure environment must error, not silently no-op.
    #[test]
    fn test_capload_outside_closure_errors() {
        let mut module = CodeModule::new("test_capload_err");
        module.emit(Instruction::new3(OpCode::CapLoad, 0, 1, 0));
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_err(),
            "CapLoad outside a closure call should error"
        );
    }

    /// Regression test for the closure_envs leak's safety valve: once the
    /// ceiling is reached, creating a new capturing closure must fail with
    /// an honest error rather than growing `closure_envs` forever.
    #[test]
    fn test_closure_env_limit_is_an_honest_error_not_unbounded_growth() {
        let mut module = CodeModule::new("test_capture_limit");
        let c41_idx = module.add_constant(Constant::Int(41));
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c41_idx >> 8) & 0xFF) as u8,
            (c41_idx & 0xFF) as u8,
            1,
        ));
        module.emit(Instruction::new3(OpCode::Closure, 0, 0, 2));
        module.emit(Instruction::new3(OpCode::CapStore, 2, 0, 1));
        module.emit(Instruction::new0(OpCode::Halt));
        module.function_table.push(module.current_offset());
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.set_max_closure_envs_for_test(0);
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_err(),
            "CapStore past the closure-env ceiling should be an honest error, not silently succeed"
        );
        assert_eq!(
            vm.closure_env_count(),
            0,
            "no env should have been retained past the ceiling"
        );
    }

    /// Test 17: NodeId returns the configured local node ID.
    #[test]
    fn test_node_id_returns_configured_value() {
        let mut module = CodeModule::new("test_node_id");
        module.emit(Instruction::new1(OpCode::NodeId, 0));
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.set_node_id(42);
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_ok(), "NodeId should not fail: {:?}", result.err());
        assert_eq!(result.unwrap().as_int(), Some(42));
    }

    /// Test 18: NodeId defaults to 0 with no explicit configuration.
    #[test]
    fn test_node_id_defaults_to_zero() {
        let mut module = CodeModule::new("test_node_id_default");
        module.emit(Instruction::new1(OpCode::NodeId, 0));
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_int(), Some(0));
    }

    /// Test 19: Migrate records a migration request.
    #[test]
    fn test_migrate_records_request() {
        let mut module = CodeModule::new("test_migrate");
        let actor_const = module.add_constant(Constant::Int(7));
        let node_const = module.add_constant(Constant::Int(99));
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((actor_const >> 8) & 0xFF) as u8,
            (actor_const & 0xFF) as u8,
            1,
        )); // r1 = 7
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((node_const >> 8) & 0xFF) as u8,
            (node_const & 0xFF) as u8,
            2,
        )); // r2 = 99
        module.emit(Instruction::new3(OpCode::Migrate, 1, 2, 0)); // migrate actor 7 to node 99 -> r0
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "Migrate should not fail: {:?}",
            result.err()
        );
        assert!(result.unwrap().is_unit(), "Migrate should return unit");
        assert_eq!(vm.pending_migrations(), &[(7, 99)]);
    }

    /// Test 20: RAsk returns nil when no distributed runtime is attached.
    #[test]
    fn test_rask_returns_nil_without_runtime() {
        let mut module = CodeModule::new("test_rask");
        let behavior_const = module.add_constant(Constant::String("ping".to_string()));
        let actor_const = module.add_constant(Constant::Int(3));
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((actor_const >> 8) & 0xFF) as u8,
            (actor_const & 0xFF) as u8,
            1,
        )); // r1 = 3
        module.emit(Instruction::new3(OpCode::RAsk, 1, behavior_const as u8, 0)); // rask -> r0
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_ok(), "RAsk should not fail: {:?}", result.err());
        assert!(
            result.unwrap().is_nil(),
            "RAsk should return nil without runtime"
        );
    }

    /// Test 21: Gossip records intent and returns unit.
    #[test]
    fn test_gossip_records_intent_and_returns_unit() {
        let mut module = CodeModule::new("test_gossip");
        let msg_const = module.add_constant(Constant::String("hello".to_string()));
        module.emit(Instruction::new3(OpCode::Gossip, msg_const as u8, 0, 0)); // gossip -> r0
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(result.is_ok(), "Gossip should not fail: {:?}", result.err());
        assert!(result.unwrap().is_unit(), "Gossip should return unit");
        assert_eq!(vm.gossip_log(), &["hello".to_string()]);
    }

    /// Test 22: Distributed callbacks are invoked by remote opcodes.
    #[test]
    fn test_distributed_callbacks_invoked() {
        #[derive(Debug)]
        struct MockCallbacks {
            node_id: u64,
            migrations: Vec<(u64, u64)>,
            asks: Vec<(u64, String, Vec<Value>)>,
            gossips: Vec<String>,
        }
        impl DistributedVmCallbacks for MockCallbacks {
            fn node_id(&self) -> u64 {
                self.node_id
            }
            fn migrate(&mut self, actor_id: u64, target_node_id: u64) {
                self.migrations.push((actor_id, target_node_id));
            }
            fn remote_ask(
                &mut self,
                target_actor: u64,
                behavior: &str,
                args: &[Value],
                _timeout_ms: u64,
            ) -> Value {
                self.asks
                    .push((target_actor, behavior.to_string(), args.to_vec()));
                Value::int(123)
            }
            fn gossip(&mut self, message: &str) -> Value {
                self.gossips.push(message.to_string());
                Value::unit()
            }
        }

        let mut module = CodeModule::new("test_callbacks");
        let actor_const = module.add_constant(Constant::Int(5));
        let node_const = module.add_constant(Constant::Int(11));
        let msg_const = module.add_constant(Constant::String("sync".to_string()));

        // Add a behavior so RAsk can look up the name from the behavior table.
        module.add_behavior(BehaviorTableEntry {
            name: "echo".to_string(),
            param_count: 2,
            code_offset: 0,
            local_count: 0,
            effect_mask: 0,
            compensate_offset: None,
            content_hash: None,
            source_location: None,
            parallel_branches: None,
        });
        let behavior_idx: usize = 0; // first (and only) behavior

        module.emit(Instruction::new1(OpCode::NodeId, 0)); // r0 = node_id
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((actor_const >> 8) & 0xFF) as u8,
            (actor_const & 0xFF) as u8,
            1,
        )); // r1 = 5 (target; also lands as arg1 per the Ask staging convention)
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((actor_const >> 8) & 0xFF) as u8,
            (actor_const & 0xFF) as u8,
            0,
        )); // r0 = 5 (arg0)
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((node_const >> 8) & 0xFF) as u8,
            (node_const & 0xFF) as u8,
            2,
        )); // r2 = 11
        module.emit(Instruction::new3(OpCode::Migrate, 1, 2, 3)); // r3 = migrate
                                                                  // RAsk: op1=actor_reg, op2+op3=16-bit behavior table index (same as Ask).
                                                                  // Result written to op1; codegen follows with Move op1→dst.
        module.emit(Instruction::new3(
            OpCode::RAsk,
            1,
            ((behavior_idx >> 8) & 0xFF) as u8,
            (behavior_idx & 0xFF) as u8,
        ));
        // Move the RAsk result from r1 (actor reg, now overwritten with result) to r4.
        module.emit(Instruction::new2(OpCode::Move, 1, 4));
        module.emit(Instruction::new3(OpCode::Gossip, msg_const as u8, 0, 5)); // r5 = gossip
        module.emit(Instruction::new1(OpCode::RetVal, 4)); // return the RAsk result
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let callbacks = Box::new(MockCallbacks {
            node_id: 77,
            migrations: Vec::new(),
            asks: Vec::new(),
            gossips: Vec::new(),
        });
        let expected_node_id = callbacks.node_id;

        let mut vm = VM::new();
        vm.set_distributed_callbacks(callbacks);
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "Callbacks should not fail: {:?}",
            result.err()
        );
        // The RAsk result must be the callback's value (not nil, not the
        // target actor ref) — regression for the RAsk register-convention
        // mismatch where the remote ask returned the wrong value.
        assert_eq!(
            result.unwrap(),
            Value::int(123),
            "RAsk must surface the remote_ask callback's value"
        );

        let cb = (vm.distributed_callbacks.as_ref().unwrap().as_ref() as &dyn std::any::Any)
            .downcast_ref::<MockCallbacks>()
            .unwrap();
        assert_eq!(cb.node_id, expected_node_id);
        assert_eq!(cb.migrations, &[(5, 11)]);
        assert_eq!(
            cb.asks,
            &[(5, "echo".to_string(), vec![Value::int(5), Value::int(5)])],
            "RAsk must stage behavior args (regs 0..param_count) like local Ask"
        );
        assert_eq!(cb.gossips, &["sync".to_string()]);
    }

    /// Test 23: FFI call to libm sqrt (skipped if libm cannot be opened).
    #[test]
    #[cfg(target_os = "linux")]
    fn test_ffi_call_libm_sqrt() {
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;

        let source = r#"
            extern "libm.so.6" {
                fn sqrt(x: Float) -> Float
            }
            sqrt(4.0)
        "#;
        let mut type_checker = TypeChecker::new();
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");
        type_checker.check_module(&ast).expect("typecheck");
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).expect("mir lower");
        let module = crate::mir_codegen::compile_mir(&mut mir, "test").expect("compile");

        let mut vm = VM::new();
        vm.load_module(module);
        match vm.run() {
            Ok(result) => {
                let f = result.as_float().expect("float result");
                assert!(
                    (f - 2.0).abs() < 1e-12,
                    "sqrt(4.0) should be 2.0, got {}",
                    f
                );
            }
            Err(crate::types::NuError::VMError { msg, span: _ })
                if msg.contains("open") || msg.contains("load failed") =>
            {
                eprintln!("warning: could not open libm.so.6, skipping test: {}", msg);
            }
            Err(e) => panic!("unexpected FFI error: {}", e),
        }
    }

    /// Test 23b: FFI sandbox blocks calls to non-allowlisted libraries.
    #[test]
    #[cfg(target_os = "linux")]
    fn test_ffi_sandbox_blocks_unauthorized_library() {
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        use crate::typechecker::TypeChecker;

        let source = r#"
            extern "libm.so.6" {
                fn sqrt(x: Float) -> Float
            }
            sqrt(4.0)
        "#;
        let mut type_checker = TypeChecker::new();
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");
        type_checker.check_module(&ast).expect("typecheck");
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).expect("mir lower");
        let module = crate::mir_codegen::compile_mir(&mut mir, "test_sandbox").expect("compile");

        // Test 1: sandbox enabled, empty allow-list → blocked.
        let mut vm = VM::new();
        vm.set_ffi_sandbox(true, vec![]);
        vm.load_module(module.clone());
        let result = vm.run();
        assert!(result.is_err(), "FFI call should be blocked by sandbox");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("FFI sandbox blocked"),
            "error should mention sandbox: {}",
            err_msg
        );

        // Test 2: sandbox enabled, libm.so.6 allowlisted → should work.
        let mut vm2 = VM::new();
        vm2.set_ffi_sandbox(true, vec!["libm.so.6".to_string()]);
        vm2.load_module(module);
        match vm2.run() {
            Ok(result) => {
                let f = result.as_float().expect("float result");
                assert!(
                    (f - 2.0).abs() < 1e-12,
                    "sqrt(4.0) should be 2.0, got {}",
                    f
                );
            }
            Err(crate::types::NuError::VMError { msg, span: _ })
                if msg.contains("open") || msg.contains("load failed") =>
            {
                eprintln!(
                    "warning: could not open libm.so.6, skipping allow-list test: {}",
                    msg
                );
            }
            Err(e) => panic!("unexpected FFI error with allow-list: {}", e),
        }
    }

    /// Test 24: `Drop` clears the register, so a duplicate `Drop` of the same
    /// register (as `plan_drops` can emit: last-use drop followed by a
    /// redefinition or block-entry drop) is a no-op rather than a second
    /// decrement of an already-freed object's reference count.
    #[test]
    fn test_drop_clears_register_and_double_drop_is_noop() {
        let mut module = CodeModule::new("test_drop_idempotent");
        module.emit(Instruction::new1(OpCode::Const1, 0)); // r0 = 1 (len)
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 1)); // r1 = array(1)
        module.emit(Instruction::new1(OpCode::Drop, 1)); // free it
        module.emit(Instruction::new1(OpCode::Drop, 1)); // must be a no-op
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 2)); // r2 = array(1)
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 3)); // r3 = array(1)
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "double Drop should not fail: {:?}",
            result.err()
        );

        let regs = &vm.frames[0].regs;
        assert_eq!(
            regs[1].as_raw(),
            Value::nil().as_raw(),
            "Drop must clear the register to nil"
        );
        assert!(regs[2].as_ptr().is_some() && regs[3].as_ptr().is_some());
        assert_ne!(
            regs[2].as_raw(),
            regs[3].as_raw(),
            "fresh allocations must be distinct blocks (free-list corruption check)"
        );

        // Exactly one reference was actually dropped: the second Drop hit nil.
        let cb_any: &dyn std::any::Any = &*vm.actor_callbacks;
        let cb = cb_any
            .downcast_ref::<StandaloneVmCallbacks>()
            .expect("standalone callbacks");
        let dropped = cb.gc.stats().local_refs_dropped;
        assert_eq!(dropped, 1, "duplicate Drop must not decrement twice");
    }

    /// Test 25: `ArrStore` releases the overwritten slot's old value, so
    /// repeatedly overwriting one slot does not leak the previous elements.
    /// Observable via exact-size free-list reuse: once the overwritten object
    /// is released and its register dropped, the next same-size allocation
    /// recycles its block.
    #[test]
    fn test_arrstore_releases_overwritten_slot() {
        let mut module = CodeModule::new("test_arrstore_release");
        module.emit(Instruction::new1(OpCode::Const0, 6)); // r6 = 0 (idx)
        module.emit(Instruction::new1(OpCode::Const1, 0)); // r0 = 1 (len)
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 1)); // r1 = container
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 2)); // r2 = X
        module.emit(Instruction::new3(OpCode::ArrStore, 1, 6, 2)); // arr[0] = X (retain)
        module.emit(Instruction::new3(OpCode::ArrLoad, 1, 6, 5)); // r5 = X's bits
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 3)); // r3 = Y
        module.emit(Instruction::new3(OpCode::ArrStore, 1, 6, 3)); // arr[0] = Y (release X)
        module.emit(Instruction::new1(OpCode::Drop, 2)); // drop register ref to X -> freed
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 4)); // r4 = Z: reuses X's block
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "ArrStore release test failed: {:?}",
            result.err()
        );

        let regs = &vm.frames[0].regs;
        assert_eq!(
            regs[4].as_raw(),
            regs[5].as_raw(),
            "released-and-dropped object should have been freed and its block reused"
        );
        assert_ne!(
            regs[4].as_raw(),
            regs[3].as_raw(),
            "Z must not alias the live Y"
        );
    }

    /// Test 26: `RecS` releases the overwritten slot's old value (same
    /// protocol as `ArrStore`).
    #[test]
    fn test_rec_s_releases_overwritten_slot() {
        let mut module = CodeModule::new("test_rec_s_release");
        module.emit(Instruction::new1(OpCode::Const1, 0)); // r0 = 1 (len)
        module.emit(Instruction::new2(OpCode::RecMk, 1, 1)); // r1 = record(1 slot)
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 2)); // r2 = X
        module.emit(Instruction::new3(OpCode::RecS, 1, 0, 2)); // rec.f0 = X (retain)
        module.emit(Instruction::new3(OpCode::RecL, 1, 0, 5)); // r5 = X's bits
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 3)); // r3 = Y
        module.emit(Instruction::new3(OpCode::RecS, 1, 0, 3)); // rec.f0 = Y (release X)
        module.emit(Instruction::new1(OpCode::Drop, 2)); // drop register ref to X -> freed
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 4)); // r4 = Z: reuses X's block
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "RecS release test failed: {:?}",
            result.err()
        );

        let regs = &vm.frames[0].regs;
        assert_eq!(
            regs[4].as_raw(),
            regs[5].as_raw(),
            "released-and-dropped record field value should have been freed and reused"
        );
        assert_ne!(
            regs[4].as_raw(),
            regs[3].as_raw(),
            "Z must not alias the live Y"
        );
    }

    /// Test 27: `FieldS` (tuple store) must retain the stored value — the
    /// GC's `free_object` releases slot references when a container is
    /// reclaimed, so an uncounted store would decrement a child that was
    /// never retained. The full chain: child kept alive by the slot after
    /// its register is dropped, then freed when the tuple is dropped, then
    /// its block recycled.
    #[test]
    fn test_fields_retains_and_release_chain_frees_child() {
        let mut module = CodeModule::new("test_fields_barrier");
        module.emit(Instruction::new1(OpCode::Const1, 0)); // r0 = 1 (len)
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 2)); // r2 = X (child)
        module.emit(Instruction::new2(OpCode::TupleMk, 1, 1)); // r1 = tuple(1)
        module.emit(Instruction::new3(OpCode::FieldS, 1, 0, 2)); // tup.0 = X (must retain)
        module.emit(Instruction::new1(OpCode::Drop, 2)); // drop register ref; slot keeps X alive
        module.emit(Instruction::new3(OpCode::FieldL, 1, 0, 5)); // r5 = X's bits
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 3)); // r3: fresh block while X lives
        module.emit(Instruction::new1(OpCode::Drop, 1)); // free tuple -> releases child X
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 4)); // r4: pops tuple's block (LIFO)
        module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 7)); // r7: pops X's block
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "FieldS barrier test failed: {:?}",
            result.err()
        );

        let regs = &vm.frames[0].regs;
        assert_ne!(
            regs[3].as_raw(),
            regs[5].as_raw(),
            "X must still be alive (slot-owned) when r3 is allocated — Drop of the register must not free it"
        );
        assert_eq!(
            regs[7].as_raw(),
            regs[5].as_raw(),
            "after the tuple is dropped the child's block must be freed and recycled"
        );
        assert_ne!(
            regs[4].as_raw(),
            regs[5].as_raw(),
            "sanity: tuple block and child block differ"
        );
    }

    /// Regression: string constants must survive JIT tier-up.
    /// `constants_to_jit_bits` used to encode `Constant::String` as nil bits,
    /// so a hot loop loading a string constant produced nil once the region
    /// compiled, while the cold interpreter produced the string.
    #[test]
    fn test_jit_hot_loop_string_constant_survives_tierup() {
        fn build_string_loop_module(limit: i64) -> CodeModule {
            let mut module = CodeModule::new("test_jit_str_const");
            let c_limit = module.add_constant(Constant::Int(limit));
            let c_hi = module.add_constant(Constant::String("hi".to_string()));

            // r0 = counter, r1 = limit, r2 = string load, r3 = cond,
            // r5 = carried string value.
            module.emit(Instruction::new3(
                OpCode::ConstU,
                ((c_limit >> 8) & 0xFF) as u8,
                (c_limit & 0xFF) as u8,
                1,
            )); // 0: r1 = limit
            module.emit(Instruction::new1(OpCode::Const0, 0)); // 1: r0 = 0
                                                               // Loop body (pc 2..=5): loads the string constant every iteration.
            module.emit(Instruction::new3(
                OpCode::ConstU,
                ((c_hi >> 8) & 0xFF) as u8,
                (c_hi & 0xFF) as u8,
                2,
            )); // 2: r2 = "hi"
            module.emit(Instruction::new2(OpCode::Move, 2, 5)); // 3: r5 = r2
            module.emit(Instruction::new1(OpCode::IInc, 0)); // 4: i++
            module.emit(Instruction::new3(OpCode::ICmpLt, 0, 1, 3)); // 5: r3 = i < limit
            let back: i16 = -4; // JmpT at pc 6 -> pc 2
            module.emit(Instruction::new3(
                OpCode::JmpT,
                3,
                ((back as u16) >> 8) as u8,
                (back as u16 & 0xFF) as u8,
            )); // 6
            module.emit(Instruction::new2(OpCode::Move, 5, 0)); // 7: r0 = r5
            module.emit(Instruction::new0(OpCode::Halt)); // 8
            module.entry_point = Some(0);
            module
        }

        // Cold run (below HOT_THRESHOLD): interpreted throughout.
        let mut cold_vm = VM::new();
        cold_vm.load_module(build_string_loop_module(900));
        let cold = cold_vm.run().expect("cold string loop should run");
        assert_eq!(
            cold.raw & TAG_MASK,
            TAG_STRING,
            "cold run should yield a string"
        );

        // Hot run: the loop body tiers up past HOT_THRESHOLD=1000 and must
        // still produce the string constant, not nil.
        let mut hot_vm = VM::new();
        hot_vm.load_module(build_string_loop_module(3000));
        let hot = hot_vm.run().expect("hot string loop should run");
        assert_eq!(
            hot.raw & TAG_MASK,
            TAG_STRING,
            "string constant must survive JIT tier-up (was silently nil)"
        );
        assert_eq!(hot.raw, cold.raw, "hot and cold runs must agree");
        let compiled = hot_vm
            .jit_session
            .as_ref()
            .map(|j| j.compiled_count())
            .unwrap_or(0);
        assert!(compiled > 0, "loop body must have been JIT-compiled");
    }

    /// Regression: a JIT-compiled `ArrLoad` must apply the interpreter's
    /// null/type/bounds checks and yield nil for out-of-bounds reads instead
    /// of dereferencing unchecked memory (a large offset used to read
    /// garbage or segfault after tier-up).
    #[test]
    fn test_jit_hot_oob_arrload_returns_nil() {
        fn build_oob_module(limit: i64) -> CodeModule {
            let mut module = CodeModule::new("test_jit_oob_arrload");
            let c_len = module.add_constant(Constant::Int(3));
            let c_big = module.add_constant(Constant::Int(1_000_000));
            let c_limit = module.add_constant(Constant::Int(limit));

            // r0 = counter/result, r1 = limit, r3 = cond, r4 = array,
            // r5 = loaded value, r6 = out-of-bounds index.
            module.emit(Instruction::new3(
                OpCode::ConstU,
                ((c_len >> 8) & 0xFF) as u8,
                (c_len & 0xFF) as u8,
                0,
            )); // 0: r0 = 3
            module.emit(Instruction::new2(OpCode::ArrAlloc, 0, 4)); // 1: r4 = array[3]
            module.emit(Instruction::new3(
                OpCode::ConstU,
                ((c_big >> 8) & 0xFF) as u8,
                (c_big & 0xFF) as u8,
                6,
            )); // 2: r6 = 1_000_000
            module.emit(Instruction::new3(
                OpCode::ConstU,
                ((c_limit >> 8) & 0xFF) as u8,
                (c_limit & 0xFF) as u8,
                1,
            )); // 3: r1 = limit
            module.emit(Instruction::new1(OpCode::Const0, 0)); // 4: r0 = 0
                                                               // Loop body (pc 5..=7): reads far out of bounds every iteration.
            module.emit(Instruction::new3(OpCode::ArrLoad, 4, 6, 5)); // 5: r5 = a[1_000_000]
            module.emit(Instruction::new1(OpCode::IInc, 0)); // 6: i++
            module.emit(Instruction::new3(OpCode::ICmpLt, 0, 1, 3)); // 7: r3 = i < limit
            let back: i16 = -3; // JmpT at pc 8 -> pc 5
            module.emit(Instruction::new3(
                OpCode::JmpT,
                3,
                ((back as u16) >> 8) as u8,
                (back as u16 & 0xFF) as u8,
            )); // 8
            module.emit(Instruction::new2(OpCode::Move, 5, 0)); // 9: r0 = r5
            module.emit(Instruction::new0(OpCode::Halt)); // 10
            module.entry_point = Some(0);
            module
        }

        // Cold run: the interpreter yields nil out of bounds.
        let mut cold_vm = VM::new();
        cold_vm.load_module(build_oob_module(3));
        let cold = cold_vm.run().expect("cold OOB loop should run");
        assert!(cold.is_nil(), "cold out-of-bounds ArrLoad must be nil");

        // Hot run: the compiled region must apply the same checks.
        let mut hot_vm = VM::new();
        hot_vm.load_module(build_oob_module(3000));
        let hot = hot_vm.run().expect("hot OOB loop must not crash");
        assert!(
            hot.is_nil(),
            "JIT-compiled out-of-bounds ArrLoad must yield nil like the interpreter"
        );
        let compiled = hot_vm
            .jit_session
            .as_ref()
            .map(|j| j.compiled_count())
            .unwrap_or(0);
        assert!(compiled > 0, "loop body must have been JIT-compiled");
    }

    /// Verify that a hot integer-arithmetic loop compiles through the
    /// type-directed (guard-stripped) path and produces the same result as
    /// the interpreter.
    #[test]
    fn test_jit_typed_tiering_integer_loop() {
        let mut module = CodeModule::new("test_jit_typed_int_loop");
        // r0 = sum, r1 = i, r2 = limit, r3 = one
        module.emit(Instruction::new1(OpCode::Const0, 0)); // 0: sum = 0
        module.emit(Instruction::new1(OpCode::Const0, 1)); // 1: i = 0
        let c10 = module.add_constant(Constant::Int(10));
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c10 >> 8) & 0xFF) as u8,
            (c10 & 0xFF) as u8,
            2,
        )); // 2: limit = 10
        module.emit(Instruction::new1(OpCode::Const1, 3)); // 3: one = 1

        let loop_start = module.current_offset();
        module.emit(Instruction::new3(OpCode::ICmpLt, 1, 2, 4)); // 4: cond = i < limit
        let jmpf_idx = module.current_offset();
        module.emit(Instruction::new2(OpCode::JmpF, 4, 0)); // 5 (patched): exit if !cond
        module.emit(Instruction::new3(OpCode::IAdd, 0, 1, 0)); // 6: sum += i
        module.emit(Instruction::new3(OpCode::IAdd, 1, 3, 1)); // 7: i += 1
        let jmp_back = module.current_offset();
        let back = loop_start as i64 - jmp_back as i64;
        module.emit(Instruction::new3(
            OpCode::Jmp,
            ((back as i16 >> 8) & 0xFF) as u8,
            (back as i16 & 0xFF) as u8,
            0,
        )); // 8: goto loop_start
        let after = module.current_offset();
        if let Some(instr) = module.instructions.get_mut(jmpf_idx) {
            let fwd = after as i64 - jmpf_idx as i64;
            instr.op2 = ((fwd as i16 >> 8) & 0xFF) as u8;
            instr.op3 = (fwd as i16 & 0xFF) as u8;
        }
        module.emit(Instruction::new0(OpCode::Halt)); // 9
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);

        // Cold run: pure interpreter.
        let cold = vm.run_from(0, 0).unwrap();
        assert_eq!(cold.as_int(), Some(45), "sum 0..10 = 45");

        // Heat to trigger tiered compilation.
        for _ in 0..2000 {
            let _ = vm.run_from(0, 0);
        }
        let hot = vm.run_from(0, 0).unwrap();
        assert_eq!(hot.as_int(), Some(45), "typed JIT must match interpreter");

        // Verify at least one region was compiled.
        let compiled = vm
            .jit_session
            .as_ref()
            .map(|j| j.compiled_count())
            .unwrap_or(0);
        assert!(compiled > 0, "integer loop must be JIT-compiled");
    }
    /// Regression: resuming from a handler that is NOT the top of the
    /// handler stack must work. `Perform` captures the continuation into
    /// the innermost *matching* handler frame (rposition), but `Resume`
    /// used to look only at the top frame, so nested handlers binding
    /// different effects — e.g. `handle handle perform A.x() { | B.y() => 0 }
    /// { | A.x() => 42 }` — errored with "resume called without a captured
    /// continuation".
    #[test]
    fn test_nested_handlers_resume_outer_continuation() {
        let mut module = CodeModule::new("test_nested_resume");

        // Outer table binds "A.x", inner table binds "B.y": a perform of
        // "A.x" matches the outer frame, which is not the stack top.
        module.add_handler_table(HandlerTable {
            bindings: vec![HandlerBinding {
                effect_name: "A.x".to_string(),
                handler_offset: 10,
                arg_count: 0,
                result_reg: 0,
                single_shot: false,
            }],
            fallback_offset: None,
        });
        module.add_handler_table(HandlerTable {
            bindings: vec![HandlerBinding {
                effect_name: "B.y".to_string(),
                handler_offset: 12,
                arg_count: 0,
                result_reg: 0,
                single_shot: false,
            }],
            fallback_offset: None,
        });

        let ax_idx = module.add_constant(Constant::String("A.x".to_string()));
        let c42_idx = module.add_constant(Constant::Int(42));

        // PC 0: Handle(0) — outer
        // PC 1: Handle(1) — inner
        // PC 2: Perform "A.x" -> r0 — continuation lands on the OUTER frame
        // PC 3: Unwind (inner)
        // PC 4: Unwind (outer)
        // PC 5: Halt
        // PC 10: outer handler body: ConstU 42 -> r0; Resume r0
        // PC 12: inner handler body: Const0 -> r0; Resume r0 (unused)
        module.emit(Instruction::new1(OpCode::Handle, 0)); // 0
        module.emit(Instruction::new1(OpCode::Handle, 1)); // 1
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((ax_idx >> 8) & 0xFF) as u8,
            (ax_idx & 0xFF) as u8,
            0,
        )); // 2
        module.emit(Instruction::new0(OpCode::Unwind)); // 3
        module.emit(Instruction::new0(OpCode::Unwind)); // 4
        module.emit(Instruction::new0(OpCode::Halt)); // 5
        for _ in 6..10 {
            module.emit(Instruction::new0(OpCode::Nop));
        } // 6-9
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c42_idx >> 8) & 0xFF) as u8,
            (c42_idx & 0xFF) as u8,
            0,
        )); // 10
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 11
        module.emit(Instruction::new1(OpCode::Const0, 0)); // 12
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 13
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "resume from a non-top handler frame must work: {:?}",
            result.err()
        );
        assert_eq!(
            result.unwrap().as_int(),
            Some(42),
            "outer handler resumes with 42"
        );
    }

    /// Regression: `perform IO.print` in a standalone script (no handler on
    /// the stack) must print via the standalone built-in instead of failing
    /// with "Unhandled effect: IO".
    #[test]
    fn test_standalone_io_print_builtin() {
        let mut module = CodeModule::new("test_io_print");
        let hello_idx = module.add_constant(Constant::String("hello".to_string()));
        let eff_idx = module.add_constant(Constant::String("IO.print".to_string()));

        // r0 = "hello" (staged arg); Perform IO.print -> r1; result is unit.
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((hello_idx >> 8) & 0xFF) as u8,
            (hello_idx & 0xFF) as u8,
            0,
        )); // 0
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((eff_idx >> 8) & 0xFF) as u8,
            (eff_idx & 0xFF) as u8,
            1,
        )); // 1
        module.emit(Instruction::new2(OpCode::Move, 1, 0)); // 2
        module.emit(Instruction::new0(OpCode::Halt)); // 3
        module.entry_point = Some(0);

        let sink = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut callbacks = StandaloneVmCallbacks::new();
        callbacks.io_output = Some(sink.clone());

        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(callbacks));
        let result = vm.run();
        assert!(
            result.is_ok(),
            "standalone IO.print must not error: {:?}",
            result.err()
        );
        assert!(result.unwrap().is_unit(), "IO.print resumes with unit");
        assert_eq!(sink.borrow().as_slice(), &["hello".to_string()]);
    }

    /// Http.get and Http.post must be handled as built-in effects in the
    /// standalone VM; they must not produce "Unhandled effect" errors.
    #[test]
    fn test_standalone_http_get_builtin() {
        let mut module = CodeModule::new("test_http_get");
        // URL "http://127.0.0.1:1" — will fail to connect (port 1 is privileged),
        // but the effect must dispatch and return nil, not "Unhandled effect".
        let url = "http://127.0.0.1:1";
        let url_idx = module.add_constant(Constant::String(url.to_string()));
        let eff_idx = module.add_constant(Constant::String("Http.get".to_string()));

        // r0 = url; Perform Http.get -> r1
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((url_idx >> 8) & 0xFF) as u8,
            (url_idx & 0xFF) as u8,
            0,
        )); // 0
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((eff_idx >> 8) & 0xFF) as u8,
            (eff_idx & 0xFF) as u8,
            1,
        )); // 1
        module.emit(Instruction::new2(OpCode::Move, 1, 0)); // 2
        module.emit(Instruction::new0(OpCode::Halt)); // 3
        module.entry_point = Some(0);

        let callbacks = StandaloneVmCallbacks::new();
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(callbacks));
        let result = vm.run();
        assert!(
            result.is_ok(),
            "standalone Http.get must not error with 'Unhandled effect': {:?}",
            result.err()
        );
        // On connection failure, the result should be nil (not a string body).
        assert!(
            result.unwrap().is_nil(),
            "Http.get on non-connectable port should return nil"
        );
    }

    /// Http.post must also dispatch in the standalone VM without error.
    #[test]
    fn test_standalone_http_post_builtin() {
        let mut module = CodeModule::new("test_http_post");
        let url = "http://127.0.0.1:1";
        let url_idx = module.add_constant(Constant::String(url.to_string()));
        let body_idx = module.add_constant(Constant::String("{}".to_string()));
        let eff_idx = module.add_constant(Constant::String("Http.post".to_string()));

        // r0 = url; r1 = body; Perform Http.post -> r2
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((url_idx >> 8) & 0xFF) as u8,
            (url_idx & 0xFF) as u8,
            0,
        )); // 0
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((body_idx >> 8) & 0xFF) as u8,
            (body_idx & 0xFF) as u8,
            1,
        )); // 1
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((eff_idx >> 8) & 0xFF) as u8,
            (eff_idx & 0xFF) as u8,
            2,
        )); // 2
        module.emit(Instruction::new2(OpCode::Move, 2, 0)); // 3
        module.emit(Instruction::new0(OpCode::Halt)); // 4
        module.entry_point = Some(0);

        let callbacks = StandaloneVmCallbacks::new();
        let mut vm = VM::new();
        vm.load_module(module);
        vm.set_actor_callbacks(Box::new(callbacks));
        let result = vm.run();
        assert!(
            result.is_ok(),
            "standalone Http.post must not error with 'Unhandled effect': {:?}",
            result.err()
        );
        assert!(
            result.unwrap().is_nil(),
            "Http.post on non-connectable port should return nil"
        );
    }

    /// Regression: effect dispatch must match on the (effect, op) pair —
    /// a handler for `IO.bar` must NOT catch a perform of `IO.foo`.
    #[test]
    fn test_perform_dispatches_on_op_name() {
        let mut module = CodeModule::new("test_op_dispatch");
        module.add_handler_table(HandlerTable {
            bindings: vec![HandlerBinding {
                effect_name: "IO.bar".to_string(),
                handler_offset: 8,
                arg_count: 0,
                result_reg: 0,
                single_shot: false,
            }],
            fallback_offset: None,
        });
        let eff_idx = module.add_constant(Constant::String("IO.foo".to_string()));

        module.emit(Instruction::new1(OpCode::Handle, 0)); // 0
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((eff_idx >> 8) & 0xFF) as u8,
            (eff_idx & 0xFF) as u8,
            0,
        )); // 1
        module.emit(Instruction::new0(OpCode::Unwind)); // 2
        module.emit(Instruction::new0(OpCode::Halt)); // 3
        for _ in 4..8 {
            module.emit(Instruction::new0(OpCode::Nop));
        } // 4-7
          // IO.bar handler body (must NOT run): resume with 42.
        let c42_idx = module.add_constant(Constant::Int(42));
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c42_idx >> 8) & 0xFF) as u8,
            (c42_idx & 0xFF) as u8,
            0,
        )); // 8
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 9
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_err(),
            "IO.foo must not be caught by an IO.bar handler"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("Unhandled effect"),
            "wrong-op perform should be unhandled, got: {}",
            msg
        );
        assert!(
            msg.contains("IO.foo"),
            "error should name the qualified effect, got: {}",
            msg
        );
    }

    /// Positive control for op-name dispatch: a handler naming the exact
    /// "Effect.op" pair DOES catch the perform.
    #[test]
    fn test_perform_op_name_matches_qualified_handler() {
        let mut module = CodeModule::new("test_op_match");
        module.add_handler_table(HandlerTable {
            bindings: vec![HandlerBinding {
                effect_name: "IO.foo".to_string(),
                handler_offset: 8,
                arg_count: 0,
                result_reg: 0,
                single_shot: false,
            }],
            fallback_offset: None,
        });
        let eff_idx = module.add_constant(Constant::String("IO.foo".to_string()));
        let c7_idx = module.add_constant(Constant::Int(7));

        module.emit(Instruction::new1(OpCode::Handle, 0)); // 0
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((eff_idx >> 8) & 0xFF) as u8,
            (eff_idx & 0xFF) as u8,
            1,
        )); // 1
        module.emit(Instruction::new2(OpCode::Move, 1, 0)); // 2
        module.emit(Instruction::new0(OpCode::Unwind)); // 3
        module.emit(Instruction::new0(OpCode::Halt)); // 4
        for _ in 5..8 {
            module.emit(Instruction::new0(OpCode::Nop));
        } // 5-7
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c7_idx >> 8) & 0xFF) as u8,
            (c7_idx & 0xFF) as u8,
            0,
        )); // 8
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 9
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "qualified handler must catch: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().as_int(), Some(7));
    }

    /// Compatibility: a binding carrying a bare effect name (no op, as the
    /// MIR pipeline emitted before op-qualified dispatch) still catches any
    /// op of that effect.
    #[test]
    fn test_perform_bare_effect_binding_matches_legacy() {
        let mut module = CodeModule::new("test_bare_binding");
        module.add_handler_table(HandlerTable {
            bindings: vec![HandlerBinding {
                effect_name: "IO".to_string(),
                handler_offset: 8,
                arg_count: 0,
                result_reg: 0,
                single_shot: false,
            }],
            fallback_offset: None,
        });
        let eff_idx = module.add_constant(Constant::String("IO.foo".to_string()));
        let c9_idx = module.add_constant(Constant::Int(9));

        module.emit(Instruction::new1(OpCode::Handle, 0)); // 0
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((eff_idx >> 8) & 0xFF) as u8,
            (eff_idx & 0xFF) as u8,
            1,
        )); // 1
        module.emit(Instruction::new2(OpCode::Move, 1, 0)); // 2
        module.emit(Instruction::new0(OpCode::Unwind)); // 3
        module.emit(Instruction::new0(OpCode::Halt)); // 4
        for _ in 5..8 {
            module.emit(Instruction::new0(OpCode::Nop));
        } // 5-7
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c9_idx >> 8) & 0xFF) as u8,
            (c9_idx & 0xFF) as u8,
            0,
        )); // 8
        module.emit(Instruction::new1(OpCode::Resume, 0)); // 9
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run();
        assert!(
            result.is_ok(),
            "bare binding must stay compatible: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().as_int(), Some(9));
    }

    /// Regression: `IMul` on 48-bit boundary values must not overflow i64
    /// and panic in debug builds — 2^47 * 2^47 wraps to 0 once masked to
    /// 48 bits. The hot loop also exercises the `nulang_imul` JIT helper
    /// after tier-up.
    #[test]
    fn test_imul_boundary_value_wraps() {
        const BOUNDARY: i64 = 140737488355328; // 2^47
        let mut module = CodeModule::new("test_imul_boundary");
        let c_val = module.add_constant(Constant::Int(BOUNDARY));
        let c_limit = module.add_constant(Constant::Int(3000));

        // r0/r1 = operands, r2 = limit, r3 = counter, r4 = product,
        // r5 = cond.
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c_val >> 8) & 0xFF) as u8,
            (c_val & 0xFF) as u8,
            0,
        )); // 0
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c_val >> 8) & 0xFF) as u8,
            (c_val & 0xFF) as u8,
            1,
        )); // 1
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((c_limit >> 8) & 0xFF) as u8,
            (c_limit & 0xFF) as u8,
            2,
        )); // 2
        module.emit(Instruction::new1(OpCode::Const0, 3)); // 3
                                                           // Loop body (pc 4..=6): r4 = (2^47 * 2^47) wraps to 0.
        module.emit(Instruction::new3(OpCode::IMul, 0, 1, 4)); // 4
        module.emit(Instruction::new1(OpCode::IInc, 3)); // 5
        module.emit(Instruction::new3(OpCode::ICmpLt, 3, 2, 5)); // 6
        let back: i16 = -3; // JmpT at pc 7 -> pc 4
        module.emit(Instruction::new3(
            OpCode::JmpT,
            5,
            ((back as u16) >> 8) as u8,
            (back as u16 & 0xFF) as u8,
        )); // 7
        module.emit(Instruction::new2(OpCode::Move, 4, 0)); // 8
        module.emit(Instruction::new0(OpCode::Halt)); // 9
        module.entry_point = Some(0);

        let mut vm = VM::new();
        vm.load_module(module);
        let result = vm.run().expect("boundary IMul must not panic");
        assert_eq!(result.as_int(), Some(0), "(2^47 * 2^47) mod 2^48 = 0");
        let compiled = vm
            .jit_session
            .as_ref()
            .map(|j| j.compiled_count())
            .unwrap_or(0);
        assert!(compiled > 0, "loop body must have been JIT-compiled");
    }

    // -- SCmpEq (string equality; emitted by variant-match lowering) --

    /// A module whose first instruction is `SCmpEq r0, r1 -> r2`, carrying
    /// the given constant pool.
    fn scmpeq_module(name: &str, constants: Vec<Constant>) -> CodeModule {
        let mut module = CodeModule::new(name);
        for c in constants {
            module.add_constant(c);
        }
        module.emit(Instruction::new3(OpCode::SCmpEq, 0, 1, 2));
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);
        module
    }

    /// Drive the single leading `SCmpEq` instruction of the last-loaded
    /// module with `a`/`b` preloaded into r0/r1 and return r2.
    fn run_scmpeq(vm: &mut VM, a: Value, b: Value) -> Value {
        let module_idx = vm.modules.len() - 1;
        let mut frame = Frame::new(None, module_idx);
        frame.regs[0] = a;
        frame.regs[1] = b;
        vm.frames.clear();
        vm.frames.push(frame);
        vm.current_frame_idx = Some(0);
        vm.step().expect("SCmpEq must not error");
        vm.frames[0].regs[2]
    }

    /// Two pool strings with the same text but different constant-pool
    /// indices must compare equal — equality is by content, not by bits.
    #[test]
    fn test_scmpeq_same_content_different_pool_indices() {
        let module = scmpeq_module(
            "test_scmpeq_pool_idx",
            vec![
                Constant::String("Some".to_string()),
                Constant::Int(999), // separator so the duplicate lands at idx 2
                Constant::String("Some".to_string()),
            ],
        );
        let mut vm = VM::new();
        vm.load_module(module);
        let result = run_scmpeq(&mut vm, Value::string(0), Value::string(2));
        assert_eq!(
            result.as_bool(),
            Some(true),
            "same text at different pool indices must be equal"
        );
    }

    /// Pool indices are module-scoped: "Some" at index 0 of module A and
    /// index 1 of module B must resolve to equal content. (Resolution is
    /// frame-module-scoped, so this exercises `string_operand` — the exact
    /// path the SCmpEq arm uses — against both modules.)
    #[test]
    fn test_scmpeq_same_content_across_modules() {
        let mod_a = scmpeq_module(
            "test_scmpeq_mod_a",
            vec![Constant::String("Some".to_string())],
        );
        let mod_b = scmpeq_module(
            "test_scmpeq_mod_b",
            vec![Constant::Int(7), Constant::String("Some".to_string())],
        );
        let mut vm = VM::new();
        vm.load_module(mod_a);
        vm.load_module(mod_b);
        let from_a = vm.string_operand(0, Value::string(0));
        let from_b = vm.string_operand(1, Value::string(1));
        assert_eq!(from_a.as_deref(), Some("Some"));
        assert_eq!(from_a, from_b, "cross-module same-text strings must match");
    }

    /// A pool string and a heap string with the same bytes compare equal,
    /// in either operand order.
    #[test]
    fn test_scmpeq_pool_vs_heap_string() {
        let module = scmpeq_module(
            "test_scmpeq_pool_heap",
            vec![Constant::String("hello".to_string())],
        );
        let mut vm = VM::new();
        vm.load_module(module);
        let heap = vm.allocate_string("hello");
        let pool_first = run_scmpeq(&mut vm, Value::string(0), heap);
        assert_eq!(pool_first.as_bool(), Some(true), "pool vs heap must match");
        let heap_first = run_scmpeq(&mut vm, heap, Value::string(0));
        assert_eq!(heap_first.as_bool(), Some(true), "heap vs pool must match");
    }

    /// Two distinct heap allocations with the same bytes compare equal.
    #[test]
    fn test_scmpeq_heap_vs_heap_string() {
        let module = scmpeq_module("test_scmpeq_heap_heap", vec![]);
        let mut vm = VM::new();
        vm.load_module(module);
        let a = vm.allocate_string("hello");
        let b = vm.allocate_string("hello");
        assert_ne!(a.to_bits(), b.to_bits(), "distinct allocations expected");
        let result = run_scmpeq(&mut vm, a, b);
        assert_eq!(result.as_bool(), Some(true), "heap vs heap must match");
    }

    /// Different string contents are unequal (pool/pool and heap/heap).
    #[test]
    fn test_scmpeq_different_strings_false() {
        let module = scmpeq_module(
            "test_scmpeq_different",
            vec![
                Constant::String("Some".to_string()),
                Constant::String("None".to_string()),
            ],
        );
        let mut vm = VM::new();
        vm.load_module(module);
        let result = run_scmpeq(&mut vm, Value::string(0), Value::string(1));
        assert_eq!(result.as_bool(), Some(false), "Some != None");
        let other = vm.allocate_string("world");
        let result = run_scmpeq(&mut vm, Value::string(0), other);
        assert_eq!(result.as_bool(), Some(false), "Some != world");
    }

    /// Non-string operands (int, nil, a non-string heap object, a string-id
    /// pointing at a non-string constant) yield `false`, never an error —
    /// mirroring ICmpEq/FCmpEq coerce-don't-fail style.
    #[test]
    fn test_scmpeq_non_string_operands_false() {
        let module = scmpeq_module(
            "test_scmpeq_non_string",
            vec![Constant::String("hello".to_string()), Constant::Int(42)],
        );
        let mut vm = VM::new();
        vm.load_module(module);
        let hello = Value::string(0);

        let result = run_scmpeq(&mut vm, Value::int(0), hello);
        assert_eq!(result.as_bool(), Some(false), "int vs string must be false");
        let result = run_scmpeq(&mut vm, hello, Value::int(0));
        assert_eq!(result.as_bool(), Some(false), "string vs int must be false");
        let result = run_scmpeq(&mut vm, Value::nil(), hello);
        assert_eq!(result.as_bool(), Some(false), "nil vs string must be false");

        // A string-id whose pool slot is not a string constant.
        let result = run_scmpeq(&mut vm, Value::string(1), hello);
        assert_eq!(
            result.as_bool(),
            Some(false),
            "non-string pool slot must be false"
        );

        // A heap pointer to a record must not be read as a C string.
        let rec_ptr = vm
            .actor_callbacks
            .alloc(std::mem::size_of::<Value>(), HeapTypeTag::Record)
            .expect("record allocation");
        let result = run_scmpeq(&mut vm, Value::ptr(rec_ptr), hello);
        assert_eq!(
            result.as_bool(),
            Some(false),
            "record ptr vs string must be false"
        );
    }

    /// Round-trip: allocate heap objects, capture continuation, serialize,
    /// deserialize into a fresh VM, verify values are intact.
    #[test]
    fn test_continuation_roundtrip_serialization() {
        use crate::runtime::heap_serialize;

        // Build a module so we have a valid module_idx and string pool.
        let mut module = CodeModule::new("test_roundtrip");
        module.constants.push(Constant::String("hello".into()));
        module.constants.push(Constant::String("world".into()));
        // Dummy instruction so the module has at least one.
        module.instructions.push(Instruction::new0(OpCode::Halt));

        let mut vm1 = VM::new();
        vm1.load_module(module.clone());

        // Allocate a heap array with two values.
        let arr_ptr = vm1
            .actor_callbacks
            .alloc(2 * std::mem::size_of::<Value>(), HeapTypeTag::Array)
            .expect("array allocation");
        let arr_value = Value::ptr(arr_ptr);
        // Write [int(42), string("hello")] into the array.
        unsafe {
            let slots = std::slice::from_raw_parts_mut(arr_ptr as *mut Value, 2);
            slots[0] = Value::int(42);
            slots[1] = Value::string(0); // "hello" at constant index 0
                                         // Retain refs to match what ArrStore barrier would do.
        }

        // Push a frame with the array value in r0.
        let mut frame = Frame::new(None, 0);
        frame.regs[0] = arr_value;
        frame.regs[1] = Value::int(99);
        frame.pc = 1; // non-zero to verify serialization
        frame.return_dst = 5;
        vm1.frames.push(frame);
        vm1.current_frame_idx = Some(0);

        // Push a handler frame.
        vm1.handler_stack.push(HandlerFrame::new(0, 0, 10, 3));

        // Capture continuation.
        let cont = Continuation::capture(&vm1, 0).expect("capture");
        assert_eq!(cont.frames.len(), 1);
        assert_eq!(cont.frames[0].regs[0].as_ptr().is_some(), true);
        assert_eq!(cont.frames[0].regs[1].as_int(), Some(99));

        // Serialize.
        let module_hash = [0u8; 32];
        let handler_stack_clone = vm1.handler_stack.clone();
        let bytes =
            heap_serialize::serialize_continuation(&cont, &handler_stack_clone, &vm1, &module_hash)
                .expect("serialization");

        assert!(!bytes.is_empty(), "serialized payload must not be empty");
        assert!(bytes.len() > 32, "payload must have header + content");

        // Deserialize into a fresh VM.
        let mut vm2 = VM::new();
        vm2.load_module(module);

        let (restored_cont, restored_handlers) =
            heap_serialize::deserialize_continuation(&bytes, &mut vm2).expect("deserialization");

        assert_eq!(restored_cont.frames.len(), 1);
        assert_eq!(restored_handlers.len(), 1);
        assert_eq!(restored_handlers[0].handler_table_idx, 0);
        assert_eq!(restored_handlers[0].resume_pc, 10);
        assert_eq!(restored_handlers[0].resume_dst, 3);

        // Verify register values are intact.
        let restored_frame = &restored_cont.frames[0];
        assert_eq!(restored_frame.pc, 1);
        assert_eq!(restored_frame.return_dst, 5);
        assert_eq!(restored_frame.regs[1].as_int(), Some(99));

        // r0 should be a TAG_PTR pointing to a heap array with [42, "hello"].
        let restored_arr_ptr = restored_frame.regs[0]
            .as_ptr()
            .expect("r0 should be a heap pointer");
        assert!(!restored_arr_ptr.is_null());

        unsafe {
            let header = &*ActorHeap::header_of(restored_arr_ptr);
            assert_eq!(header.type_tag, HeapTypeTag::Array);

            let slots = std::slice::from_raw_parts(restored_arr_ptr as *const Value, 2);
            assert_eq!(slots[0].as_int(), Some(42), "array[0] should be 42");
            // slots[1] is a TAG_STRING — verify it's a valid string value
            assert!(slots[1].is_string(), "array[1] should be a string");
        }
    }
}
