# Actor Flight-Recorder Diagnostics (Experimental)

Nulang's existing per-actor flight recorder can now export a bounded, chronological
**metadata-only JSONL** history suitable for inspecting local mailbox admission
and dispatch. This is an incremental diagnostics surface, **not** a complete
Instruments profiler, distributed event history, or deterministic replay log.

## Enable and inspect

Per-actor recording is opt-in outside tests:

```sh
NULANG_FLIGHT_RECORDER=1 nulang app.nula
```

The variable is read once on first use. Embedders can inspect the runtime:

```rust
if let Some(jsonl) = runtime.actor_trace_jsonl(actor_id) {
    print!("{jsonl}");
}
```

`None` means the actor does not exist in this local runtime. `Some("")`
means the recorder is empty or disabled. The runtime default retains at most
1,000 events per actor, overwriting the oldest events. Event sequence numbers
are local to each actor; there is no cross-actor total order.

## Export schema

One JSON object per line, oldest first:

```json
{"actor_id":12,"seq":0,"event":"admitted","sender":7,"behavior_id":1,"payload_len":1}
{"actor_id":12,"seq":1,"event":"backpressured","sender":7,"behavior_id":1,"payload_len":1}
{"actor_id":12,"seq":2,"event":"dequeued","sender":7,"behavior_id":1,"payload_len":1}
```

- `admitted`: a local message passed mailbox admission. It is **not** a
  guarantee that its handler ran.
- `backpressured`: a local send to an existing, capacity-limited mailbox
  was rejected. The payload did not enter that mailbox.
- `dequeued`: the scheduler removed a message for dispatch. It is **not**
  a success/completion signal and does not include time spent queued.

The export **never serializes payload values or the internal
`payload_summary`**. The existing in-memory `TraceEntry` API *can* retain
payload summaries when recording is enabled, so do not expose raw entries
to untrusted consumers or turn it on indiscriminately in sensitive workloads.

Only the local delivery/dispatch paths above currently emit these lifecycle
events. Unknown targets, unknown behaviors, cross-shard and cross-node sends,
special system messages, and selective-receive internals are not guaranteed
to appear as complete lifecycles. Backpressure may also emit a dead-letter
notification; JSONL does not provide a retry mechanism.

## Operating characteristics

Recording is disabled by default and the disabled path returns without
allocating trace entries. Enabled histories are in-memory, per actor, bounded,
and erased on actor removal or recorder reset. Export allocates a JSONL
snapshot on demand; do not call it on every hot-path message.

Planned extensions should be driven by profiling evidence: monotonic
queue-wait durations, supervision transitions, traceparent correlation,
OpenTelemetry export, and an eventual timeline UI. None is implemented by
this initial metadata export.
