# Trace-context diagnostic benchmarks

The optional `trace_profile` Criterion benchmark separates W3C trace-context
costs from actor scheduling, mailbox admission, message delivery, and handler
execution. It is **diagnostic-only** and does not participate in the rolling
main-branch regression gate.

```sh
cargo bench --locked --no-default-features --bench trace_profile
cargo bench --locked --no-default-features --bench trace_profile -- --test
```

The probes measure individual operations: root generation, child generation,
parsing a fixed valid W3C traceparent, parsing plus child generation, outbound
string encoding, and the dispatch span guard. No source compilation,
serialization to the NUL0 wire, actor scheduling, or end-to-end message
execution is included. The guard probe normally exercises the disabled-TRACE
path; installed subscribers may change its behavior, so record subscriber
configuration for every run.

For optimization decisions, run exact base and candidate on the **same idle
host**, preferably pinned to one logical CPU with alternating execution order,
and compare raw per-run distributions and confidence intervals. Repeat with
multiple threads when evaluating global ID-generation contention. A
single-threaded Criterion result cannot establish cross-core scaling.

Pair these probes with the existing 50,000-message native actor drain fixture
and its traced/untraced controls (issue #2128). A reduction in parse latency
does not prove an equivalent actor-throughput increase. Keep traceparent
validation, parent/child relationships, sampling flags, downstream propagation,
wire compatibility, and replay semantics intact.

## Traced vs untraced actor dispatch

The opt-in `trace_actor_profile` Criterion harness compares two **100-message**
receive batches with a registered no-op native handler. Runtime construction,
actor spawning, setup message drain, traceparent stamping, and teardown are
excluded from the measured section using `iter_batched_ref`.

```sh
cargo test --locked --no-default-features --bench trace_actor_profile -- --test
cargo bench --locked --no-default-features --bench trace_actor_profile
```

The `root_per_receive` fixture has no incoming traceparent; each received
message starts a fresh root. The `parse_child_per_receive` fixture stamps an
incoming W3C traceparent during enqueue and parses it to create a child at
receive. Both assert exactly 100 messages admitted and that the mailbox is
empty after processing. Thus a silent drop or a wrong handler cannot produce
false throughput.

The comparison measures scheduler/mailbox/handler cost **plus** tracing
behavior; it does not isolate just parser instructions. Report traced and
untraced values separately, and use exact-base paired same-runner comparisons
before attributing any change to the codec. The smoke test only verifies the
fixture, not speedup. Profiling changes never waive repository security checks.
