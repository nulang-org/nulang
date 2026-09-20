//! ORCA-compatible per-actor heap allocator for Nulang.
//!
//! Stage A1 — Nulang v0.4 ORCA Garbage Collector
//!
//! This module provides a bump allocator backed by a contiguous memory block,
//! with per-size-class intrusive free lists for fast object reuse. Every
//! allocation carries an [`OrcaHeader`] that stores reference counts, GC
//! colour, type tag, and live-object linked-list pointers.
//!
//! # Design decisions
//!
//! * **Bump allocation** for speed on the fast path (most allocations).
//! * **Chained bump blocks** — when the active bump block is exhausted, a new
//!   block is allocated and chained onto the heap instead of failing.  Objects
//!   never move (raw payload pointers are held in VM registers, foreign refs,
//!   and JIT code), so growth must chain — never realloc/copy — the existing
//!   block.
//! * **Size-class free lists** (Tiny → Huge) so that freed objects can be
//!   reused without touching the bump pointer.
//! * **Large-object space (LOS)** — allocations whose total size exceeds the
//!   largest size class (256 bytes) are individually allocated with the
//!   global allocator instead of consuming the contiguous bump region, and
//!   are reused on an exact-size match from the `Huge` free list.
//! * **Intrusive live list** — every live object is a node in a doubly-linked
//!   list embedded in the header. This makes `iter_live_objects` O(live) and
//!   avoids auxiliary hash maps or bitmaps.
//! * **8-byte alignment** is enforced for every allocation (header + payload).
//! * **Zero `actor_id` default** — the heap is created before the actor ID is
//!   known; callers should invoke `set_actor_id` immediately after creation.

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Required alignment for every allocation (header + payload).
const ALIGN: usize = 8;

/// Number of discrete size classes.
const NUM_SIZE_CLASSES: usize = 5;

/// Total-size threshold (header + aligned payload) above which allocations
/// go to the large-object space instead of the bump region.  Set just above
/// the largest size-class block (256 bytes), so exactly the `Huge` class
/// is served by the LOS.
const LOS_THRESHOLD: usize = 256;

// ---------------------------------------------------------------------------
// SizeClass
// ---------------------------------------------------------------------------

/// Size classification for heap objects.
///
/// Each class represents an upper bound on the *total* allocation size
/// (header + aligned payload). Free lists are bucketed by this class so
/// that reallocation of similarly-sized objects is cache-friendly.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeClass {
    /// Up to 32 bytes total.
    Tiny = 0,
    /// 33–64 bytes total.
    Small = 1,
    /// 65–128 bytes total.
    Medium = 2,
    /// 129–256 bytes total.
    Large = 3,
    /// 257+ bytes total (unbounded).
    Huge = 4,
}

impl SizeClass {}

/// Map a *total* allocation size (header + payload, already aligned) to its
/// size class and the rounded-up block size for free-list bucketing.  The
/// bump allocator reserves the full block size for every allocation, so all
/// blocks in a class are physically uniform and freely interchangeable.
fn classify_total_size(total_size: usize) -> (SizeClass, usize) {
    // Clamp to at least the header size so that even zero-payload
    // allocations have a well-defined class.
    let total_size = total_size.max(std::mem::size_of::<OrcaHeader>());
    match total_size {
        0..=32 => (SizeClass::Tiny, 32),
        33..=64 => (SizeClass::Small, 64),
        65..=128 => (SizeClass::Medium, 128),
        129..=256 => (SizeClass::Large, 256),
        n => (SizeClass::Huge, n),
    }
}

// ---------------------------------------------------------------------------
// GcColor
// ---------------------------------------------------------------------------

/// Tri-colour marker used by the ORCA cycle detector.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcColor {
    /// Object is potentially garbage (not yet visited).
    White = 0,
    /// Object has been discovered but children not yet scanned.
    Gray = 1,
    /// Object and its transitive children are reachable.
    Black = 2,
}

// ---------------------------------------------------------------------------
// TypeTag
// ---------------------------------------------------------------------------

/// Runtime type tag for heap-allocated objects.
///
/// The GC and the debugger use this to interpret payload layout.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTag {
    /// Reference to another actor (contains a `u64` actor id).
    ActorRef = 0,
    /// Dynamically-sized array.
    Array = 1,
    /// UTF-8 string data.
    String = 2,
    /// Record / object with named fields.
    Record = 3,
    /// Function closure (captures environment).
    Closure = 4,
    /// Hash map.
    Map = 5,
    /// Fixed-size tuple.
    Tuple = 6,
    /// Raw untyped data (FFI boundaries).
    Raw = 7,
    /// Remote actor reference (node_id + actor_id).
    RemoteActor = 8,
    /// Zero-copy logical window over a backing Array.
    ///
    /// Payload layout is three Value slots:
    /// [backing_array, absolute_start, logical_len].
    ArrayView = 9,
}

// ---------------------------------------------------------------------------
// OrcaHeader
// ---------------------------------------------------------------------------

/// Header prepended to every heap allocation.
///
/// The header is laid out with `#[repr(C)]` so that the payload pointer
/// returned by [`ActorHeap::alloc`] is always exactly one `OrcaHeader`
/// stride past the header base address. [`ActorHeap::header_of`] recovers
/// the header by walking backward one stride.
///
/// # Memory layout ( verified by `test_header_size` )
///
/// ```text
///  offset | field
///  -------+---------------
///    0    | ref_count      (u32)
///    4    | foreign_count  (u32)
///    8    | sticky         (bool)
///    9    | size_class     (SizeClass   u8)
///   10    | gc_color       (GcColor     u8)
///   11    | type_tag       (TypeTag     u8)
///   12    | _pad           ([u8; 4]) — aligns actor_id to 8 bytes
///   16    | actor_id       (u64)
///   24    | size           (usize — total bytes, header + aligned payload)
///   32    | payload_size   (usize — requested payload bytes)
///   40    | live_next      (*mut OrcaHeader)
///   48    | live_prev      (*mut OrcaHeader)
///  -------+---------------
///   56    | TOTAL
/// ```
///
/// # Thread safety
///
/// The counts are **plain integers, not atomics**.  The runtime is a
/// single-threaded synchronous coordinator: one scheduler thread runs all
/// actor steps, `process_gc_ops`, and cycle detection, and it is the only
/// thread that ever touches a heap.  Network reader threads and LLM worker
/// threads never dereference an `OrcaHeader`.  (The header was never
/// `Send`/`Sync` anyway — `live_next`/`live_prev` are raw pointers.)
#[repr(C)]
pub struct OrcaHeader {
    /// Local reference count — how many references exist *inside* the owning
    /// actor. When this drops to zero the object may be reclaimed.
    pub ref_count: u32,
    /// Foreign reference count — how many references exist in *other* actors.
    /// Part of the ORCA protocol for cross-actor reference tracking.
    pub foreign_count: u32,
    /// When `true` the object is immortal (sticky) and must never be collected.
    /// Used for global constants and pinned FFI buffers.
    pub sticky: bool,
    /// Size class bucket this object belongs to.
    pub size_class: SizeClass,
    /// GC tri-colour state.
    pub gc_color: GcColor,
    /// Runtime type tag — tells the GC how to scan this object's payload.
    pub type_tag: TypeTag,
    /// Padding to ensure `actor_id` (u64) is 8-byte aligned.
    _pad: [u8; 4],
    /// ID of the actor that owns this object.
    pub actor_id: u64,
    /// Total bytes allocated for this object (header + aligned payload).
    pub size: usize,
    /// Requested payload size in bytes (as passed to `alloc`).
    pub payload_size: usize,
    /// Intrusive next pointer for the live-object doubly-linked list.
    /// This is internal to the allocator and not part of the public ORCA spec.
    pub(crate) live_next: *mut OrcaHeader,
    /// Intrusive previous pointer for the live-object doubly-linked list.
    pub(crate) live_prev: *mut OrcaHeader,
}

impl OrcaHeader {
    /// Create a *logically* initialised header on the caller's stack.
    ///
    /// # Safety
    /// The returned value must be copied into heap-backed storage (via
    /// `ptr::write`) before it is observed by any heap/GC code.
    pub(crate) fn new(
        actor_id: u64,
        type_tag: TypeTag,
        total_size: usize,
        payload_size: usize,
    ) -> Self {
        let (size_class, _) = classify_total_size(total_size);
        OrcaHeader {
            ref_count: 1,
            foreign_count: 0,
            sticky: false,
            size_class,
            gc_color: GcColor::White,
            type_tag,
            _pad: [0; 4],
            actor_id,
            size: total_size,
            payload_size,
            live_next: std::ptr::null_mut(),
            live_prev: std::ptr::null_mut(),
        }
    }
}

// ---------------------------------------------------------------------------
// ActorHeap
// ---------------------------------------------------------------------------

/// Per-actor heap allocator with ORCA-compatible object headers.
///
/// `ActorHeap` combines a fast bump allocator with size-class free lists.
/// All allocations are 8-byte aligned and carry an [`OrcaHeader`].  The
/// allocator maintains an intrusive doubly-linked list of *live* objects so
/// that the GC can walk all reachable objects in O(live) time.
///
/// # Thread safety
///
/// `ActorHeap` is **not** `Sync` — it is designed to be owned by a single
/// actor and accessed only while that actor is running.  It **is** `Send`
/// so that an actor (and its heap) can be migrated between scheduler threads.
/// In practice the runtime is a single-threaded synchronous coordinator, so
/// all heap access happens on the one scheduler thread; header refcounts are
/// therefore plain integers, not atomics (see [`OrcaHeader`]).
#[derive(Debug)]
pub struct ActorHeap {
    /// Owning actor ID (0 until `set_actor_id` is called).
    actor_id: u64,
    /// Base pointer of the *active* bump block (the one `current` bumps into).
    base: *mut u8,
    /// Bump pointer — next free byte in the active block.
    current: *mut u8,
    /// One-past-the-end pointer of the active block.
    limit: *mut u8,
    /// Total size of the active block (bytes).
    total_size: usize,
    /// Bytes committed by the bump pointer in the active block
    /// (i.e. `current - base`).
    used_bytes: usize,
    /// Cumulative `used_bytes` of retired (exhausted) bump blocks.
    prior_used: usize,
    /// `(base, size)` of every retired bump block.  Retired blocks are full:
    /// their live objects stay put (objects never move) and their freed
    /// objects sit in the shared size-class free lists.  They are deallocated
    /// on `reset`/`drop`.
    retired_blocks: Vec<(*mut u8, usize)>,
    /// Per-size-class intrusive free lists.
    /// Each entry is either `null_mut()` or points to the payload of the
    /// first free block in that class.  The first 8 bytes of a free payload
    /// store a `*mut u8` to the next free block.  The `Huge` list doubles as
    /// the large-object-space free list.
    free_lists: [*mut u8; NUM_SIZE_CLASSES],
    /// Payload pointers of every large-object-space block ever allocated by
    /// this heap (live or sitting in the `Huge` free list).  Each block is
    /// individually malloc'd and must be individually deallocated on
    /// `reset`/`drop`.
    los_blocks: Vec<*mut u8>,
    /// Head of the live-object doubly-linked list.
    live_head: *mut OrcaHeader,
    /// Tail of the live-object doubly-linked list.
    live_tail: *mut OrcaHeader,
    /// Number of objects currently in the live list.
    live_count: usize,
    /// Cumulative allocations (including reuses from free lists).
    total_allocs: usize,
    /// Cumulative frees.
    total_frees: usize,
    /// High-water mark of `used_bytes`.
    peak_used: usize,
}

// ActorHeap can be sent between scheduler threads because it owns all of
// its memory and no other thread holds pointers into it.
unsafe impl Send for ActorHeap {}

/// Round `size` up to the next multiple of `ALIGN` (8).
#[inline(always)]
const fn align_up(size: usize) -> usize {
    (size + ALIGN - 1) & !(ALIGN - 1)
}

impl ActorHeap {
    /// Size of the ORCA header in bytes.
    pub const HEADER_SIZE: usize = std::mem::size_of::<OrcaHeader>();

    // ------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------

    /// Create a new per-actor heap with the given total backing size.
    ///
    /// The backing memory is allocated with the global allocator and is
    /// 8-byte aligned.  The block is eagerly reserved but lazily committed
    /// (the bump pointer touches pages on demand), and when it fills up the
    /// heap grows by chaining another block — see [`ActorHeap::grow_bump_block`].
    /// The `actor_id` defaults to `0`; the caller should
    /// invoke [`ActorHeap::set_actor_id`] as soon as the real actor ID is
    /// known.
    ///
    /// # Panics
    ///
    /// Panics if `total_size` is zero or the layout is invalid.
    pub fn new(total_size: usize) -> Self {
        assert!(total_size > 0, "ActorHeap size must be > 0");

        // Try the thread-local heap pool before the global allocator.
        let (base, actual_size) = HEAP_POOL
            .with(|pool| pool.borrow_mut().acquire(total_size))
            .unwrap_or_else(|| {
                let layout = std::alloc::Layout::from_size_align(total_size, ALIGN)
                    .expect("invalid ActorHeap layout");
                let base = unsafe { std::alloc::alloc(layout) };
                if base.is_null() {
                    std::alloc::handle_alloc_error(layout);
                }
                (base, total_size)
            });

        ActorHeap {
            actor_id: 0,
            base,
            current: base,
            limit: unsafe { base.add(actual_size) },
            total_size: actual_size,
            used_bytes: 0,
            prior_used: 0,
            retired_blocks: Vec::new(),
            free_lists: [std::ptr::null_mut(); NUM_SIZE_CLASSES],
            los_blocks: Vec::new(),
            live_head: std::ptr::null_mut(),
            live_tail: std::ptr::null_mut(),
            live_count: 0,
            total_allocs: 0,
            total_frees: 0,
            peak_used: 0,
        }
    }

    /// Set the owning actor ID.
    ///
    /// All subsequently allocated objects will have this `actor_id` written
    /// into their header.  Existing objects are **not** updated.
    pub fn set_actor_id(&mut self, id: u64) {
        self.actor_id = id;
    }

    // ------------------------------------------------------------------
    // Allocation
    // ------------------------------------------------------------------

    /// Allocate an object with the given payload size and type tag.
    ///
    /// Returns a pointer to the **payload** (the writable region just past
    /// the [`OrcaHeader`]).  The header is automatically prepended and
    /// initialised with:
    ///
    /// * `ref_count = 1`
    /// * `foreign_count = 0`
    /// * `sticky = false`
    /// * `gc_color = White`
    /// * `actor_id` = the heap's current actor ID
    /// * `size_class` computed from the total allocation size
    ///
    /// # Algorithm
    ///
    /// 1. Align `payload_size` to 8 bytes.
    /// 2. Compute `total_size = HEADER_SIZE + aligned_payload`.
    /// 3. Determine the size class.
    /// 4. Check the corresponding free list — if a block is available, pop
    ///    it, rewrite the header fields, and return the payload pointer.
    ///    Large-object-space (`Huge`) blocks are only reused on an exact
    ///    size match, because each LOS block is individually deallocated
    ///    with its original layout on `reset`/`drop`.
    /// 5. Otherwise fall back to bump allocation from the active block —
    ///    chaining a fresh block when the active one is exhausted — or to a
    ///    fresh LOS allocation when `total_size > LOS_THRESHOLD`.  The bump
    ///    pointer advances by the uniform class block size (not the exact
    ///    total), so all blocks in a class are physically identical and
    ///    free-list reuse can never overrun a neighbour.
    ///
    /// Returns `None` only if the global allocator fails (OS OOM); an
    /// exhausted bump block triggers growth instead of failure.
    pub fn alloc(&mut self, payload_size: usize, type_tag: TypeTag) -> Option<*mut u8> {
        let aligned_payload = align_up(payload_size);
        let total_size = Self::HEADER_SIZE + aligned_payload;
        // `block_size` is the uniform physical size reserved for the class
        // (the exact total for Huge/LOS blocks).  The header still records
        // the exact `total_size`, because the VM derives array / record /
        // tuple element counts from it.  Every reachable class block is
        // >= HEADER_SIZE + ALIGN (the smallest is 64 bytes vs a 56-byte
        // header), so even a zero-byte payload leaves room for `free`'s
        // intrusive next-pointer.
        let (size_class, block_size) = classify_total_size(total_size);
        let sc_idx = size_class as usize;

        // --- Fast path: try the free list for this size class ---
        let reuse = unsafe {
            if size_class == SizeClass::Huge {
                // LOS blocks carry their original total size in the header;
                // only an exact match keeps reset/drop deallocation correct.
                self.take_los_block(total_size)
            } else if !self.free_lists[sc_idx].is_null() {
                // Pop the first block from the intrusive list.
                let payload_ptr = self.free_lists[sc_idx];
                // The first 8 bytes of the free payload hold the next pointer.
                let next_free = *(payload_ptr as *mut *mut u8);
                self.free_lists[sc_idx] = next_free;
                Some(payload_ptr)
            } else {
                None
            }
        };

        if let Some(payload_ptr) = reuse {
            unsafe {
                // Rewrite header (the old header values are stale).
                let header_ptr = Self::header_of(payload_ptr);
                // SAFETY: payload_ptr came from a previous alloc on this
                // heap, so header_ptr points to a valid OrcaHeader inside
                // our backing block or LOS region.
                std::ptr::write(
                    header_ptr,
                    OrcaHeader::new(self.actor_id, type_tag, total_size, payload_size),
                );

                self.add_to_live_list(header_ptr);
            }
            self.live_count += 1;
            self.total_allocs += 1;
            return Some(payload_ptr);
        }

        // --- Large-object space: individually malloc'd block ---
        if total_size > LOS_THRESHOLD {
            return self.alloc_los(total_size, payload_size, type_tag);
        }

        // --- Slow path: bump allocation, chaining a new block on exhaustion ---
        unsafe {
            if self.current.add(block_size) > self.limit {
                // The active block is full.  Objects never move (raw payload
                // pointers live in VM registers, foreign refs, and JIT code),
                // so we chain a fresh block instead of reallocating.
                self.grow_bump_block(block_size)?;
                debug_assert!(self.current.add(block_size) <= self.limit);
            }

            let new_current = self.current.add(block_size);
            let header_ptr = self.current as *mut OrcaHeader;
            let payload_ptr = self.current.add(Self::HEADER_SIZE);
            self.current = new_current;
            self.used_bytes += block_size;

            // Initialise the header in place.
            std::ptr::write(
                header_ptr,
                OrcaHeader::new(self.actor_id, type_tag, total_size, payload_size),
            );

            self.add_to_live_list(header_ptr);
            self.live_count += 1;
            self.total_allocs += 1;

            let committed = self.prior_used + self.used_bytes;
            if committed > self.peak_used {
                self.peak_used = committed;
            }

            Some(payload_ptr)
        }
    }

    // ------------------------------------------------------------------
    // Free
    // ------------------------------------------------------------------

    /// Free an object back to the free list for its size class.
    ///
    /// `payload_ptr` must be a pointer previously returned by [`alloc`].
    /// The object is removed from the live list and its payload memory is
    /// repurposed as an intrusive linked-list node.  Large-object-space
    /// (`Huge`) blocks go onto the same intrusive list (they stay tracked in
    /// `los_blocks` and are deallocated on `reset`/`drop`, not here).
    ///
    /// # Safety
    ///
    /// * `payload_ptr` must be valid and must have come from `alloc` on this
    ///   exact heap.
    /// * No references to this object may remain — violating this is UB.
    /// * This method must not be called twice on the same pointer (double-free).
    pub unsafe fn free(&mut self, payload_ptr: *mut u8) {
        // Recover the header.
        let header_ptr = Self::header_of(payload_ptr);

        // Remove from the live list.
        self.remove_from_live_list(header_ptr);
        self.live_count -= 1;
        self.total_frees += 1;

        // Read the size class so we know which free list to push to.
        // Plain read: the single scheduler thread is the only mutator of
        // any header (ActorHeap is !Sync).
        let sc = (*header_ptr).size_class;
        let sc_idx = sc as usize;

        // classify_total_size only ever produces valid discriminants, so an
        // out-of-range class here means header corruption; catch it in
        // debug builds instead of silently leaking the block.
        debug_assert!(
            sc_idx < NUM_SIZE_CLASSES,
            "free: header size class {sc_idx} out of range (corrupt header?)"
        );
        if sc_idx < NUM_SIZE_CLASSES {
            // Intrusive free list: the first 8 bytes of the (now dead) payload
            // store a pointer to the previous head of the free list.
            //
            // SAFETY: every block physically reserves at least ALIGN bytes of
            // payload — alloc advances the bump pointer by the uniform class
            // block size, and the smallest class block is 64 bytes against a
            // 56-byte header — so there is always room for the next-pointer,
            // even for zero-byte payloads.
            *(payload_ptr as *mut *mut u8) = self.free_lists[sc_idx];
            self.free_lists[sc_idx] = payload_ptr;
        }
        // If the size class is somehow out of range we simply leak the block.
        // This should never happen because classify_total_size only returns
        // valid discriminants.
    }

    // ------------------------------------------------------------------
    // Header recovery
    // ------------------------------------------------------------------

    /// Recover the [`OrcaHeader`] pointer from a payload pointer.
    ///
    /// # Safety
    ///
    /// `payload_ptr` must point to the payload region of a valid allocation
    /// on this heap (i.e. it must have been returned by `alloc`).  The
    /// header is located exactly `HEADER_SIZE` bytes before the payload
    /// because of the `#[repr(C)]` layout.
    pub unsafe fn header_of(payload_ptr: *mut u8) -> *mut OrcaHeader {
        // Cast to *mut OrcaHeader and offset by -1.  Because OrcaHeader is
        // 56 bytes, this subtracts 56 bytes from the address, landing exactly
        // on the header that was laid out immediately before the payload.
        (payload_ptr as *mut OrcaHeader).offset(-1)
    }

    // ------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------

    /// Total bytes committed by the bump allocator across all chained blocks.
    pub fn used(&self) -> usize {
        self.prior_used + self.used_bytes
    }

    /// Remaining free space in the active bump block.
    pub fn free_bytes(&self) -> usize {
        self.total_size - self.used_bytes
    }

    /// Number of objects currently alive (allocated but not freed).
    pub fn live_count(&self) -> usize {
        self.live_count
    }

    /// Total number of objects sitting in free lists (available for reuse).
    pub fn free_list_count(&self) -> usize {
        let mut count = 0usize;
        for sc_idx in 0..NUM_SIZE_CLASSES {
            let mut cursor = self.free_lists[sc_idx];
            while !cursor.is_null() {
                count += 1;
                // SAFETY: cursor is a payload pointer from a previous free()
                // on this heap. The first 8 bytes hold the next pointer.
                unsafe {
                    cursor = *(cursor as *mut *mut u8);
                }
            }
        }
        count
    }

    // ------------------------------------------------------------------
    // Iteration
    // ------------------------------------------------------------------

    /// Iterate over all live objects.
    ///
    /// Calls `callback` with `(header_ptr, payload_ptr, payload_size)` for
    /// every object that is currently in the live list (i.e. allocated and
    /// not yet freed).  The order follows the allocation order because the
    /// live list is append-only.
    ///
    /// # Usage in GC mark phase
    ///
    /// ```ignore
    /// heap.iter_live_objects(|header, payload, size| {
    ///     unsafe {
    ///         (*header).gc_color = GcColor::Gray;
    ///         // enqueue payload for scanning ...
    ///     }
    /// });
    /// ```
    pub fn iter_live_objects<F>(&self, mut callback: F)
    where
        F: FnMut(*mut OrcaHeader, *mut u8, usize),
    {
        let mut current = self.live_head;
        while !current.is_null() {
            unsafe {
                // Payload starts one header stride past the header.
                let payload_ptr = current.add(1) as *mut u8;
                let payload_size = (*current).size - Self::HEADER_SIZE;
                callback(current, payload_ptr, payload_size);
                current = (*current).live_next;
            }
        }
    }

    // ------------------------------------------------------------------
    // Reset
    // ------------------------------------------------------------------

    /// Reset the heap to a pristine state.
    ///
    /// * The bump pointer returns to `base` of the active block.
    /// * All retired (chained) blocks and large-object-space blocks are
    ///   deallocated.
    /// * All free lists are discarded.
    /// * The live-object list is cleared.
    /// * Statistics are zeroed.
    ///
    /// This is used when an actor is restarted by a supervisor.  Any
    /// outstanding pointers into this heap become dangling — the caller
    /// must ensure none exist.
    pub fn reset(&mut self) {
        // Deallocate retired bump blocks and large-object-space blocks before
        // discarding the free lists — the free lists point into those blocks.
        self.release_retired_blocks();
        self.release_los_blocks();
        self.current = self.base;
        self.used_bytes = 0;
        self.prior_used = 0;
        self.live_head = std::ptr::null_mut();
        self.live_tail = std::ptr::null_mut();
        self.live_count = 0;
        self.free_lists = [std::ptr::null_mut(); NUM_SIZE_CLASSES];
        self.total_allocs = 0;
        self.total_frees = 0;
        self.peak_used = 0;
    }

    // ==================================================================
    // Internal helpers
    // ==================================================================

    /// Chain a fresh bump block onto the heap and make it the active block.
    ///
    /// The exhausted block is retired — kept mapped, not copied — because its
    /// live objects must never move (raw payload pointers are held in VM
    /// registers, other actors' foreign refs, and JIT code).  Retired blocks
    /// are deallocated on `reset`/`drop`.
    ///
    /// The new block is the same size as the exhausted one, or `min_capacity`
    /// when a single allocation needs more.  Equal-size chaining (rather than
    /// doubling) keeps per-actor memory growth linear and predictable: an
    /// actor's footprint stays proportional to the data it actually holds.
    ///
    /// Returns `None` only when the global allocator fails.
    fn grow_bump_block(&mut self, min_capacity: usize) -> Option<()> {
        let new_size = self.total_size.max(min_capacity);
        let layout = std::alloc::Layout::from_size_align(new_size, ALIGN).ok()?;
        // SAFETY: layout has non-zero size (`total_size` > 0) and is valid.
        let base = unsafe { std::alloc::alloc(layout) };
        if base.is_null() {
            // Report OS OOM as exhaustion, matching alloc's `None` contract.
            return None;
        }

        // Retire the exhausted block; its contents stay exactly where they are.
        self.retired_blocks.push((self.base, self.total_size));
        self.prior_used += self.used_bytes;

        self.base = base;
        self.current = base;
        self.limit = unsafe { base.add(new_size) };
        self.total_size = new_size;
        self.used_bytes = 0;
        Some(())
    }

    /// Deallocate every retired (exhausted) bump block.
    ///
    /// Called from `reset` and `drop`.  Any live objects in these blocks are
    /// abandoned — the caller must ensure no outstanding pointers exist.
    fn release_retired_blocks(&mut self) {
        for &(base, size) in &self.retired_blocks {
            // Return to the thread-local pool instead of deallocating.
            HEAP_POOL.with(|pool| {
                pool.borrow_mut().release(base, size);
            });
        }
        self.retired_blocks.clear();
    }

    /// Allocate a large-object-space block with the global allocator.
    ///
    /// The block is tracked in `los_blocks` so it can be individually
    /// deallocated on `reset`/`drop`.  `total_size` must already include the
    /// header and be 8-byte aligned.
    fn alloc_los(
        &mut self,
        total_size: usize,
        payload_size: usize,
        type_tag: TypeTag,
    ) -> Option<*mut u8> {
        let layout = std::alloc::Layout::from_size_align(total_size, ALIGN).ok()?;
        // SAFETY: layout is non-zero (total_size >= HEADER_SIZE) and valid.
        let base = unsafe { std::alloc::alloc(layout) };
        if base.is_null() {
            // Report OOM as exhaustion, matching alloc's `None` contract.
            return None;
        }

        let header_ptr = base as *mut OrcaHeader;
        // SAFETY: `base` points to a fresh block of at least HEADER_SIZE +
        // aligned payload bytes, so the payload region is in bounds.
        let payload_ptr = unsafe { base.add(Self::HEADER_SIZE) };

        // SAFETY: header_ptr points to writable memory we just allocated.
        unsafe {
            std::ptr::write(
                header_ptr,
                OrcaHeader::new(self.actor_id, type_tag, total_size, payload_size),
            );
            self.add_to_live_list(header_ptr);
        }
        self.los_blocks.push(payload_ptr);
        self.live_count += 1;
        self.total_allocs += 1;
        Some(payload_ptr)
    }

    /// Pop a large-object-space block whose total size exactly matches
    /// `total_size` from the `Huge` free list.
    ///
    /// Exact-size matching is required because each LOS block is deallocated
    /// with its original layout; reusing a larger block for a smaller request
    /// would lose the true allocation size.
    ///
    /// # Safety
    /// The `Huge` free list must contain only payload pointers from previous
    /// `free` calls on this heap.
    unsafe fn take_los_block(&mut self, total_size: usize) -> Option<*mut u8> {
        let huge_idx = SizeClass::Huge as usize;
        let mut prev: *mut u8 = std::ptr::null_mut();
        let mut cursor = self.free_lists[huge_idx];
        while !cursor.is_null() {
            // SAFETY: cursor is a payload pointer from a previous free() on
            // this heap. The first 8 bytes hold the next pointer and the
            // header one stride back holds the block's original total size.
            let next = *(cursor as *mut *mut u8);
            let header = Self::header_of(cursor);
            if (*header).size == total_size {
                if prev.is_null() {
                    self.free_lists[huge_idx] = next;
                } else {
                    *(prev as *mut *mut u8) = next;
                }
                return Some(cursor);
            }
            prev = cursor;
            cursor = next;
        }
        None
    }

    /// Deallocate every tracked large-object-space block.
    ///
    /// Called from `reset` and `drop`.  The `Huge` free list must already
    /// have been discarded (or be discarded immediately after) so no stale
    /// pointers into the released blocks survive.
    fn release_los_blocks(&mut self) {
        for &payload_ptr in &self.los_blocks {
            // SAFETY: every pointer in `los_blocks` was returned by
            // `alloc_los` on this heap and is deallocated exactly once here;
            // the header one stride back records the original total size
            // used for the matching `Layout`.
            unsafe {
                let header_ptr = Self::header_of(payload_ptr);
                let layout = std::alloc::Layout::from_size_align((*header_ptr).size, ALIGN)
                    .expect("invalid LOS layout");
                std::alloc::dealloc(header_ptr as *mut u8, layout);
            }
        }
        self.los_blocks.clear();
    }

    /// Append `header_ptr` to the tail of the live-object doubly-linked list.
    ///
    /// # Safety
    /// `header_ptr` must point to a valid, writable `OrcaHeader` inside this
    /// heap's backing block.  This function is called only from `alloc`.
    unsafe fn add_to_live_list(&mut self, header_ptr: *mut OrcaHeader) {
        (*header_ptr).live_next = std::ptr::null_mut();
        (*header_ptr).live_prev = self.live_tail;

        if self.live_tail.is_null() {
            // First object.
            self.live_head = header_ptr;
        } else {
            (*self.live_tail).live_next = header_ptr;
        }
        self.live_tail = header_ptr;
    }

    /// Remove `header_ptr` from the live-object doubly-linked list.
    ///
    /// # Safety
    /// `header_ptr` must be a current member of the live list.
    unsafe fn remove_from_live_list(&mut self, header_ptr: *mut OrcaHeader) {
        let prev = (*header_ptr).live_prev;
        let next = (*header_ptr).live_next;

        if prev.is_null() {
            self.live_head = next;
        } else {
            (*prev).live_next = next;
        }

        if next.is_null() {
            self.live_tail = prev;
        } else {
            (*next).live_prev = prev;
        }
    }
}

// ---------------------------------------------------------------------------
// OrcaHeap trait implementation
// ---------------------------------------------------------------------------

use super::gc::OrcaHeap;

impl OrcaHeap for ActorHeap {
    /// Allocate payload bytes with the Raw type tag.
    ///
    /// Delegates to [`ActorHeap::alloc`] using [`TypeTag::Raw`] as the
    /// default type tag.  The ORCA GC (in `gc.rs`) calls this when it needs
    /// to allocate an object whose type will be determined later.
    fn alloc_payload(&mut self, payload_size: usize) -> Option<*mut u8> {
        self.alloc(payload_size, TypeTag::Raw)
    }

    /// Free a payload previously returned by [`alloc_payload`].
    ///
    /// Delegates directly to [`ActorHeap::free`].
    ///
    /// # Safety
    /// `payload_ptr` must be a live pointer returned by `alloc_payload` on
    /// this exact heap.
    unsafe fn free_payload(&mut self, payload_ptr: *mut u8) {
        self.free(payload_ptr);
    }

    /// Recover the [`OrcaHeader`] pointer from a payload pointer.
    ///
    /// # Safety
    /// `payload_ptr` must be a valid payload pointer from this heap.
    unsafe fn header_ptr(&self, payload_ptr: *mut u8) -> *mut OrcaHeader {
        ActorHeap::header_of(payload_ptr)
    }
}

// ---------------------------------------------------------------------------
// HeapPool — recycled bump-allocator blocks across actor generations
// ---------------------------------------------------------------------------

/// A pool of deallocated bump-allocator blocks that can be reused for new
/// actors.  When an actor exits and its heap is below the pooling threshold,
/// the blocks are returned here instead of being freed.  The next actor
/// spawn draws from the pool before calling the global allocator.
///
/// This is safe because:
/// - Blocks are only returned to the pool after the owning actor has exited,
///   all live objects have been reclaimed, and no foreign references remain.
/// - The runtime is single-threaded per shard, so no synchronization is needed.
pub struct HeapPool {
    /// Recycled blocks: `(base_ptr, size_bytes)`.
    blocks: Vec<(*mut u8, usize)>,
    /// Maximum number of blocks to hold.  When exceeded, the oldest block is
    /// deallocated.
    max_blocks: usize,
    /// Only pool blocks whose size is ≤ this threshold.  Oversized heaps
    /// (actors that grew via chaining) are deallocated directly.
    size_threshold: usize,
}

impl HeapPool {
    /// Create a new pool.
    ///
    /// `max_blocks`: maximum number of recycled blocks to retain.
    /// `size_threshold`: only blocks ≤ this size (bytes) are pooled.
    pub fn new(max_blocks: usize, size_threshold: usize) -> Self {
        HeapPool {
            blocks: Vec::new(),
            max_blocks,
            size_threshold,
        }
    }

    /// Try to acquire a recycled block of at least `min_size` bytes.
    /// Returns `None` when the pool is empty or no block is large enough.
    pub fn acquire(&mut self, min_size: usize) -> Option<(*mut u8, usize)> {
        // Best-fit: find the smallest block that satisfies the request.
        let mut best_idx = None;
        let mut best_size = usize::MAX;
        for (i, &(_base, size)) in self.blocks.iter().enumerate() {
            if size >= min_size && size < best_size {
                best_idx = Some(i);
                best_size = size;
            }
        }
        best_idx.map(|i| self.blocks.swap_remove(i))
    }

    /// Return a block to the pool for reuse.
    ///
    /// The block will only be retained if its size is ≤ `size_threshold`.
    /// Blocks above the threshold, and excess blocks when `max_blocks` is
    /// exceeded, are deallocated immediately.
    fn release(&mut self, base: *mut u8, size: usize) {
        if size > self.size_threshold {
            // Oversized block: deallocate immediately.
            let layout = std::alloc::Layout::from_size_align(size, ALIGN)
                .expect("invalid pooled block layout");
            unsafe {
                std::alloc::dealloc(base, layout);
            }
            return;
        }
        self.blocks.push((base, size));
        // Evict oldest block if over limit.
        while self.blocks.len() > self.max_blocks {
            let (base, size) = self.blocks.remove(0);
            let layout = std::alloc::Layout::from_size_align(size, ALIGN)
                .expect("invalid pooled block layout");
            unsafe {
                std::alloc::dealloc(base, layout);
            }
        }
    }

    /// Number of blocks currently in the pool.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

impl Drop for HeapPool {
    fn drop(&mut self) {
        for (base, size) in self.blocks.drain(..) {
            let layout = std::alloc::Layout::from_size_align(size, ALIGN)
                .expect("invalid pooled block layout");
            unsafe {
                std::alloc::dealloc(base, layout);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Thread-local heap pool — recycled across actor generations
// ---------------------------------------------------------------------------

// Per-shard-thread pool of deallocated bump-allocator blocks.
// Since each shard runs on exactly one scheduler thread and heaps never
// cross threads, a `thread_local!` is safe and avoids plumbing a pool
// reference through every `ActorHeap::new` call site.
//
// Default: retain up to 8 blocks of ≤ 128 KiB each.
thread_local! {
    static HEAP_POOL: std::cell::RefCell<HeapPool> =
        std::cell::RefCell::new(HeapPool::new(8, 128 * 1024));
}
// ---------------------------------------------------------------------------
// Drop
// ---------------------------------------------------------------------------

impl Drop for ActorHeap {
    fn drop(&mut self) {
        // Deallocate individually malloc'd large-object-space blocks.
        self.release_los_blocks();
        // Return retired bump blocks to the pool.
        self.release_retired_blocks();
        // Return the primary bump block to the pool.
        HEAP_POOL.with(|pool| {
            pool.borrow_mut().release(self.base, self.total_size);
        });
    }
}

// =============================================================================
// Unit Tests
// =============================================================================

#[test]
fn test_alloc_and_write() {
    let mut heap = ActorHeap::new(64 * 1024);
    heap.set_actor_id(42);

    let payload = heap.alloc(16, TypeTag::String).expect("alloc failed");
    assert!(!payload.is_null());

    // SAFETY: payload is a valid, unique allocation on this heap.
    unsafe {
        let data = std::slice::from_raw_parts_mut(payload, 16);
        data.copy_from_slice(b"Hello, Nulang!!!");
        assert_eq!(&data[..6], b"Hello,");
    }
}

#[test]
fn test_alloc_multiple() {
    let mut heap = ActorHeap::new(64 * 1024);
    let mut ptrs = Vec::new();

    for i in 0..10 {
        let p = heap.alloc(32, TypeTag::Tuple).expect("alloc failed");
        ptrs.push(p);
        // Write a marker so we can distinguish objects.
        unsafe {
            *(p as *mut u64) = i as u64;
        }
    }

    // Distinct pointers.
    let mut sorted = ptrs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ptrs.len());

    // Markers intact.
    for (i, p) in ptrs.iter().enumerate() {
        unsafe {
            assert_eq!(*(*p as *mut u64), i as u64);
        }
    }
}

#[test]
fn test_free_and_reuse() {
    let mut heap = ActorHeap::new(64 * 1024);

    // Allocate and remember the payload pointer.
    let p1 = heap.alloc(64, TypeTag::Record).unwrap();

    // Free it — the block should go to the appropriate free list.
    unsafe {
        heap.free(p1);
    }

    // Allocate again with the same size class. We should get the same
    // payload pointer back because the free list is LIFO.
    let p2 = heap.alloc(64, TypeTag::Record).unwrap();
    assert_eq!(p1, p2, "freed block should be reused");
}

#[test]
fn test_size_classes() {
    let mut heap = ActorHeap::new(64 * 1024);

    // Payload sizes that result in different total-size buckets.
    // HEADER_SIZE = 48, ALIGN = 8.
    // total = 48 + align_up(payload)
    let cases: Vec<(usize, SizeClass)> = vec![
        (1, SizeClass::Small),   // total = 64 → Small (33-64)
        (16, SizeClass::Medium), // total = 72 → Medium (65-128)
        (17, SizeClass::Medium), // total = 80 → Medium
        (80, SizeClass::Large),  // total = 136 → Large (129-256)
        (81, SizeClass::Large),  // total = 144 → Large
        (200, SizeClass::Large), // total = 256 → Large
        (210, SizeClass::Huge),  // total = 272 → Huge (257+)
    ];

    for (payload_size, expected_class) in cases {
        let p = heap.alloc(payload_size, TypeTag::Raw).unwrap();
        unsafe {
            let header = ActorHeap::header_of(p);
            assert_eq!(
                (*header).size_class,
                expected_class,
                "payload_size={} got {:?}, expected {:?}",
                payload_size,
                (*header).size_class,
                expected_class
            );
        }
    }
}

#[test]
fn test_header_integrity() {
    let mut heap = ActorHeap::new(64 * 1024);
    heap.set_actor_id(99);

    let p = heap.alloc(42, TypeTag::Closure).unwrap();
    unsafe {
        let h = &*ActorHeap::header_of(p);
        assert_eq!(h.ref_count, 1);
        assert_eq!(h.foreign_count, 0);
        assert_eq!(h.sticky, false);
        assert_eq!(h.actor_id, 99);
        assert_eq!(h.type_tag, TypeTag::Closure);
        assert_eq!(h.gc_color, GcColor::White);
        // Total size = HEADER_SIZE + align_up(42) = 48 + 48 = 96
        assert_eq!(h.size, ActorHeap::HEADER_SIZE + 48);
    }
}

#[test]
fn test_header_of_roundtrip() {
    let mut heap = ActorHeap::new(64 * 1024);
    let p = heap.alloc(100, TypeTag::Array).unwrap();

    unsafe {
        let h = ActorHeap::header_of(p);
        // The payload pointer from the header.
        let p2 = h.add(1) as *mut u8;
        assert_eq!(p, p2, "header_of roundtrip failed");
    }
}

#[test]
fn test_free_list_reuse() {
    let mut heap = ActorHeap::new(64 * 1024);

    // Allocate several objects.
    let a = heap.alloc(64, TypeTag::Tuple).unwrap();
    let b = heap.alloc(64, TypeTag::Tuple).unwrap();
    let c = heap.alloc(64, TypeTag::Tuple).unwrap();

    // Free in reverse order.
    unsafe {
        heap.free(c);
        heap.free(b);
        heap.free(a);
    }

    // All three should go back to the free list.
    assert_eq!(heap.free_list_count(), 3);

    // Re-allocate — they should come back in LIFO order.
    // Free order was c,b,a (a was freed last), so a is popped first.
    let r1 = heap.alloc(64, TypeTag::Tuple).unwrap();
    let r2 = heap.alloc(64, TypeTag::Tuple).unwrap();
    let r3 = heap.alloc(64, TypeTag::Tuple).unwrap();

    assert_eq!(r1, a, "last-freed object should be reused first (LIFO)");
    assert_eq!(r2, b);
    assert_eq!(r3, c);

    // Free list should be empty now.
    assert_eq!(heap.free_list_count(), 0);
}

#[test]
fn test_tiny_heap_grows_on_exhaustion() {
    // A tiny heap: the first block fits exactly one 56-byte header + 8-byte
    // payload.  Instead of failing, the second allocation must chain a new
    // block and succeed.
    let mut heap = ActorHeap::new(64);

    let p1 = heap.alloc(8, TypeTag::Raw);
    assert!(p1.is_some());

    let p2 = heap.alloc(8, TypeTag::Raw);
    assert!(p2.is_some(), "heap should grow by chaining a new block");
    assert_eq!(heap.retired_blocks.len(), 1);
    // The first object stays put: growth never moves existing allocations.
    assert_eq!(heap.live_count(), 2);
}

#[test]
fn test_heap_grows_beyond_initial_block() {
    let mut heap = ActorHeap::new(64 * 1024);

    // payload 64 → total 128 bytes; 64KB holds ~512, so 1200 allocations
    // chain two extra blocks (≈150KB committed).
    let mut ptrs = Vec::new();
    for i in 0..1200usize {
        let p = heap
            .alloc(64, TypeTag::Raw)
            .expect("alloc past 64KB failed");
        unsafe {
            *(p as *mut u64) = i as u64;
        }
        ptrs.push(p);
    }

    assert!(
        heap.retired_blocks.len() >= 2,
        "heap should have chained blocks"
    );
    assert!(
        heap.used() > 64 * 1024,
        "used() is cumulative across blocks"
    );
    assert_eq!(heap.live_count(), 1200);

    // Objects never move: every marker written before growth is intact.
    for (i, p) in ptrs.iter().enumerate() {
        unsafe {
            assert_eq!(*(*p as *mut u64), i as u64);
        }
    }
}

#[test]
fn test_live_count_spans_chained_blocks() {
    let mut heap = ActorHeap::new(64 * 1024);

    let mut ptrs = Vec::new();
    for _ in 0..1200 {
        ptrs.push(heap.alloc(64, TypeTag::Raw).unwrap());
    }
    assert!(heap.retired_blocks.len() >= 2);
    assert_eq!(heap.live_count(), 1200);

    // iter_live_objects walks the intrusive live list, which spans blocks.
    let mut seen = 0;
    heap.iter_live_objects(|_header, _payload, _size| {
        seen += 1;
    });
    assert_eq!(seen, 1200);

    // Free every 12th object (spans all blocks) and re-check.
    for &p in ptrs.iter().step_by(12) {
        unsafe {
            heap.free(p);
        }
    }
    assert_eq!(heap.live_count(), 1100);
    let mut seen = 0;
    heap.iter_live_objects(|_header, _payload, _size| {
        seen += 1;
    });
    assert_eq!(seen, 1100);
}

#[test]
fn test_free_list_reuse_after_growth() {
    let mut heap = ActorHeap::new(64 * 1024);

    // Allocate into the second chained block.
    let mut ptrs = Vec::new();
    for _ in 0..600 {
        ptrs.push(heap.alloc(64, TypeTag::Raw).unwrap());
    }
    assert!(!heap.retired_blocks.is_empty());

    // Free one object from the first block and one from the active block.
    let p_early = ptrs[0];
    let p_late = *ptrs.last().unwrap();
    unsafe {
        heap.free(p_early);
        heap.free(p_late);
    }
    assert_eq!(heap.free_list_count(), 2);

    // The shared free list is LIFO across blocks: last freed is reused first.
    let r1 = heap.alloc(64, TypeTag::Raw).unwrap();
    let r2 = heap.alloc(64, TypeTag::Raw).unwrap();
    assert_eq!(r1, p_late, "free list must reach across chained blocks");
    assert_eq!(r2, p_early);
    assert_eq!(heap.free_list_count(), 0);
}

#[test]
fn test_reset_after_growth() {
    let mut heap = ActorHeap::new(64 * 1024);

    for _ in 0..1200 {
        heap.alloc(64, TypeTag::Raw).unwrap();
    }
    assert!(heap.retired_blocks.len() >= 2);

    heap.reset();
    assert_eq!(heap.live_count(), 0);
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.free_list_count(), 0);
    assert!(
        heap.retired_blocks.is_empty(),
        "reset must release chained blocks"
    );

    // The heap is fully functional after reset, including growing again.
    let mut ptrs = Vec::new();
    for _ in 0..1200 {
        ptrs.push(
            heap.alloc(64, TypeTag::Raw)
                .expect("alloc after reset failed"),
        );
    }
    assert_eq!(heap.live_count(), 1200);
    assert!(heap.retired_blocks.len() >= 2);
}

#[test]
fn test_reset() {
    let mut heap = ActorHeap::new(64 * 1024);

    heap.alloc(100, TypeTag::Map).unwrap();
    heap.alloc(200, TypeTag::Record).unwrap();
    assert_eq!(heap.live_count(), 2);
    assert!(heap.used() > 0);

    // Free one object so it goes into the free list.
    let p = heap.alloc(64, TypeTag::String).unwrap();
    unsafe {
        heap.free(p);
    }
    assert!(heap.free_list_count() > 0);

    // Reset everything.
    heap.reset();

    assert_eq!(heap.live_count(), 0);
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.free_list_count(), 0);
    assert_eq!(heap.free_bytes(), 64 * 1024);

    // Should be able to allocate again from a clean slate.
    let q = heap.alloc(300, TypeTag::Array);
    assert!(q.is_some());
}

#[test]
fn test_live_object_iteration() {
    let mut heap = ActorHeap::new(64 * 1024);

    let p1 = heap.alloc(16, TypeTag::String).unwrap();
    let p2 = heap.alloc(32, TypeTag::Tuple).unwrap();
    let p3 = heap.alloc(48, TypeTag::Map).unwrap();

    // Free the middle object.
    unsafe {
        heap.free(p2);
    }
    assert_eq!(heap.live_count(), 2);

    // Collect all live object payload pointers.
    let mut live = Vec::new();
    heap.iter_live_objects(|_header, payload, _size| {
        live.push(payload);
    });

    assert_eq!(live.len(), 2);
    assert!(live.contains(&p1));
    assert!(!live.contains(&p2));
    assert!(live.contains(&p3));
}

#[test]
fn test_large_allocation() {
    let mut heap = ActorHeap::new(64 * 1024);

    // 512 bytes payload → total = 48 + 512 = 560 → Huge size class.
    let p = heap.alloc(512, TypeTag::Array).unwrap();
    unsafe {
        let h = &*ActorHeap::header_of(p);
        assert_eq!(h.size_class, SizeClass::Huge);
        assert_eq!(h.size, ActorHeap::HEADER_SIZE + 512);
        assert_eq!(h.type_tag, TypeTag::Array);
    }

    // Write and verify the full payload.
    unsafe {
        let buf = std::slice::from_raw_parts_mut(p, 512);
        for (i, slot) in buf.iter_mut().enumerate() {
            *slot = (i % 256) as u8;
        }
        for i in 0..512 {
            assert_eq!(buf[i], (i % 256) as u8);
        }
    }
}

#[test]
fn test_alignment() {
    let mut heap = ActorHeap::new(64 * 1024);

    // Allocate many objects with odd payload sizes and verify every
    // returned pointer is 8-byte aligned.
    let sizes = [1, 3, 7, 8, 9, 15, 17, 31, 33, 63, 65, 127, 129, 255];
    let mut ptrs = Vec::new();

    for &sz in &sizes {
        let p = heap.alloc(sz, TypeTag::Raw).unwrap();
        assert_eq!(
            p as usize % ALIGN,
            0,
            "payload pointer {:p} is not {}-byte aligned (size={})",
            p,
            ALIGN,
            sz
        );
        ptrs.push(p);
    }

    // Verify headers are also properly aligned.
    for p in &ptrs {
        unsafe {
            let h = ActorHeap::header_of(*p);
            assert_eq!(
                h as usize % ALIGN,
                0,
                "header pointer {:p} is not {}-byte aligned",
                h,
                ALIGN
            );
        }
    }
}

#[test]
fn test_header_size_is_56() {
    // The whole design assumes HEADER_SIZE == 56. If this ever changes,
    // classify_total_size and the memory layout comments must be updated.
    assert_eq!(ActorHeap::HEADER_SIZE, 56);
}

#[test]
fn test_alloc_zero_payload() {
    let mut heap = ActorHeap::new(64 * 1024);

    // Zero-byte payload should still allocate a header.
    let p = heap.alloc(0, TypeTag::Raw).unwrap();
    unsafe {
        let h = &*ActorHeap::header_of(p);
        assert_eq!(h.size, ActorHeap::HEADER_SIZE);
        assert_eq!(h.ref_count, 1);
    }
}

#[test]
fn test_statistics() {
    let mut heap = ActorHeap::new(64 * 1024);

    assert_eq!(heap.used(), 0);
    assert_eq!(heap.free_bytes(), 64 * 1024);
    assert_eq!(heap.live_count(), 0);
    assert_eq!(heap.free_list_count(), 0);

    let p1 = heap.alloc(100, TypeTag::Raw).unwrap();
    let _p2 = heap.alloc(200, TypeTag::Raw).unwrap();

    assert_eq!(heap.live_count(), 2);
    assert!(heap.used() >= ActorHeap::HEADER_SIZE * 2 + 100 + 200);

    unsafe {
        heap.free(p1);
    }
    assert_eq!(heap.live_count(), 1);
    assert_eq!(heap.free_list_count(), 1);
}

#[test]
fn test_los_alloc_free_reuse() {
    let mut heap = ActorHeap::new(64 * 1024);

    // 512-byte payload → total 568 > LOS_THRESHOLD → large-object space.
    let p1 = heap.alloc(512, TypeTag::Array).unwrap();
    unsafe {
        assert_eq!((*ActorHeap::header_of(p1)).size_class, SizeClass::Huge);
    }

    // LOS blocks must not consume the bump region.
    assert_eq!(heap.used(), 0);

    unsafe {
        heap.free(p1);
    }
    assert_eq!(heap.free_list_count(), 1);
    assert_eq!(heap.live_count(), 0);

    // Same size → the exact-match LOS free list returns the same block.
    let p2 = heap.alloc(512, TypeTag::Array).unwrap();
    assert_eq!(
        p1, p2,
        "freed LOS block should be reused on exact size match"
    );
    assert_eq!(heap.free_list_count(), 0);
}

#[test]
fn test_los_exact_size_reuse_only() {
    let mut heap = ActorHeap::new(64 * 1024);

    let p512 = heap.alloc(512, TypeTag::Array).unwrap();
    unsafe {
        heap.free(p512);
    }

    // A different (larger) size must not reuse the 512-payload block.
    let p768 = heap.alloc(768, TypeTag::Array).unwrap();
    assert_ne!(p512, p768, "LOS reuse requires an exact size match");
    assert_eq!(heap.free_list_count(), 1, "512-byte block still queued");

    // The original size is still available for reuse afterwards.
    let p512_again = heap.alloc(512, TypeTag::Array).unwrap();
    assert_eq!(p512, p512_again);
}

#[test]
fn test_los_interleaved_small_large() {
    let mut heap = ActorHeap::new(64 * 1024);

    // Interleave small (bump/size-class) and large (LOS) allocations.
    let s1 = heap.alloc(32, TypeTag::Tuple).unwrap();
    let l1 = heap.alloc(1024, TypeTag::Array).unwrap();
    let s2 = heap.alloc(32, TypeTag::Tuple).unwrap();
    let l2 = heap.alloc(2048, TypeTag::Array).unwrap();

    // Small and large regions must not overlap: LOS payloads lie outside
    // the heap's contiguous backing block.
    let bump_used = heap.used();
    assert!(
        bump_used > 0,
        "small allocations should use the bump region"
    );
    assert!(l1 < heap.base || l1 >= heap.limit);
    assert!(l2 < heap.base || l2 >= heap.limit);

    // Write markers into every region and verify they stay intact.
    unsafe {
        *(s1 as *mut u64) = 0x51;
        *(s2 as *mut u64) = 0x52;
        *(l1 as *mut u64) = 0xA1;
        *(l2 as *mut u64) = 0xA2;
        assert_eq!(*(s1 as *mut u64), 0x51);
        assert_eq!(*(s2 as *mut u64), 0x52);
        assert_eq!(*(l1 as *mut u64), 0xA1);
        assert_eq!(*(l2 as *mut u64), 0xA2);
    }

    // Free all, then re-allocate: small blocks reuse their size class,
    // large blocks reuse the LOS exact-match list.
    unsafe {
        heap.free(s1);
        heap.free(l1);
        heap.free(s2);
        heap.free(l2);
    }
    assert_eq!(heap.live_count(), 0);
    assert_eq!(heap.free_list_count(), 4);

    let s2r = heap.alloc(32, TypeTag::Tuple).unwrap();
    let l2r = heap.alloc(2048, TypeTag::Array).unwrap();
    let s1r = heap.alloc(32, TypeTag::Tuple).unwrap();
    let l1r = heap.alloc(1024, TypeTag::Array).unwrap();

    assert_eq!(s2r, s2, "LIFO reuse within the small size class");
    assert_eq!(s1r, s1);
    assert_eq!(l2r, l2, "exact-match reuse within the LOS");
    assert_eq!(l1r, l1);
    assert_eq!(heap.free_list_count(), 0);
}

#[test]
fn test_los_alignment() {
    let mut heap = ActorHeap::new(64 * 1024);

    // Odd payload sizes above the threshold — every LOS payload and header
    // must remain 8-byte aligned.
    let sizes = [257, 300, 511, 513, 1000, 4097];
    for &sz in &sizes {
        let p = heap.alloc(sz, TypeTag::Raw).unwrap();
        assert_eq!(
            p as usize % ALIGN,
            0,
            "LOS payload pointer {:p} is not {}-byte aligned (size={})",
            p,
            ALIGN,
            sz
        );
        unsafe {
            let h = ActorHeap::header_of(p);
            assert_eq!(
                h as usize % ALIGN,
                0,
                "LOS header pointer {:p} is not {}-byte aligned",
                h,
                ALIGN
            );
            // Payload must be fully writable.
            let buf = std::slice::from_raw_parts_mut(p, sz);
            buf[0] = 0xAB;
            buf[sz - 1] = 0xCD;
            assert_eq!(buf[0], 0xAB);
            assert_eq!(buf[sz - 1], 0xCD);
        }
    }
}

#[test]
fn test_los_threshold_boundary() {
    let mut heap = ActorHeap::new(64 * 1024);

    // total = HEADER_SIZE + align_up(payload); LOS kicks in above 256.
    // payload 200 → total 256 → Largest size-class block, bump path.
    let p_at = heap.alloc(200, TypeTag::Raw).unwrap();
    let used_after_small = heap.used();
    unsafe {
        assert_eq!((*ActorHeap::header_of(p_at)).size_class, SizeClass::Large);
    }
    assert_eq!(
        used_after_small, 256,
        "boundary block should use the bump region"
    );

    // payload 201 → total 264 → Huge, served by the LOS.
    let p_over = heap.alloc(201, TypeTag::Raw).unwrap();
    unsafe {
        assert_eq!((*ActorHeap::header_of(p_over)).size_class, SizeClass::Huge);
    }
    assert_eq!(
        heap.used(),
        used_after_small,
        "LOS must not touch the bump region"
    );

    // Free/realloc cycle on both sides of the boundary reuses each block.
    unsafe {
        heap.free(p_at);
        heap.free(p_over);
    }
    assert_eq!(heap.alloc(200, TypeTag::Raw).unwrap(), p_at);
    assert_eq!(heap.alloc(201, TypeTag::Raw).unwrap(), p_over);
}

#[test]
fn test_los_reset_releases_blocks() {
    let mut heap = ActorHeap::new(64 * 1024);

    let l1 = heap.alloc(1024, TypeTag::Array).unwrap();
    let l2 = heap.alloc(2048, TypeTag::Array).unwrap();
    unsafe {
        heap.free(l1);
    }
    assert_eq!(heap.free_list_count(), 1);
    let _ = l2;

    heap.reset();
    assert_eq!(heap.live_count(), 0);
    assert_eq!(heap.free_list_count(), 0);

    // Heap is fully functional after reset; LOS allocation still works.
    let l3 = heap.alloc(1024, TypeTag::Array).unwrap();
    unsafe {
        assert_eq!((*ActorHeap::header_of(l3)).size_class, SizeClass::Huge);
    }
}

#[test]
fn test_bump_advances_by_uniform_class_block_size() {
    let mut heap = ActorHeap::new(64 * 1024);

    // payload 16 -> total 72 -> Medium class: the bump pointer must reserve
    // the whole 128-byte class block, not the exact 72-byte total.
    heap.alloc(16, TypeTag::Array).unwrap();
    assert_eq!(
        heap.used(),
        128,
        "bump must reserve the uniform Medium class block"
    );

    // payload 1 -> total 64 -> Small class block of 64 bytes.
    heap.alloc(1, TypeTag::Raw).unwrap();
    assert_eq!(heap.used(), 128 + 64);
}

#[test]
fn test_free_then_larger_alloc_same_class_preserves_neighbor() {
    let mut heap = ActorHeap::new(64 * 1024);

    // A and B are adjacent: payload 16 -> total 72 (Medium).
    let a = heap.alloc(16, TypeTag::Array).unwrap();
    let b = heap.alloc(16, TypeTag::Array).unwrap();

    // Snapshot B's header.
    let (b_ref, b_foreign, b_size, b_class, b_tag) = unsafe {
        let h = &*ActorHeap::header_of(b);
        (
            h.ref_count,
            h.foreign_count,
            h.size,
            h.size_class,
            h.type_tag,
        )
    };

    // Free A, then allocate a larger object in the same class (payload 40 ->
    // total 96, still Medium).  The freed block is reused, but writing the
    // full 40-byte payload must not overrun into B's header (before the fix
    // the reused block was only 72 bytes and the write ran 24 bytes into B).
    unsafe {
        heap.free(a);
    }
    let a2 = heap.alloc(40, TypeTag::Array).unwrap();
    assert_eq!(a, a2, "freed Medium block should be reused");
    unsafe {
        std::ptr::write_bytes(a2, 0xAB, 40);
    }

    unsafe {
        let h = &*ActorHeap::header_of(b);
        assert_eq!(h.ref_count, b_ref, "neighbour ref_count corrupted");
        assert_eq!(h.foreign_count, b_foreign);
        assert_eq!(h.size, b_size);
        assert_eq!(h.size_class, b_class);
        assert_eq!(h.type_tag, b_tag);
    }
}

#[test]
fn test_zero_payload_free_preserves_neighbor() {
    let mut heap = ActorHeap::new(64 * 1024);

    // Zero-byte payloads are reachable via ArrAlloc/RecMk/TupleMk with
    // count 0 (empty [], {}, () literals).
    let e1 = heap.alloc(0, TypeTag::Array).unwrap();
    let e2 = heap.alloc(0, TypeTag::Array).unwrap();
    let neighbor = heap.alloc(0, TypeTag::Array).unwrap();

    unsafe {
        let h = &*ActorHeap::header_of(neighbor);
        assert_eq!(h.ref_count, 1);
        assert_eq!(h.size_class, SizeClass::Small);
        // The header records the exact requested total (header only): the
        // VM derives element counts from it, so an empty array stays len 0.
        assert_eq!(h.size, ActorHeap::HEADER_SIZE);
    }

    // Free e1 (stores a null next-pointer), then e2 (stores e1's non-null
    // pointer).  Both 8-byte writes must stay inside their own blocks.
    unsafe {
        heap.free(e1);
        heap.free(e2);
    }

    unsafe {
        let h = &*ActorHeap::header_of(neighbor);
        assert_eq!(
            h.ref_count, 1,
            "neighbour header clobbered by free-list next-pointer write"
        );
        assert_eq!(h.foreign_count, 0);
        assert_eq!(h.size_class, SizeClass::Small);
        assert_eq!(h.type_tag, TypeTag::Array);
        assert_eq!(h.size, ActorHeap::HEADER_SIZE);
    }
    assert_eq!(heap.live_count(), 1);
    assert_eq!(heap.free_list_count(), 2);

    // Empty blocks stay reusable across repeated alloc/free cycles.
    for _ in 0..16 {
        let p = heap.alloc(0, TypeTag::Array).unwrap();
        unsafe {
            heap.free(p);
        }
    }
    assert_eq!(heap.live_count(), 1);
    assert_eq!(heap.free_list_count(), 2);
}

#[test]
fn test_heap_pool_reuse_across_heaps() {
    // Create and drop several heaps; the pool should reuse blocks.
    let ptrs: Vec<*mut u8> = (0..4)
        .map(|_| {
            let heap = ActorHeap::new(64 * 1024);
            let base = heap.base;
            std::mem::forget(heap); // don't drop — we'll check the pool
            base
        })
        .collect();

    // Drop the heaps to return blocks to the pool.
    for &base in &ptrs {
        HEAP_POOL.with(|pool| {
            pool.borrow_mut().release(base, 64 * 1024);
        });
    }

    // Now allocate new heaps — they should get pooled blocks.
    let mut reused = 0;
    for _ in 0..4 {
        let heap = ActorHeap::new(64 * 1024);
        if ptrs.contains(&heap.base) {
            reused += 1;
        }
        // Don't return to pool during test cleanup.
        let base = heap.base;
        let size = heap.total_size;
        std::mem::forget(heap);
        // Deallocate manually so we don't pollute the pool for other tests.
        let layout =
            std::alloc::Layout::from_size_align(size, crate::runtime::heap::ALIGN).unwrap();
        unsafe {
            std::alloc::dealloc(base, layout);
        }
    }
    assert!(reused > 0, "pool should have reused at least one block");
}
