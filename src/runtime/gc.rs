//! ORCA (Optimized Reference Counting Architecture) garbage collector.
//!
//! This module implements the ORCA reference counting protocol for Nulang's
//! actor-based runtime. Each actor owns a private heap; objects are never
//! moved between heaps. Cross-actor references use a distributed ref-counting
//! protocol that avoids stop-the-world pauses.
//!
//! # Core Concepts
//!
//! Each heap object carries three ref-count fields:
//!
//! | Field | Meaning |
//! |-------|---------|
//! | `local_count`    | References held by the **owning** actor |
//! | `foreign_count`  | References held by **other** actors (or in-flight) |
//! | `sticky`         | If `true`, the object is never collected |
//!
//! An object is eligible for deallocation when:
//! ```text
//! !sticky && local_count == 0 && foreign_count == 0
//! ```
//!
//! # ORCA Cross-Actor Protocol
//!
//! When actor A sends a reference to object O (owned by A) to actor B:
//!
//! 1. A increments O.foreign_count (the reference is now "in flight").
//! 2. A hands a [`ForeignRefOp`] to the [`OrcaCoordinator`].
//! 3. The coordinator delivers the op to B between scheduling rounds.
//! 4. B processes the op: decrements O.foreign_count.
//!
//! When B receives a reference to an object on **its own** heap from a foreign
//! actor, B calls [`OrcaGc::receive_ref`] which:
//!
//! 1. Decrements foreign_count (the in-flight reference has landed).
//! 2. Increments local_count (B now holds a local reference).
//!
//! # Safety
//!
//! All pointer-manipulating methods are `unsafe` and document their preconditions.
//! The caller (the runtime) is responsible for ensuring that payload pointers are
//! valid and that no data races occur when multiple actors access the same object.

use crate::runtime::heap::{OrcaHeader, TypeTag};

// ---------------------------------------------------------------------------
// OrcaHeap trait
// ---------------------------------------------------------------------------

/// Trait abstracting over the per-actor heap so that `OrcaGc` can be tested
/// with a mock allocator.
///
/// The real `ActorHeap` (in `heap.rs`) implements this trait.
pub trait OrcaHeap {
    /// Allocate `payload_size` bytes for user data, preceded by an
    /// `OrcaHeader`.  Returns a pointer to the **payload** (not the header).
    ///
    /// The implementation must fully initialise the [`OrcaHeader`] so that
    /// `header_ptr` returns a valid header.
    fn alloc_payload(&mut self, payload_size: usize) -> Option<*mut u8>;

    /// Deallocate a block previously returned by `alloc_payload`.
    ///
    /// # Safety
    /// `payload_ptr` must be a live pointer returned by `alloc_payload`.
    unsafe fn free_payload(&mut self, payload_ptr: *mut u8);

    /// Given a payload pointer, return a mutable pointer to the `OrcaHeader`
    /// that precedes it.
    ///
    /// # Safety
    /// `payload_ptr` must be a valid payload pointer from this heap.
    unsafe fn header_ptr(&self, payload_ptr: *mut u8) -> *mut OrcaHeader;
}

// ---------------------------------------------------------------------------
// GC Statistics
// ---------------------------------------------------------------------------

/// Counters for GC-related events.
///
/// Plain `u64`s, not atomics: the runtime is a single-threaded synchronous
/// coordinator, so the scheduler thread is the only reader and writer.
/// Aggregation across actors (`Runtime::gc_stats`) also happens on that
/// thread.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GcStats {
    /// Total objects allocated.
    pub objects_allocated: u64,
    /// Total objects freed.
    pub objects_freed: u64,
    /// Local reference creations.
    pub local_refs_created: u64,
    /// Local reference drops.
    pub local_refs_dropped: u64,
    /// Foreign reference sends.
    pub foreign_refs_sent: u64,
    /// Foreign reference receives.
    pub foreign_refs_received: u64,
    /// Cycles detected by the ORCA cycle detector (`orca_cycle::CycleDetector`).
    pub cycles_detected: u64,
    /// Total bytes allocated.
    pub bytes_allocated: u64,
    /// Total bytes freed.
    pub bytes_freed: u64,
}

impl Default for GcStats {
    fn default() -> Self {
        GcStats {
            objects_allocated: 0,
            objects_freed: 0,
            local_refs_created: 0,
            local_refs_dropped: 0,
            foreign_refs_sent: 0,
            foreign_refs_received: 0,
            cycles_detected: 0,
            bytes_allocated: 0,
            bytes_freed: 0,
        }
    }
}

impl GcStats {
    /// Reset all counters to zero.
    pub fn reset(&mut self) {
        *self = GcStats::default();
    }
}

// ---------------------------------------------------------------------------
// ForeignRefOp
// ---------------------------------------------------------------------------

/// A pending foreign-reference operation that must be delivered to another
/// actor's GC engine.
///
/// Created by [`OrcaGc::send_ref_to`] and consumed by
/// [`OrcaGc::process_foreign_op`] on the target actor.
#[derive(Debug)]
pub struct ForeignRefOp {
    /// The actor that should process this operation.
    pub target_actor: u64,
    /// The actor that **owns** the object (recorded at send time from the
    /// object's header).  Consumers must use this instead of dereferencing
    /// `object_header` to find the owner: the owner's heap may have been
    /// retired after the actor exited, and reading the header to discover
    /// the owner would then be a use-after-free.
    pub owner_actor: u64,
    /// Pointer to the `OrcaHeader` of the object being referenced.
    ///
    /// This is a raw pointer into the **sender's** heap; it remains valid
    /// as long as the object is alive because actors are never moved in
    /// memory and deallocation is deferred until all references are gone.
    /// An in-flight op keeps `foreign_count > 0`, which in turn keeps the
    /// owning heap alive (retired, not freed) even if the owner exits.
    pub object_header: *mut OrcaHeader,
    /// Amount to adjust the foreign count by.
    /// `+1` = increment, `-1` = decrement.
    pub delta: i32,
}

// Manual impl because `*mut OrcaHeader` doesn't implement `Send` by default.
// The pointer is always into a heap that outlives the operation.
unsafe impl Send for ForeignRefOp {}

// ---------------------------------------------------------------------------
// OrcaGc — per-actor GC engine
// ---------------------------------------------------------------------------

/// ORCA reference-counting engine tied to a single actor.
///
/// Each actor has exactly one `OrcaGc` instance.  It manages:
///
/// * Allocations on the actor's heap.
/// * Local reference counting.
/// * Deferred deallocations (objects whose `local_count` hit zero but still
///   have foreign references).
/// * Foreign ref operations queued for the coordinator.
///
/// # Thread Safety
///
/// `OrcaGc` is **not** `Send` nor `Sync`.  It should only be accessed from
/// the thread that runs the owning actor.  Cross-actor communication happens
/// via [`ForeignRefOp`] messages handled by the [`OrcaCoordinator`].
///
/// The runtime is a single-threaded synchronous coordinator (one scheduler
/// thread runs every actor step, `process_gc_ops`, and cycle detection), so
/// header refcounts and these stats are plain integers mutated through
/// `&mut self` — no atomic operations are needed or used.
#[derive(Debug)]
pub struct OrcaGc {
    actor_id: u64,
    /// Objects whose `local_count` reached zero but which could not be freed
    /// because `foreign_count` was still positive.  We retry them periodically
    /// in [`process_deferred`].
    deferred_decrements: Vec<*mut OrcaHeader>,
    /// Foreign-ref operations waiting to be handed off to the coordinator.
    /// The runtime drains this vector between scheduling rounds.
    foreign_ref_queue: Vec<ForeignRefOp>,
    /// Foreign references this actor has **received** and still holds.
    /// Each entry is `(owning actor id, header of the held object)`.
    /// Every hold keeps the object's `foreign_count` elevated by one, so
    /// the object (and, if the owner has exited, its retired heap) stays
    /// alive until the runtime releases the hold when this actor exits.
    held_foreign_refs: Vec<(u64, *mut OrcaHeader)>,
    /// Per-actor statistics.
    stats: GcStats,
}

impl OrcaGc {
    /// Create a new ORCA GC engine for the given actor.
    ///
    /// The engine starts with empty deferred and foreign-ref queues.
    pub fn new(actor_id: u64) -> Self {
        OrcaGc {
            actor_id,
            deferred_decrements: Vec::new(),
            foreign_ref_queue: Vec::new(),
            held_foreign_refs: Vec::new(),
            stats: GcStats::default(),
        }
    }

    /// Allocate a new object on the actor's heap.
    ///
    /// The returned pointer points to the **payload** (user data).  An
    /// [`OrcaHeader`] is laid out immediately before it.  The object's
    /// `local_count` is initialised to 1 (the creator holds the sole
    /// reference).
    ///
    /// Returns `None` if the heap cannot satisfy the allocation.
    pub fn alloc_object(
        &mut self,
        heap: &mut dyn OrcaHeap,
        payload_size: usize,
        type_tag: TypeTag,
    ) -> Option<*mut u8> {
        if payload_size == 0 {
            // Zero-sized payloads are not supported.
            return None;
        }

        let payload_ptr = heap.alloc_payload(payload_size)?;

        // `alloc_payload` initialises the whole header.  We only need to
        // correct the actor_id (the heap may not know it yet in tests) and
        // set the real type tag (alloc_payload uses TypeTag::Raw as a
        // placeholder).
        // SAFETY: payload_ptr just allocated by heap.alloc_payload above;
        // header_ptr points to the valid OrcaHeader immediately before the
        // payload (repr(C) layout). Single-threaded runtime guarantees no
        // concurrent writes to this header.
        unsafe {
            let header = &mut *heap.header_ptr(payload_ptr);
            header.actor_id = self.actor_id;
            header.type_tag = type_tag;
        }

        self.stats.objects_allocated += 1;
        self.stats.bytes_allocated += payload_size as u64;

        Some(payload_ptr)
    }

    /// Create a local reference to an object.
    ///
    /// Increments the object's `local_count`.
    ///
    /// # Safety
    /// * `payload_ptr` must be a valid payload pointer previously returned by
    ///   `alloc_object`.
    /// * The object must be owned by this actor.
    /// * The caller must ensure there are no concurrent modifications to the
    ///   object's reference counts.
    pub unsafe fn local_ref(&mut self, heap: &dyn OrcaHeap, payload_ptr: *mut u8) {
        // SAFETY: caller guarantees payload_ptr is valid. Single-threaded
        // runtime: no other thread mutates this header concurrently.
        let header = &mut *heap.header_ptr(payload_ptr);
        debug_assert_eq!(
            header.actor_id, self.actor_id,
            "local_ref called on object not owned by this actor"
        );

        header.ref_count += 1;
        self.stats.local_refs_created += 1;
    }

    /// Drop a local reference to an object.
    ///
    /// Decrements `local_count`.  If both `local_count` and `foreign_count`
    /// reach zero (and the object is not sticky), the object is freed
    /// immediately and `true` is returned.
    ///
    /// If `local_count` reaches zero but `foreign_count` is still positive,
    /// the object is added to the deferred-decrement list and will be retried
    /// by [`process_deferred`].
    ///
    /// # Safety
    /// * `payload_ptr` must be a valid payload pointer with at least one
    ///   outstanding local reference.
    /// * The object must be owned by this actor.
    pub unsafe fn drop_local_ref(&mut self, heap: &mut dyn OrcaHeap, payload_ptr: *mut u8) -> bool {
        // SAFETY: caller guarantees payload_ptr is valid. Single-threaded
        // runtime: no other thread mutates this header concurrently.
        let header = &mut *heap.header_ptr(payload_ptr);
        debug_assert_eq!(
            header.actor_id, self.actor_id,
            "drop_local_ref called on object not owned by this actor"
        );

        header.ref_count -= 1;

        self.stats.local_refs_dropped += 1;

        // If the count is now 0, the object may be reclaimable.
        if header.ref_count == 0 {
            let foreign = header.foreign_count;
            let is_sticky = header.sticky;

            if foreign == 0 && !is_sticky {
                // SAFETY: payload_ptr is a live allocation on this heap.
                unsafe { self.free_object(heap, payload_ptr) };
                true
            } else {
                // Cannot free yet — foreign refs exist or object is pinned.
                // Defer and retry later.
                self.deferred_decrements.push(heap.header_ptr(payload_ptr));
                false
            }
        } else {
            false
        }
    }

    /// Send a reference to another actor (ORCA protocol).
    ///
    /// Increments the object's `foreign_count` (the reference is now
    /// "in flight") and returns a [`ForeignRefOp`] that the caller must
    /// deliver to the target actor's GC (via the [`OrcaCoordinator`]).
    ///
    /// # Safety
    /// * `payload_ptr` must be a valid payload pointer owned by this actor.
    /// * The object must have at least one local reference.
    pub unsafe fn send_ref_to(
        &mut self,
        heap: &dyn OrcaHeap,
        payload_ptr: *mut u8,
        target_actor: u64,
    ) -> ForeignRefOp {
        // SAFETY: caller guarantees payload_ptr is valid. Single-threaded
        // runtime: no other thread mutates this header concurrently.
        let header_ptr = heap.header_ptr(payload_ptr);
        let header = &mut *header_ptr;

        debug_assert_eq!(
            header.actor_id, self.actor_id,
            "send_ref_to called on object not owned by this actor"
        );

        // Mark the reference as in-flight.
        header.foreign_count += 1;
        self.stats.foreign_refs_sent += 1;

        ForeignRefOp {
            target_actor,
            owner_actor: self.actor_id,
            object_header: header_ptr,
            delta: -1, // target will decrement foreign_count on receipt
        }
    }

    /// Receive a reference from another actor (ORCA protocol).
    ///
    /// Called when a foreign actor returns a reference to an object that
    /// lives on **this** actor's heap.  Decrements `foreign_count` (the
    /// in-flight reference has landed) and increments `local_count` (this
    /// actor now holds a local reference again).
    ///
    /// # Safety
    /// * `payload_ptr` must point to an object on this actor's heap.
    pub unsafe fn receive_ref(&mut self, heap: &dyn OrcaHeap, payload_ptr: *mut u8) {
        // SAFETY: caller guarantees payload_ptr is valid. Single-threaded
        // runtime: no other thread mutates this header concurrently.
        let header = &mut *heap.header_ptr(payload_ptr);
        debug_assert_eq!(
            header.actor_id, self.actor_id,
            "receive_ref called on object not owned by this actor"
        );

        // In-flight reference has arrived.
        header.foreign_count -= 1;

        // We now hold a local reference.
        header.ref_count += 1;

        self.stats.foreign_refs_received += 1;
    }

    /// Take a receiver-side hold on an object owned by **this** actor.
    ///
    /// Called by the runtime when another actor receives a message carrying
    /// a pointer to one of this actor's objects.  The hold increments
    /// `foreign_count` so the object cannot be freed while the receiver
    /// still holds it (in a register, state field, or container).  The
    /// runtime records the hold on the *receiver's* engine via
    /// [`record_held_ref`](Self::record_held_ref) and releases it (a `-1`
    /// foreign op) when the receiver exits.
    ///
    /// # Safety
    /// * `payload_ptr` must point to a live object owned by this actor.
    pub unsafe fn inc_foreign_hold(&mut self, heap: &dyn OrcaHeap, payload_ptr: *mut u8) {
        // SAFETY: caller guarantees payload_ptr is valid. Single-threaded
        // runtime: no other thread mutates this header concurrently.
        let header = &mut *heap.header_ptr(payload_ptr);
        debug_assert_eq!(
            header.actor_id, self.actor_id,
            "inc_foreign_hold called on object not owned by this actor"
        );

        header.foreign_count += 1;
        self.stats.foreign_refs_received += 1;
    }

    /// Record that this actor (the receiver) holds a foreign reference to
    /// the object with header `object_header` owned by `owner_actor`.
    ///
    /// The matching `foreign_count` increment must already have happened
    /// (via the owner's [`inc_foreign_hold`](Self::inc_foreign_hold) or a
    /// direct header bump for a retired owner heap).  Holds are released
    /// by the runtime when this actor exits.
    pub fn record_held_ref(&mut self, owner_actor: u64, object_header: *mut OrcaHeader) {
        self.held_foreign_refs.push((owner_actor, object_header));
    }

    /// Drain the list of foreign references this actor holds.
    ///
    /// The runtime calls this when the actor exits and applies a `-1`
    /// foreign-count decrement for each entry on the owning side.  Draining
    /// makes a repeated call a no-op, so exit handling can be idempotent.
    pub fn take_held_refs(&mut self) -> Vec<(u64, *mut OrcaHeader)> {
        std::mem::take(&mut self.held_foreign_refs)
    }

    /// Process a foreign ref operation delivered from another actor.
    ///
    /// Applies `op.delta` to the target object's `foreign_count`.  If the
    /// count drops to zero and `local_count` is also zero (and the object is
    /// not sticky), the object is freed.
    ///
    /// This method is called on the **owning** actor's GC engine by the
    /// runtime's `process_gc_ops`.
    pub fn process_foreign_op(&mut self, heap: &mut dyn OrcaHeap, op: ForeignRefOp) {
        // SAFETY: the coordinator only delivers ops whose object_header is
        // a live pointer into a sender heap.  The object stays alive because
        // the foreign_count was incremented when the ref was sent.
        // Single-threaded runtime: no other thread mutates this header
        // concurrently.
        let header = unsafe { &mut *op.object_header };

        let prev_foreign = if op.delta >= 0 {
            header.foreign_count += op.delta as u32;
            header.foreign_count
        } else {
            let delta = (-op.delta) as u32;
            header.foreign_count -= delta;
            // The count *after* subtraction.
            header.foreign_count
        };

        // If foreign count reached zero and local count is also zero, free.
        let prev_foreign_for_check = prev_foreign;
        if prev_foreign_for_check == 0 {
            let local = header.ref_count;
            let is_sticky = header.sticky;
            if local == 0 && !is_sticky {
                // We need the payload pointer to free.  Compute it from the
                // header pointer.
                let payload_ptr = Self::payload_from_header(op.object_header);
                // SAFETY: object_header came from a live allocation; the
                // coordinator only delivers ops for live objects.
                unsafe { self.free_object(heap, payload_ptr) };

                // Remove from deferred list so process_deferred doesn't
                // access freed memory.
                if let Some(pos) = self
                    .deferred_decrements
                    .iter()
                    .position(|&h| h == op.object_header)
                {
                    self.deferred_decrements.swap_remove(pos);
                }
            }
        }
    }

    /// Set the sticky flag on an object (make it immortal).
    ///
    /// Sticky objects are never collected, even when all reference counts
    /// drop to zero.  This is used for global constants and runtime
    /// meta-objects.
    ///
    /// # Safety
    /// `payload_ptr` must be a valid payload pointer owned by this actor.
    pub unsafe fn pin_object(&mut self, heap: &dyn OrcaHeap, payload_ptr: *mut u8) {
        // SAFETY: caller guarantees payload_ptr is valid. Single-threaded
        // runtime: no other thread mutates this header concurrently.
        let header = &mut *heap.header_ptr(payload_ptr);
        debug_assert_eq!(header.actor_id, self.actor_id);
        header.sticky = true;
    }

    /// Unset the sticky flag, allowing the object to be collected when its
    /// reference counts drop to zero.
    ///
    /// # Safety
    /// `payload_ptr` must be a valid payload pointer owned by this actor.
    pub unsafe fn unpin_object(&mut self, heap: &dyn OrcaHeap, payload_ptr: *mut u8) {
        // SAFETY: caller guarantees payload_ptr is valid. Single-threaded
        // runtime: no other thread mutates this header concurrently.
        let header = &mut *heap.header_ptr(payload_ptr);
        debug_assert_eq!(header.actor_id, self.actor_id);
        header.sticky = false;
    }

    /// Try to free all deferred deallocations.
    ///
    /// Call this periodically when the actor is idle (e.g., after processing
    /// all mailbox messages).  Objects that were deferred because they had
    /// foreign references are rechecked; if `foreign_count` has since dropped
    /// to zero, they are freed.
    ///
    /// Entries are drained one at a time because freeing a container
    /// recursively releases its children, which can push new entries onto —
    /// or free objects still present in — this list mid-pass.
    pub fn process_deferred(&mut self, heap: &mut dyn OrcaHeap) {
        let mut i = 0;
        while i < self.deferred_decrements.len() {
            let header_ptr = self.deferred_decrements[i];
            // SAFETY: entries are live headers; free_object removes an entry
            // before its object is freed, so a retained entry is never stale.
            let header = unsafe { &*header_ptr };
            let local = header.ref_count;
            let foreign = header.foreign_count;
            let is_sticky = header.sticky;

            if local == 0 && foreign == 0 && !is_sticky {
                self.deferred_decrements.swap_remove(i);
                let payload_ptr = Self::payload_from_header(header_ptr);
                // SAFETY: payload_ptr is derived from a live header.
                unsafe { self.free_object(heap, payload_ptr) };
            } else {
                i += 1;
            }
        }
    }

    /// Drain and return the queued foreign-ref operations.
    ///
    /// The runtime calls this and hands the ops to the coordinator.
    pub fn drain_foreign_ops(&mut self) -> Vec<ForeignRefOp> {
        std::mem::take(&mut self.foreign_ref_queue)
    }

    /// Queue a foreign-ref operation locally (used by the runtime when
    /// `send_ref_to` is called during message construction).
    pub fn queue_foreign_op(&mut self, op: ForeignRefOp) {
        self.foreign_ref_queue.push(op);
    }

    /// Return a reference to the actor's GC statistics.
    pub fn stats(&self) -> &GcStats {
        &self.stats
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Remove `header` from the deferred-decrement list.
    ///
    pub fn remove_deferred(&mut self, header: *mut OrcaHeader) {
        if let Some(pos) = self.deferred_decrements.iter().position(|&h| h == header) {
            self.deferred_decrements.swap_remove(pos);
        }
    }

    /// Free an object and update statistics.
    ///
    /// Container payloads (arrays, records, tuples) are `Value` arrays whose
    /// slots each own a counted local reference (taken by the
    /// `ArrStore`/`RecS`/`FieldS` write barriers in the VM). Freeing the
    /// container must release those references or every element would leak.
    /// Children are released before the block itself is freed; releasing a
    /// child's last reference frees it recursively (nesting depth is bounded
    /// by the number of live objects, which the fixed-size actor heap caps).
    ///
    /// Child release is skipped when the object is owned by a different
    /// actor than this GC engine: `process_foreign_op` can free an object
    /// that lives on the sender's heap, and touching its children here would
    /// operate on the wrong heap (the cross-actor protocol reclaims such
    /// objects through the owner side instead).
    ///
    /// # Safety
    /// `payload_ptr` must be a live pointer returned by `alloc_object`.
    unsafe fn free_object(&mut self, heap: &mut dyn OrcaHeap, payload_ptr: *mut u8) {
        let header_ptr = heap.header_ptr(payload_ptr);
        let (size, type_tag, owner) = {
            let header = &*header_ptr;
            (header.payload_size, header.type_tag, header.actor_id)
        };

        if owner == self.actor_id
            && matches!(
                type_tag,
                TypeTag::Array | TypeTag::Record | TypeTag::Tuple | TypeTag::Map
            )
        {
            let slot_count = size / std::mem::size_of::<crate::vm::Value>();
            // SAFETY: container payloads are laid out as `slot_count` Values
            // by the VM's ArrAlloc/RecMk/TupleMk.
            let slots =
                std::slice::from_raw_parts(payload_ptr as *const crate::vm::Value, slot_count);
            for slot in slots {
                if let Some(child) = slot.as_ptr() {
                    // SAFETY: the slot held a counted local reference to an
                    // object on this heap; releasing it balances the barrier
                    // retain exactly once.
                    self.drop_local_ref(heap, child);
                }
            }
        }

        heap.free_payload(payload_ptr);

        // A recursive child release above may have freed objects that were
        // still queued here; never let the deferred list point at freed
        // memory (process_deferred drains one entry at a time for the same
        // reason). Linear scan + swap_remove avoids Vec::retain's full copy.
        if let Some(pos) = self
            .deferred_decrements
            .iter()
            .position(|&h| h == header_ptr)
        {
            self.deferred_decrements.swap_remove(pos);
        }

        self.stats.objects_freed += 1;
        self.stats.bytes_freed += size as u64;
    }

    /// Given a header pointer, compute the payload pointer.
    ///
    /// This is the inverse of `OrcaHeap::header_ptr`.
    fn payload_from_header(header_ptr: *mut OrcaHeader) -> *mut u8 {
        let header_size = std::mem::size_of::<OrcaHeader>();
        // SAFETY: header_ptr is always followed by the payload.
        unsafe { (header_ptr as *mut u8).add(header_size) }
    }
}

// ---------------------------------------------------------------------------
// OrcaCoordinator — global singleton
// ---------------------------------------------------------------------------

/// Global ORCA coordinator that lives inside the [`Runtime`](super::Runtime).
///
/// Responsible for:
/// * Collecting [`ForeignRefOp`]s from all actor GCs.
/// * Delivering them to the correct target actors between scheduling rounds.
/// * Tracking global GC statistics.
/// * Deciding when to trigger cycle detection (via `orca_cycle::CycleDetector`).
pub struct OrcaCoordinator {
    /// Queue of foreign-ref operations waiting to be delivered.
    ///
    /// Actor GCs drain their local queues into here; the coordinator then
    /// routes each op to the correct target actor.
    pub pending_ops: Vec<ForeignRefOp>,
    /// How many delivered ops must accumulate before we run cycle detection.
    pub cycle_detect_threshold: usize,
    /// Total number of ops delivered since last cycle check.
    delivered_count: usize,
    /// Global statistics (aggregated from all actor GCs).
    pub stats: GcStats,
}

impl OrcaCoordinator {
    /// Create a new coordinator with default thresholds.
    pub fn new() -> Self {
        OrcaCoordinator {
            pending_ops: Vec::new(),
            cycle_detect_threshold: 10_000,
            delivered_count: 0,
            stats: GcStats::default(),
        }
    }

    /// Submit a foreign reference operation to be delivered.
    ///
    /// Typically called by the runtime after `OrcaGc::send_ref_to` returns
    /// an op during message construction.
    pub fn submit_op(&mut self, op: ForeignRefOp) {
        self.pending_ops.push(op);
    }

    /// Absorb a batch of foreign-ref ops from an actor GC.
    pub fn absorb_ops(&mut self, mut ops: Vec<ForeignRefOp>) {
        self.pending_ops.append(&mut ops);
    }

    /// Check if cycle detection should be triggered.
    ///
    /// Delegates to `orca_cycle::CycleDetector::incremental_detect` via
    /// the runtime's `process_gc_ops` pipeline.
    pub fn should_trigger_cycle_detection(&self) -> bool {
        self.delivered_count >= self.cycle_detect_threshold
    }

    /// Reset the delivered-op counter (called after cycle detection runs).
    pub fn reset_delivered_count(&mut self) {
        self.delivered_count = 0;
    }

    /// Reset all global statistics.
    pub fn reset_stats(&mut self) {
        self.stats.reset();
    }
}

impl Default for OrcaCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// SharedHeapGc — retained for backward compatibility
// ---------------------------------------------------------------------------

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc;

    // ------------------------------------------------------------------
    // Mock heap and header for isolated testing
    // ------------------------------------------------------------------

    /// Minimal mock of `ActorHeap` that allocates blocks of
    /// `(OrcaHeader + payload)` and tracks live allocations for leak
    /// detection.
    struct MockHeap {
        /// Pointers to live payload allocations.
        live: Vec<*mut u8>,
        /// Total bytes currently allocated.
        bytes_used: usize,
    }

    impl MockHeap {
        fn new() -> Self {
            MockHeap {
                live: Vec::new(),
                bytes_used: 0,
            }
        }

        fn is_live(&self, payload_ptr: *mut u8) -> bool {
            self.live.contains(&payload_ptr)
        }

        fn live_count(&self) -> usize {
            self.live.len()
        }
    }

    impl OrcaHeap for MockHeap {
        fn alloc_payload(&mut self, payload_size: usize) -> Option<*mut u8> {
            let header_size = std::mem::size_of::<OrcaHeader>();
            let total = header_size.saturating_add(payload_size);
            let layout = alloc::Layout::from_size_align(total, 8).ok()?;

            // SAFETY: layout is non-zero and properly aligned.
            let base = unsafe { alloc::alloc(layout) };
            if base.is_null() {
                return None;
            }

            // Initialise the header so that OrcaGc can operate on it directly.
            // SAFETY: base..base+total is writable and properly aligned.
            unsafe {
                std::ptr::write(
                    base as *mut OrcaHeader,
                    OrcaHeader::new(0, TypeTag::Raw, total, payload_size),
                );
            }

            let payload_ptr = unsafe { base.add(header_size) };
            self.live.push(payload_ptr);
            self.bytes_used += total;
            Some(payload_ptr)
        }

        unsafe fn free_payload(&mut self, payload_ptr: *mut u8) {
            let header_size = std::mem::size_of::<OrcaHeader>();
            let base = payload_ptr.sub(header_size);

            let idx = self.live.iter().position(|&p| p == payload_ptr);
            if let Some(i) = idx {
                self.live.swap_remove(i);
                let header = &*(base as *mut OrcaHeader);
                let total = header_size + header.payload_size;
                self.bytes_used -= total;

                let layout = alloc::Layout::from_size_align_unchecked(total, 8);
                alloc::dealloc(base, layout);
            }
        }

        unsafe fn header_ptr(&self, payload_ptr: *mut u8) -> *mut OrcaHeader {
            let header_size = std::mem::size_of::<OrcaHeader>();
            payload_ptr.sub(header_size) as *mut OrcaHeader
        }
    }

    // ------------------------------------------------------------------
    // Helper: read counts safely
    // ------------------------------------------------------------------

    fn local_count(header: &OrcaHeader) -> u32 {
        header.ref_count
    }

    fn foreign_count(header: &OrcaHeader) -> u32 {
        header.foreign_count
    }

    fn is_sticky(header: &OrcaHeader) -> bool {
        header.sticky
    }

    // ------------------------------------------------------------------
    // Test 1: allocate an object and verify counts
    // ------------------------------------------------------------------

    #[test]
    fn test_alloc_object() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 64, TypeTag::String);
        assert!(ptr.is_some(), "allocation should succeed");

        let ptr = ptr.unwrap();
        assert!(heap.is_live(ptr), "object should be live in heap");

        let header = unsafe { &*heap.header_ptr(ptr) };
        assert_eq!(local_count(header), 1, "local_count should start at 1");
        assert_eq!(foreign_count(header), 0, "foreign_count should start at 0");
        assert!(!is_sticky(header), "sticky should start as false");
        assert_eq!(header.actor_id, 1, "actor_id should match");
        assert_eq!(header.payload_size, 64, "payload_size should match");

        assert_eq!(
            gc.stats.objects_allocated, 1,
            "stats should track allocation"
        );
    }

    // ------------------------------------------------------------------
    // Test 2: multiple local refs, drop them all
    // ------------------------------------------------------------------

    #[test]
    fn test_local_ref_counting() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 32, TypeTag::Tuple).unwrap();
        let header = unsafe { &*heap.header_ptr(ptr) };
        assert_eq!(local_count(header), 1);

        // Create two more local refs.
        unsafe {
            gc.local_ref(&heap, ptr);
        }
        unsafe {
            gc.local_ref(&heap, ptr);
        }
        assert_eq!(local_count(header), 3);
        assert_eq!(gc.stats.local_refs_created, 2);

        // Drop all 3 refs.
        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        assert_eq!(local_count(header), 2);

        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        assert_eq!(local_count(header), 1);

        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        // Object should be freed now.
        assert_eq!(heap.live_count(), 0, "object should be freed");
    }

    // ------------------------------------------------------------------
    // Test 3: alloc + ref + drop → object freed
    // ------------------------------------------------------------------

    #[test]
    fn test_object_freed_when_unrefd() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 48, TypeTag::Array).unwrap();
        assert_eq!(heap.live_count(), 1);

        // Drop the sole local ref.
        let freed = unsafe { gc.drop_local_ref(&mut heap, ptr) };
        assert!(freed, "object should be freed immediately");
        assert_eq!(heap.live_count(), 0);
        assert_eq!(gc.stats.objects_freed, 1);
    }

    // ------------------------------------------------------------------
    // Test 4: simulate sending a reference to another actor
    // ------------------------------------------------------------------

    #[test]
    fn test_foreign_ref_send() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 16, TypeTag::ActorRef).unwrap();
        let header = unsafe { &*heap.header_ptr(ptr) };
        assert_eq!(foreign_count(header), 0);

        // Actor 1 sends a reference to actor 2.
        let op = unsafe { gc.send_ref_to(&heap, ptr, 2) };

        // Foreign count should have been incremented.
        assert_eq!(foreign_count(header), 1);
        assert_eq!(gc.stats.foreign_refs_sent, 1);

        // Verify the op fields.
        assert_eq!(op.target_actor, 2);
        assert_eq!(op.delta, -1);
        assert!(!op.object_header.is_null());

        // Clean up: drop the remaining local ref and process the op.
        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        gc.process_foreign_op(&mut heap, op);
        assert_eq!(heap.live_count(), 0);
    }

    // ------------------------------------------------------------------
    // Test 5: simulate receiving a reference from another actor
    // ------------------------------------------------------------------

    #[test]
    fn test_foreign_ref_receive() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 24, TypeTag::Record).unwrap();
        let header = unsafe { &mut *heap.header_ptr(ptr) };

        // Simulate: foreign actor had a reference, now returns it to us.
        // Start with foreign_count = 1 (the foreign actor held a ref).
        header.foreign_count = 1;

        // We receive the reference back.
        unsafe {
            gc.receive_ref(&heap, ptr);
        }

        // Foreign count should be 0, local count should be 2 (original + received).
        assert_eq!(foreign_count(header), 0);
        assert_eq!(local_count(header), 2);
        assert_eq!(gc.stats.foreign_refs_received, 1);

        // Drop both local refs → object freed.
        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        assert_eq!(heap.live_count(), 1); // still 1 ref left

        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        assert_eq!(heap.live_count(), 0);
    }

    // ------------------------------------------------------------------
    // Test 6: local count 0 but foreign > 0 → object NOT freed
    // ------------------------------------------------------------------

    #[test]
    fn test_object_not_freed_with_foreign_refs() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 32, TypeTag::Map).unwrap();
        let header = unsafe { &mut *heap.header_ptr(ptr) };

        // Simulate a foreign reference existing.
        header.foreign_count = 1;

        // Drop the sole local ref.
        let freed = unsafe { gc.drop_local_ref(&mut heap, ptr) };
        assert!(
            !freed,
            "object should NOT be freed while foreign refs exist"
        );
        assert_eq!(heap.live_count(), 1, "object should still be alive");

        // Object should be in the deferred list.
        assert_eq!(gc.deferred_decrements.len(), 1);
    }

    // ------------------------------------------------------------------
    // Test 7: pinned (sticky) objects are never freed
    // ------------------------------------------------------------------

    #[test]
    fn test_pin_object() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 8, TypeTag::Raw).unwrap();
        let header = unsafe { &*heap.header_ptr(ptr) };
        assert!(!is_sticky(header));

        // Pin the object.
        unsafe {
            gc.pin_object(&heap, ptr);
        }
        assert!(is_sticky(header));

        // Drop the local ref — object should NOT be freed.
        let freed = unsafe { gc.drop_local_ref(&mut heap, ptr) };
        assert!(!freed, "pinned object should not be freed");
        assert_eq!(heap.live_count(), 1);

        // Unpin and try again.
        unsafe {
            gc.unpin_object(&heap, ptr);
        }
        assert!(!is_sticky(header));

        // Now try process_deferred — object should be freed.
        gc.process_deferred(&mut heap);
        assert_eq!(heap.live_count(), 0);
    }

    // ------------------------------------------------------------------
    // Test 8: deferred decrement is resolved when foreign count drops
    // ------------------------------------------------------------------

    #[test]
    fn test_deferred_decrement() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 16, TypeTag::Tuple).unwrap();
        let header = unsafe { &mut *heap.header_ptr(ptr) };

        // Foreign ref exists.
        header.foreign_count = 1;

        // Drop local ref → deferred.
        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        assert_eq!(gc.deferred_decrements.len(), 1);
        assert_eq!(heap.live_count(), 1);

        // Foreign ref goes away (simulated via a ForeignRefOp).
        let op = ForeignRefOp {
            target_actor: 1,
            owner_actor: 1,
            object_header: unsafe { heap.header_ptr(ptr) },
            delta: -1,
        };
        gc.process_foreign_op(&mut heap, op);

        // foreign_count is now 0, local_count is 0, object should be freed.
        assert_eq!(heap.live_count(), 0);
        // process_foreign_op removes the freed object from deferred_decrements.
        assert_eq!(gc.deferred_decrements.len(), 0);

        // Running process_deferred is now a no-op.
        gc.process_deferred(&mut heap);
        assert_eq!(gc.deferred_decrements.len(), 0);
    }

    // ------------------------------------------------------------------
    // Test 9: alloc, 3 local refs, drop 2, still alive
    // ------------------------------------------------------------------

    #[test]
    fn test_multiple_refs() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(42);

        let ptr = gc.alloc_object(&mut heap, 100, TypeTag::Array).unwrap();
        let header = unsafe { &*heap.header_ptr(ptr) };

        // Create 2 additional refs (total 3).
        unsafe {
            gc.local_ref(&heap, ptr);
        }
        unsafe {
            gc.local_ref(&heap, ptr);
        }
        assert_eq!(local_count(header), 3);

        // Drop 2 refs.
        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }

        // Should still be alive with 1 ref.
        assert_eq!(heap.live_count(), 1);
        assert_eq!(local_count(header), 1);

        // Drop the last one.
        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        assert_eq!(heap.live_count(), 0);
    }

    // ------------------------------------------------------------------
    // Test 10: GC stats tracking
    // ------------------------------------------------------------------

    #[test]
    fn test_gc_stats() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        // Allocate some objects.
        let p1 = gc.alloc_object(&mut heap, 10, TypeTag::String).unwrap();
        let p2 = gc.alloc_object(&mut heap, 20, TypeTag::String).unwrap();
        let p3 = gc.alloc_object(&mut heap, 30, TypeTag::String).unwrap();

        assert_eq!(gc.stats.objects_allocated, 3);
        assert_eq!(gc.stats.bytes_allocated, 60);

        // Create and drop refs.
        unsafe {
            gc.local_ref(&heap, p1);
        }
        unsafe {
            gc.local_ref(&heap, p1);
        }
        assert_eq!(gc.stats.local_refs_created, 2);

        unsafe {
            gc.drop_local_ref(&mut heap, p1);
        }
        assert_eq!(gc.stats.local_refs_dropped, 1);

        // Free everything.
        unsafe {
            gc.drop_local_ref(&mut heap, p1);
        }
        unsafe {
            gc.drop_local_ref(&mut heap, p1);
        }
        unsafe {
            gc.drop_local_ref(&mut heap, p2);
        }
        unsafe {
            gc.drop_local_ref(&mut heap, p3);
        }

        assert_eq!(gc.stats.objects_freed, 3);
        assert_eq!(gc.stats.bytes_freed, 60);
    }

    // ------------------------------------------------------------------
    // Test 11: verify ForeignRefOp is created correctly
    // ------------------------------------------------------------------

    #[test]
    fn test_send_ref_op() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(7);

        let ptr = gc.alloc_object(&mut heap, 8, TypeTag::Closure).unwrap();
        let header_ptr = unsafe { heap.header_ptr(ptr) };

        let op = unsafe { gc.send_ref_to(&heap, ptr, 99) };

        assert_eq!(op.target_actor, 99);
        assert_eq!(op.owner_actor, 7, "op should record the owning actor");
        assert_eq!(op.delta, -1);
        assert_eq!(op.object_header, header_ptr);

        // Foreign count should have been incremented.
        let header = unsafe { &*header_ptr };
        assert_eq!(foreign_count(header), 1);

        // Clean up.
        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        gc.process_foreign_op(&mut heap, op);
    }

    // ------------------------------------------------------------------
    // Test 12: verify foreign op delivery (process_foreign_op)
    // ------------------------------------------------------------------

    #[test]
    fn test_process_foreign_op() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 16, TypeTag::Map).unwrap();
        let header = unsafe { &mut *heap.header_ptr(ptr) };

        // Start with foreign_count = 2 (two foreign refs).
        header.foreign_count = 2;

        // Process a -1 op.
        let op1 = ForeignRefOp {
            target_actor: 1,
            owner_actor: 1,
            object_header: unsafe { heap.header_ptr(ptr) },
            delta: -1,
        };
        gc.process_foreign_op(&mut heap, op1);
        assert_eq!(foreign_count(header), 1);
        assert_eq!(heap.live_count(), 1); // local_count still 1

        // Drop the local ref — should be deferred since foreign_count > 0.
        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        assert_eq!(heap.live_count(), 1);

        // Process another -1 op → foreign_count becomes 0, and since local_count is also 0, the object is freed.
        let op2 = ForeignRefOp {
            target_actor: 1,
            owner_actor: 1,
            object_header: unsafe { heap.header_ptr(ptr) },
            delta: -1,
        };
        gc.process_foreign_op(&mut heap, op2);

        // Object should be freed now (both counts 0).
        assert_eq!(heap.live_count(), 0);
    }

    // ------------------------------------------------------------------
    // Test 13: process_deferred frees nothing when foreign refs remain
    // ------------------------------------------------------------------

    #[test]
    fn test_process_deferred_noop() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 8, TypeTag::Raw).unwrap();
        let header = unsafe { &mut *heap.header_ptr(ptr) };
        header.foreign_count = 3;

        unsafe {
            gc.drop_local_ref(&mut heap, ptr);
        }
        assert_eq!(gc.deferred_decrements.len(), 1);

        // process_deferred should not free because foreign_count is still 3.
        gc.process_deferred(&mut heap);
        assert_eq!(heap.live_count(), 1);
        assert_eq!(gc.deferred_decrements.len(), 1);
    }

    // ------------------------------------------------------------------
    // Test 14: coordinator submit + absorb
    // ------------------------------------------------------------------

    #[test]
    fn test_coordinator_queues() {
        let mut coord = OrcaCoordinator::new();
        assert!(coord.pending_ops.is_empty());

        // Create a dummy op (we can't easily create a real one without a
        // heap allocation, so we use a null pointer — this is only safe for
        // queue testing, not for delivery).
        let dummy_header = std::ptr::null_mut();
        let op1 = ForeignRefOp {
            target_actor: 10,
            owner_actor: 1,
            object_header: dummy_header,
            delta: -1,
        };
        coord.submit_op(op1);
        assert_eq!(coord.pending_ops.len(), 1);

        let op2 = ForeignRefOp {
            target_actor: 20,
            owner_actor: 1,
            object_header: dummy_header,
            delta: 1,
        };
        let mut batch = vec![op2];
        coord.absorb_ops(std::mem::take(&mut batch));
        assert_eq!(coord.pending_ops.len(), 2);

        // Threshold.
        assert_eq!(coord.cycle_detect_threshold, 10_000);
        assert!(!coord.should_trigger_cycle_detection());
    }

    // ------------------------------------------------------------------
    // Test 16: stats reset
    // ------------------------------------------------------------------

    #[test]
    fn test_stats_reset() {
        let mut stats = GcStats::default();
        stats.objects_allocated = 5;
        stats.bytes_freed = 100;

        stats.reset();
        assert_eq!(stats.objects_allocated, 0);
        assert_eq!(stats.bytes_freed, 0);
    }

    // ------------------------------------------------------------------
    // Test 17: zero-sized allocation rejected
    // ------------------------------------------------------------------

    #[test]
    fn test_zero_size_alloc_rejected() {
        let mut heap = MockHeap::new();
        let mut gc = OrcaGc::new(1);

        let ptr = gc.alloc_object(&mut heap, 0, TypeTag::Raw);
        assert!(ptr.is_none(), "zero-sized allocation should be rejected");
    }

    // ------------------------------------------------------------------
    // Test 18: foreign_ref_queue drain
    // ------------------------------------------------------------------

    #[test]
    fn test_foreign_ref_queue_drain() {
        let mut gc = OrcaGc::new(1);
        assert!(gc.foreign_ref_queue.is_empty());

        let dummy_header = std::ptr::null_mut();
        gc.queue_foreign_op(ForeignRefOp {
            target_actor: 5,
            owner_actor: 1,
            object_header: dummy_header,
            delta: -1,
        });
        gc.queue_foreign_op(ForeignRefOp {
            target_actor: 6,
            owner_actor: 1,
            object_header: dummy_header,
            delta: 1,
        });

        assert_eq!(gc.foreign_ref_queue.len(), 2);

        let drained = gc.drain_foreign_ops();
        assert_eq!(drained.len(), 2);
        assert!(gc.foreign_ref_queue.is_empty());
    }

    // ------------------------------------------------------------------
    // Test 19: receiver-side hold keeps an owned object alive until the
    // receiver's hold is released (ORCA receiver protocol)
    // ------------------------------------------------------------------

    #[test]
    fn test_receiver_hold_lifecycle() {
        let mut heap = MockHeap::new();
        let mut owner_gc = OrcaGc::new(1);
        let mut receiver_gc = OrcaGc::new(2);

        let ptr = owner_gc.alloc_object(&mut heap, 16, TypeTag::Raw).unwrap();
        let header_ptr = unsafe { heap.header_ptr(ptr) };

        // Receiver takes a hold on the owner's object.
        unsafe {
            owner_gc.inc_foreign_hold(&heap, ptr);
        }
        receiver_gc.record_held_ref(1, header_ptr);

        let header = unsafe { &*header_ptr };
        assert_eq!(foreign_count(header), 1);
        assert_eq!(owner_gc.stats.foreign_refs_received, 1);

        // Owner drops its only local ref: the hold must keep the object
        // alive (deferred, not freed).
        let freed = unsafe { owner_gc.drop_local_ref(&mut heap, ptr) };
        assert!(!freed, "held object must survive the owner's local drop");
        assert_eq!(heap.live_count(), 1);

        // Receiver exits: the runtime drains its holds and applies the -1
        // on the owning side, which frees the object.
        let holds = receiver_gc.take_held_refs();
        assert_eq!(holds.len(), 1);
        assert!(
            receiver_gc.take_held_refs().is_empty(),
            "drain is idempotent"
        );
        for (owner_id, header) in holds {
            assert_eq!(owner_id, 1);
            owner_gc.process_foreign_op(
                &mut heap,
                ForeignRefOp {
                    target_actor: 2,
                    owner_actor: owner_id,
                    object_header: header,
                    delta: -1,
                },
            );
        }
        assert_eq!(
            heap.live_count(),
            0,
            "object freed once the hold is released"
        );
    }
}
