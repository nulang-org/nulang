# RFC 0016: Virtual Actor Auto-Hydration and Immutable Shared Object Store

- **Status:** Implemented
- **Tier:** Experimental
- **Author:** Assistant
- **Created:** 2026-08-18
- **Resolved:** 2026-08-19
- **Language-version at effect:** (Experimental; no frozen/stable surface changed)
- **Supersedes:** none
- **Superseded by:** none

## Summary

This RFC proposes two complementary runtime extensions:

1. **Virtual Actors (Orleans-style grains)** — extend Nulang’s existing durable `entity` actors with transparent auto-hydration and dehydration. A message sent to a not-currently-resident grain key causes the runtime to load the latest snapshot, instantiate the actor, deliver the message, and later reclaim the memory when the grain goes idle.

2. **Immutable Shared Object Store (Ray-style plasma)** — add a node-local shared-memory pool for large immutable `val` buffers. `val Bytes` / `val Tensor` values are represented by lightweight `ObjectRef` handles that can be sent between actors without copying. Remote nodes receive a fetchable reference; the runtime streams the buffer out-of-band.

Both features build on capabilities and subsystems that already exist: the capability lattice, `entity` persistence, ORCA foreign-reference tracking, and the `ActorAddress` location-transparent addressing model.

## Motivation

Nulang already has most of the pieces for durable, distributed actors, but two gaps remain that directly limit production readiness for stateful services and AI workloads:

- **Virtual actors:** Today `entity` declarations are durable-first (`src/runtime/persistence.rs:17–27`), hibernation state exists (`src/runtime/actor.rs:162–173`), and recovery can replay snapshots/journals (`src/runtime/mod.rs:3984`). However, there is no way to send a message to a *key* and have the runtime materialize the actor on demand. The caller must either spawn the actor explicitly or rely on node-failure respawn. Orleans-style virtual identity removes this boilerplate and enables elastic memory usage.

- **Large immutable payloads:** The runtime currently forbids heap pointers in cross-actor messages (`src/runtime/mod.rs:1858`, `src/runtime/network.rs:1501–1517`). Strings travel by content via a per-packet `string_table` (`src/runtime/distributed.rs:1655–1720`). For embeddings, tensors, images, or JSON blobs, this means copying through mailboxes. Because `Capability::Val` already guarantees immutability (`src/types.rs:86`), Nulang can safely share such buffers in memory and only stream them across nodes when necessary.

## Design

### A. Virtual Actor Auto-Hydration

#### A.1 Grain manifest registry

Introduce a new registry inside `Runtime`:

```rust
// src/runtime/mod.rs (inside Runtime)
grain_registry: GrainRegistry,
```

```rust
// src/runtime/grain.rs (new file)
pub struct GrainRegistry {
    /// grain type name (e.g., "User") -> factory
    types: HashMap<String, GrainType>,
}

pub struct GrainType {
    /// Module containing the bytecode behavior table for this grain.
    pub module: crate::bytecode::CodeModule,
    /// Default state models parsed from the `entity` declaration.
    pub default_models: Vec<(String, StateModel)>,
    /// Bytecode offsets parallel to the module behavior table.
    pub bytecode_offsets: Vec<usize>,
    /// Compensation offsets (for saga workflows) parallel to behavior table.
    pub compensation_offsets: Vec<Option<usize>>,
    /// Optional dehydration policy.
    pub dehydrate_policy: DehydratePolicy,
}

#[derive(Clone, Copy)]
pub struct DehydratePolicy {
    /// Idle milliseconds before the runtime may hibernate the grain.
    pub idle_ms: u64,
    /// Whether the grain may be dehydrated at all.
    pub allow_dehydrate: bool,
}
```

The registry is populated at module-load time from `entity` declarations, mirroring how `spawn_from_module` builds `bytecode_offsets` today (`src/runtime/spawn.rs:177`).

#### A.2 Grain identity

A virtual actor is addressed by a *grain id*, not a raw `u64` actor id:

```rust
// src/runtime/grain.rs
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GrainId {
    pub grain_type: String,
    pub key: String,
}
```

The grain id is deterministically mapped to a stable actor id using a 64-bit hash (e.g., `seahash` or splitmix64). The runtime keeps two indexes:

```rust
// src/runtime/mod.rs (inside Runtime)
/// GrainId -> currently resident actor id (if any).
grain_residents: HashMap<GrainId, u64>,
/// actor id -> GrainId for every resident virtual actor.
actor_grain_id: HashMap<u64, GrainId>,
```

This lets `send_message_by_id` continue to operate on raw actor ids for non-virtual actors, while a new `send_to_grain` path resolves `GrainId` -> resident or hydrated actor id.

#### A.3 Hydration hook

The central interception point is `Runtime::send_message_by_id` (`src/runtime/mod.rs:1793`). After the remote-ref, migration, and cross-shard checks, but before the local mailbox push at `mod.rs:1885`, add:

```rust
if let Some(grain_id) = self.actor_grain_id.get(&target_id).cloned() {
    let actor = self.actors.get_mut(&target_id).unwrap();
    if actor.is_hibernated() {
        actor.wake_from_hibernation(self)?;
    }
    // proceed to mailbox push
}
```

For a send that starts from a `GrainId` (e.g., `send Grain("user:42").DoIt(...)`), the new path is:

```rust
// src/runtime/mod.rs
pub fn send_to_grain(
    &mut self,
    grain_id: GrainId,
    behavior_name: &str,
    args: Vec<Value>,
    sender: u64,
) -> Result<(), NuError> {
    let actor_id = self.resolve_or_hydrate_grain(grain_id)?;
    self.send_message(actor_id, behavior_name, args, sender)
}

fn resolve_or_hydrate_grain(&mut self, grain_id: GrainId) -> Result<u64, NuError> {
    if let Some(&id) = self.grain_residents.get(&grain_id) {
        return Ok(id);
    }

    let grain_type = self.grain_registry.get(&grain_id.grain_type)?;
    let stable_actor_id = grain_actor_id(&grain_id);

    // 1. Try persistence: latest snapshot + journal.
    let snapshot = self.persistence.load_snapshot(stable_actor_id).ok();

    // 2. Allocate a fresh Actor.
    let mut actor = Actor::new(stable_actor_id, grain_id.to_string());
    actor.persistent = true;
    actor.bytecode_module = Some(grain_type.module.clone());
    actor.bytecode_module_idx = Some(self.register_recovery_module(stable_actor_id, ...));
    actor.bytecode_offsets = grain_type.bytecode_offsets.clone();
    actor.compensation_offsets = grain_type.compensation_offsets.clone();

    // 3. Restore state if a snapshot exists.
    if let Some(snap) = snapshot {
        self.restore_actor_from_snapshot(&mut actor, &snap)?;
        self.persistence.read_journal(stable_actor_id)?
            .into_iter()
            .for_each(|entry| { /* replay idempotently */ });
    } else {
        // First activation: apply entity defaults.
        for (name, model) in &grain_type.default_models {
            actor.state_models.insert(name.clone(), *model);
        }
    }

    // 4. Register and enqueue.
    self.actors.insert(stable_actor_id, actor);
    self.grain_residents.insert(grain_id.clone(), stable_actor_id);
    self.actor_grain_id.insert(stable_actor_id, grain_id);
    self.enqueue_actor(stable_actor_id);

    Ok(stable_actor_id)
}
```

The `restore_actor_from_snapshot` logic can be factored out of the existing `Runtime::recover_actor` (`src/runtime/mod.rs:3984`) so hydration and recovery share one code path.

Additional interception points that must perform the same resolution:

- `Runtime::send_message` (`src/runtime/mod.rs:1429`) for name-based sends to grain targets.
- `Runtime::deliver_cross_shard_message` (`src/runtime/mod.rs:1168`) when the cross-shard target is a virtual actor on this shard.
- `distributed::process_network_packets` (`src/runtime/distributed.rs:944`) for remote `Packet::ActorMessage` whose target actor is missing but whose actor id matches a known grain id.

Because the scheduler is single-threaded per shard, all hydration happens synchronously on the scheduler thread. Persistence I/O must therefore be non-blocking or very fast; for slow stores, hydrate should dispatch an async load and suspend the sender (see Open Questions).

#### A.4 Dehydration

The existing hibernation machinery is currently un-triggered from the scheduler (research note: `Actor::increment_idle` exists but has no caller). Wire it into the scheduler loop:

```rust
// src/runtime/scheduler.rs, inside run_scheduler tick
for (actor_id, actor) in self.actors.iter_mut() {
    if let Some(grain_id) = self.actor_grain_id.get(actor_id) {
        let policy = self.grain_registry[&grain_id.grain_type].dehydrate_policy;
        if policy.allow_dehydrate
            && actor.state == ActorState::Waiting
            && actor.idle_ms >= policy.idle_ms
            && actor.mailbox.is_empty()
        {
            actor.increment_idle(policy.idle_ms); // or directly call hibernate()
            actor.hibernate(self)?;
            // Keep the GrainId mapping so the next send re-hydrates.
            actor.state = ActorState::Waiting; // hibernated but addressable
        }
    }
}
```

`Actor::hibernate` (`src/runtime/actor.rs:381`) already serializes continuation and state. After hibernation, the actor remains in `self.actors` but marked hibernated; `send_message_by_id` wakes it on demand.

For memory reclamation under pressure, add an optional `evict_hibernated` path that removes hibernated grains from `self.actors` entirely while keeping the `GrainId` -> actor id mapping. The next send re-creates the actor from persistence.

#### A.5 Pre-warming and pinning

Expose two new built-in effects:

```nula
-- Pre-warm: load the grain into memory if not already resident.
perform Grain.prewarm("User", "user:42")

-- Pin: mark the grain as non-evictable until unpinned.
perform Grain.pin("User", "user:42")
perform Grain.unpin("User", "user:42")
```

These map to runtime methods that call `resolve_or_hydrate_grain` and set a `pinned: bool` flag on the actor. Pinned grains bypass dehydration and eviction.

#### A.6 Language surface

Extend `entity` syntax with an optional `virtual` modifier:

```nula
virtual entity User(key: String) {
    state durable name: String = ""
    state durable balance: Int = 0

    behavior Greet(who: String) {
        perform IO.print("Hello " + who + ", your balance is " + self.balance)
    }
}
```

A virtual entity can only be addressed by `Grain("User", key)`; it cannot be `spawn`ed directly. Process actors (ordinary `actor`) keep the existing explicit-spawn semantics.

---

### B. Immutable Shared Object Store

#### B.1 Value representation

Add a new tag and constructor to the canonical value layout:

```rust
// src/value_layout.rs
pub const TAG_OBJECT: u64 = 0x7FF5; // next free NaN slot below TAG_PYTHON

pub fn tag_object(id: u64) -> u64 {
    debug_assert!(id <= PAYLOAD_MASK);
    SIGN_BIT | (TAG_OBJECT << 48) | id
}
```

```rust
// src/vm.rs
impl Value {
    pub fn object(id: u64) -> Value { Value { raw: value_layout::tag_object(id) } }
    pub fn is_object(self) -> bool { self.tag() == TAG_OBJECT }
    pub fn as_object_id(self) -> Option<u64> { if self.is_object() { Some(self.payload()) } else { None } }
}
```

Update `value_layout.rs` uniqueness tests to include `TAG_OBJECT`.

#### B.2 Object store subsystem

Create `src/runtime/object_store.rs`:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub type ObjectId = u64;

pub struct ObjectStore {
    next_id: ObjectId,
    entries: HashMap<ObjectId, Arc<ObjectEntry>>,
}

pub struct ObjectEntry {
    pub id: ObjectId,
    pub len: usize,
    /// Immutable bytes, typically mmap'd shared memory.
    pub bytes: Box<[u8]>,
    /// Refcount across all actors on this node that hold an ObjectRef.
    pub node_refs: AtomicUsize,
    /// Which remote nodes have fetched this object (for eager invalidation).
    pub remote_nodes: Mutex<HashSet<NodeId>>,
}

impl ObjectStore {
    pub fn put(&mut self, bytes: Box<[u8]>) -> ObjectId {
        let id = self.next_id;
        self.next_id += 1;
        let entry = Arc::new(ObjectEntry {
            id,
            len: bytes.len(),
            bytes,
            node_refs: AtomicUsize::new(1),
            remote_nodes: Mutex::new(HashSet::new()),
        });
        self.entries.insert(id, entry);
        id
    }

    pub fn get(&self, id: ObjectId) -> Option<Arc<ObjectEntry>> {
        self.entries.get(&id).cloned()
    }

    pub fn drop_ref(&mut self, id: ObjectId) {
        if let Some(entry) = self.entries.get(&id) {
            let prev = entry.node_refs.fetch_sub(1, Ordering::Release);
            if prev == 1 {
                self.entries.remove(&id);
                // bytes freed when Arc drops
            }
        }
    }
}
```

The store is owned by `Runtime` (one per shard/node; see Open Questions) and protected by a mutex because `ObjectRef` handles may be created from any actor but are immutable once inserted.

Use `mmap` (or `memfd_create` on Linux) for buffers above a size threshold so that multiple processes/VMs on the same node can map the same physical pages. For a single-process runtime, a global allocator-backed `Box<[u8]>` is sufficient as an MVP.

#### B.3 Creating ObjectRefs

Add a built-in effect:

```nula
let tensor = perform ObjectStore.put(val_bytes)
```

At the MIR level this compiles to a new opcode `OpCode::ObjectPut` (or reuse `Perform` for an `ObjectStore` effect). The runtime copies the source `val Bytes` into the store and returns `Value::object(id)`.

#### B.4 Sending ObjectRefs

- **Local same-shard send:** `Value::object(id)` is sent by value. No copy.
- **Cross-shard send:** whitelist `is_object()` alongside ints/floats/bools/strings in the cross-shard safety check (`src/runtime/mod.rs:1858`). The receiving shard looks up the same node-local `ObjectStore`.
- **Cross-node send:** encode `Value::object(id)` in the wire protocol. Two options:
  1. **Lazy:** send a small `ObjectRef(node_id, object_id, len)` descriptor. The receiver allocates a local placeholder and fetches the bytes on first access via a new `Packet::ObjectFetch`.
  2. **Eager:** inline the bytes in `Packet::ActorMessage` when the object is small; use lazy for large objects.

Add to `src/runtime/network.rs`:

```rust
// in write_value/read_value
Object(id) => {
    buf.push(WIRE_OBJECT);
    buf.write_u64::<BE>(id)?;
}
```

And add `Packet::ObjectFetch { object_id, request_id }` / `Packet::ObjectData { request_id, bytes }`.

#### B.5 Garbage collection

Because objects are immutable and not in any actor heap, ORCA is not involved. Use a node-global refcount:

- On `ObjectStore.put`, refcount = 1 (creator).
- When a message containing `Value::object(id)` is delivered, the receiver actor’s held-objects set gains the id and the store refcount increments.
- On actor exit, `Runtime::release_held_object_refs(actor_id)` decrements refcounts for all ids in the actor’s held set.
- When refcount reaches zero, remove the entry.

This mirrors the existing `Runtime::hold_payload_refs` / `release_held_foreign_refs` pattern (`src/runtime/mod.rs:2428–2485`) but operates on object ids instead of `TAG_PTR` headers.

#### B.6 Capability integration

Only `val` capabilities may be stored in or retrieved from the object store. The compiler enforces this statically: `ObjectStore.put` requires a `val` argument. At runtime, store and send operations assert `!value.is_ptr()` and `value.is_object()`; no writable `ref`/`iso` object can be placed in the store because the type system forbids it.

For extra safety, the object store marks each entry as read-only using `mprotect(..., PROT_READ)` after insertion so that even a rogue `unsafe` block cannot mutate a shared buffer.

#### B.7 Language surface

Introduce a library module (or built-in effect family):

```nula
module ObjectStore {
    -- Store immutable bytes and return a lightweight handle.
    effect put(bytes: val Bytes): ObjectRef

    -- Fetch the immutable bytes behind a handle.
    effect get(ref: ObjectRef): val Bytes

    -- Local-only length query (no copy).
    effect len(ref: ObjectRef): Int
}
```

For cross-node usage, `ObjectRef` is location-transparent: the runtime resolves it locally if the object is already present, otherwise fetches from the owning node.

## Implementation Notes

Both phases of this RFC are implemented, plus the first round of recommended next-step work, and all are covered by tests:

- **Object store (Phase 1):** `src/runtime/object_store.rs` provides a per-shard immutable buffer pool with node-local refcounting. `Value::object(id)` is represented by `TAG_OBJECT` (`src/value_layout.rs`). Object refs are whitelisted in cross-shard sends and serialized in the NUL0 wire protocol (`src/runtime/network.rs`).
- **Virtual actors (Phase 2):** `src/runtime/grain.rs` defines `GrainId`, `GrainRegistry`, and `GrainType`. The parser accepts `virtual entity Name(key: Type) { ... }` (`src/parser.rs`, `src/ast.rs`) and the flag is threaded through HIR/MIR into `bytecode::ActorMeta.is_virtual` (`src/hir.rs`, `src/hir_lower.rs`, `src/mir_lower.rs`, `src/bytecode.rs`). `Runtime::register_module_grains` populates the registry at every module-load entry point (`src/runtime/mod.rs`, `src/runtime/spawn.rs`, `src/runtime/distributed.rs`, `src/main.rs`, `src/integration_tests/mod.rs`).
- **Hydration/dehydration:** `Runtime::resolve_or_hydrate_grain` loads snapshots, replays journals, and enqueues the actor. `Runtime::send_to_grain` and the `send_message_by_id` hook wake hibernated grains and re-hydrate evicted ones. `Runtime::dehydrate_idle_grains` persists snapshots and hibernates idle grains every `DEHYDRATE_CHECK_INTERVAL` scheduler ticks; pinned actors are skipped.
- **Language surface:** `Grain("Type", key)` parses as a dedicated expression (`src/ast.rs`, `src/parser.rs`, `src/typechecker.rs`, `src/effect_checker.rs`, `src/hir_lower.rs`, `src/fmt.rs`, `src/lsp/mod.rs`) and desugars to `perform Grain.ref("Type", key)`.
- **Built-in Grain effects:** `Runtime::perform_grain_builtin` implements `Grain.ref`, `Grain.prewarm`, `Grain.pin`, and `Grain.unpin` (`src/runtime/mod.rs`, `src/runtime/callbacks.rs`).
- **Cross-shard/cross-node grain routing:** `Runtime::send_to_grain` routes to the owning shard by `stable_id % shard_count`; cross-shard `DeliverMessage` carries the `GrainId` so the owner shard can hydrate on first delivery. `Runtime::send_to_grain_on_node` provides explicit cross-node routing (`src/runtime/mod.rs`, `src/runtime/distributed.rs`).
- **Eviction under memory pressure:** `Runtime::evict_hibernated_grains` removes hibernated grains from `self.actors` while keeping the stable identity mapping, and `Runtime::maybe_evict_under_pressure` provides a coarse heuristic trigger (`src/runtime/mod.rs`).
- **Tests:** Phase 1 object-store tests live in `src/runtime/tests.rs` and `src/integration_tests/mod.rs`. Phase 2 grain tests and the next-step tests (grain ref expression, built-in effects, cross-shard routing, eviction) live in `src/integration_tests/mod.rs` and `src/runtime/tests.rs`.

Verification:
- `cargo test` — 1882 passed, 0 failed
- `cargo test --features wasm-backend` — 1963 passed, 0 failed
- `cargo test -p nulang-ai` — 58 passed, 0 failed
- `python3 verify_implementation.py` — passed with 0 warnings

## Tier Classification

Both features are **Experimental**.

- They add new syntax and runtime behavior behind no frozen/stable contract.
- The wire protocol changes (`Packet::ObjectFetch` / `ObjectData`) do not modify the existing `Packet::ActorMessage` layout; they are additive packet types.
- Virtual actor identity mapping and object-store wire encoding may change based on production experience.

No Frozen or Stable surface is changed.

## Backwards Compatibility

- **Virtual actors:** Existing `actor` and `entity` declarations continue to work unchanged. `virtual entity` is a new keyword combination. Non-virtual actors are never auto-hydrated.
- **Object store:** Existing programs that do not use `ObjectStore.put` are unaffected. The new `TAG_OBJECT` value tag is internal; it only appears in values produced by the new effect.
- **Wire protocol:** New packet types are additive. Older nodes will reject unknown packet types until upgraded; this is acceptable for an Experimental feature.

## Alternatives Considered

### Virtual actors

1. **Use raw actor ids as grain ids.** Rejected because it forces callers to know the hash scheme and prevents key-based addressing (`User("user:42")`).
2. **Hydrate from a separate node-local cache rather than persistence.** Rejected because persistence is the source of truth; a cache layer can be added later without changing the abstraction.
3. **Implement virtual actors purely in the package/stdlib layer.** Rejected because transparent hydration must intercept sends at the runtime level.

### Object store

1. **Reuse `TAG_PTR` and store buffers in actor heaps.** Rejected because actor heaps are private and non-shared; sending a `TAG_PTR` cross-actor would require deep ORCA integration for shared ownership and would violate the existing cross-shard safety rules.
2. **Use ORCA foreign counts for object-store ref counting.** Rejected because object-store buffers are not `OrcaHeader` objects. A global refcount is simpler and sufficient for immutable data.
3. **Always inline large payloads in `Packet::ActorMessage`.** Rejected because it defeats the purpose for multi-megabyte tensors and would cause head-of-line blocking.

## Open Questions

1. **Slow persistence during hydration.** If `load_snapshot` is slow (e.g., remote SQL), should hydrate block the scheduler thread or dispatch an async load and suspend the sender? The latter requires a new `GrainHydrate:suspend` sentinel similar to LLM/signal suspends.
2. **Object store scope.** Should there be one `ObjectStore` per node or per shard? Per-node sharing is more efficient but requires cross-shard synchronization; per-shard is simpler but copies large buffers between shards.
3. **Object id stability across node restarts.** Object ids are runtime-allocated and ephemeral. Should the store support named/durable objects for workflow checkpoints?
4. **`ObjectRef` serialization in hibernation.** When a hibernated grain holds an `ObjectRef`, should the snapshot store the id or the bytes? Storing the id is smaller but ties the grain to a runtime-scoped object.
5. **Remote object streaming transport.** Should large objects use a separate TCP connection, RDMA, or QUIC stream, or be chunked over the existing NUL0 connection?
6. **Mmap backend portability.** Linux supports `memfd_create`; macOS and Windows require different paths. Should the MVP use a portable anonymous mmap or a file-backed mmap?

## Resolution

(To be filled in on accept/reject.)
