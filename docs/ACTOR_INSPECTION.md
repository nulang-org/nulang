# Read-only actor inspection (developer API)

Nulang exposes `Runtime::inspect_actor(actor_id)` and `Runtime::inspect_actors()`
for **in-process, read-only** inspection of resident actor metadata. The
implementation is inspired by Smalltalk's live object inspectors but does not
introduce unrestricted reflection or mutable access to actor state.

```rust
use nulang::runtime::Runtime;

let mut rt = Runtime::new();
let actor_id = rt.spawn_actor(Box::new(Vec::new));
let info = rt.inspect_actor(actor_id).expect("actor exists");
assert_eq!(info.actor_id, actor_id);
assert_eq!(info.mailbox_depth, 0);

let json = serde_json::to_string(&info)?;
```

## Data contract

Each `ActorInspection` contains actor ID and name; lifecycle, scheduler
ownership, and priority; mailbox depth; parent and sorted child IDs; link and
monitor counts; whether the actor is persistent or a workflow; persisted
sequence; and activation epoch. `inspect_actors()` returns snapshots in
ascending actor-ID order, independent of hash-map iteration order.

No field contains user-defined state, message payloads, flight-recorder traces,
suspended execution, authority grants, or effect arguments. Snapshots own
their data, so inspecting an actor does not dequeue messages, invoke handlers,
or give the caller a mutable reference to actor storage.

## Important boundaries

- These are **Rust host APIs**, not Nulang source-level builtins, web routes,
  distributed queries, or DAP operations. They do not expose a remote endpoint.
- The caller must already own a reference to the `Runtime`. **Authorize**
  access before exposing the serialized data through any external transport.
  Names, relationships, and message counts are operational metadata and may
  themselves be sensitive.
- Snapshots cover actors currently resident in **one runtime shard**, not
  other nodes or migrated actors. Missing/removed actors return `None`.
- This is an observational view, **not an atomic distributed snapshot**.
  Concurrent mailbox producers can change depths during sampling; the caller
  should not use this output to make admission, recovery, or security decisions.
- Debugger integration, user-visible inspector UI, effects inspection, and
  historical replay require separate gated changes. In particular, this API
  does not alter durable journals or historical reconstruction.

## Regression verification

```bash
cargo test --locked --no-default-features --lib actor_inspection_
cargo fmt --all -- --check
```
