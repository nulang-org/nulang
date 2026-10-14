# Actor inspection (experimental Rust runtime API)

The OTP/Observer-inspired first inspection slice exposes **metadata**, not
mailbox contents, actor state values, capabilities, or flight recordings.

```rust
use nulang::runtime::Runtime;

let runtime = Runtime::new();
let first_page = runtime.actor_info_page(None, 50);
let next_page = first_page.last().map(|actor| actor.id)
    .map(|cursor| runtime.actor_info_page(Some(cursor), 50));

if let Some(actor) = first_page.first() {
    let detail = runtime.actor_info(actor.id);
    let json = serde_json::to_string(&detail)?;
}
```

The example is illustrative of an embedding context (the snippet's caller
must return a `Result` for `?`). Both APIs require `&self` and are
**shard-local**. They do not remotely query an actor's owning node/shard.
Call them from the owning scheduler context or when its runtime is idle;
do not concurrently access/mutate `Runtime` outside its ownership model.

- `actor_info(id)` returns `None` if no living actor with that id exists
  on the inspected shard.
- `actor_info_page(after, limit)` returns actors ordered by increasing id.
  The cursor is exclusive. Zero limit returns an empty page, and requests
  above `MAX_ACTOR_INFO_PAGE_SIZE` (100) are capped.
- Pages are not transactional across concurrent births and exits.
- Paging uses bounded O(limit) temporary memory but performs O(actor_count
  log limit) per-page selection, so callers must not poll it per scheduler
  tick or at high frequency.
- `processed_messages` counts completed message-handler reductions, **not**
  bytecode instructions or milliseconds of CPU utilization.
- `state`, `run_state` and `priority` are descriptive debug strings,
  not frozen serialization enum values.
- `activation_epoch` is included only for persistent actors, so operators
  can investigate recovery fencing. A snapshot is never proof of cluster-wide
  exclusive ownership.

Snapshots deliberately **exclude** user state, message bodies, pending tool
arguments, authority manifests, secrets and flight recorder contents.
Adding a remotely accessible inspection endpoint requires authentication,
tenant/capability authorization, rate limits, and redaction. No such endpoint
is introduced here.

## Next steps

1. Add per-actor CPU/fuel instrumentation and validated yield/suspend/resume
   safepoints for bytecode, JIT, and AOT. A message-count turn limit cannot
   preempt an unbounded handler.
2. Add bounded mailbox-age metrics, restart counters, and supervisor
   topology integration without copying user payloads.
3. Route inspection requests through scheduler-owned shards, then expose
   an authenticated `nulang inspect` CLI and optional Observer-style UI.

This interface is **not** a claim of BEAM-style mid-handler preemption or of
remote production observability.
