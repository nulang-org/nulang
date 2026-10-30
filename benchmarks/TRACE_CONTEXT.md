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
