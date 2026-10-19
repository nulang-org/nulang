# Runtime EXPLAIN ANALYZE (initial API)

Nulang exposes an **opt-in Rust embedding API** for profiling a bounded operation
performed with a single `Runtime` instance. It reuses runtime snapshots instead
of introducing a second scheduler, new Nulang syntax, or implicit global
instrumentation.

```rust
use nulang::runtime::Runtime;

let mut rt = Runtime::new();
// Spawn actors, register behaviors, enqueue work as usual.

let (outcome, report) = rt.explain_analyze(|rt| rt.run_scheduler());
println!("{}", report.render_text());
println!("{}", serde_json::to_string_pretty(&report).unwrap());
```

The return type is `(R, RuntimeExplainReport)`, so the operation's original
return value is preserved. The result serializes with `schema_version = 1`.

## Measurement contract

- `wall_time_ns` measures only the passed closure, excluding the metric
  snapshots taken immediately before and after it. This is **wall time**,
  including blocking inside the closure, not CPU time.
- Scheduler values are differences between the existing scheduler counters
  at the two sampling boundaries. **Do not call
  `Runtime::reset_scheduler_stats` during a measured scope.** If a counter
  decreases, `scheduler_counter_reset` is set and the affected deltas are
  incomplete. Resets followed by enough new work to exceed the previous
  value cannot be detected from endpoint snapshots alone.
- `mailbox_messages_before` and `mailbox_messages_after` are sums across
  all resident actor mailboxes at those sampling boundaries. `mailboxes`
  includes at most 16 actors with nonzero depth at either boundary, ordered
  by descending *initial* depth and then final depth. `mailboxes_omitted`
  records how many were truncated.
- `gc_live_before` and `gc_live_after` are aggregate GC counters for
  **currently resident actors**, not precise memory allocations within the
  closure. Actor creation/exit or stats resets make differences unsuitable
  as allocation deltas.
- `actors_live_*` and `dlq_depth_*` are point-in-time gauges.
- Sampling is shard-local. Another runtime shard and remote nodes are not
  included. This is not a coherent cluster-wide snapshot.

The profiler does not derive per-actor CPU time, blocked/wait event time,
JIT promotion/compiler time, workflow I/O time, individual durable commits,
or per-effect attribution. Adding any of those requires additional
instrumentation, overhead measurements, and independent correctness tests.
The report is therefore named **scope aggregate**, not an execution plan.

## Testing

```bash
cargo test --locked --no-default-features --test runtime_explain_analyze
cargo test --locked --no-default-features runtime::explain::tests
cargo fmt --all -- --check
```

The Rust API introduces no `.nula` syntax or persisted wire-format change.
