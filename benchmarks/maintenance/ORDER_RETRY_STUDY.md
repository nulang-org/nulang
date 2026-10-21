# Study 001 — Crash-safe order payment maintenance change

**Status: proposed experiment only.** No language has passed this study. This
is a falsifiable task contract, not an implementation or a benchmark result.

## Research question

After all three applications meet the same observable contract, does adding
crash-safe payment recovery require less engineer time, change amplification,
application-owned coordination, and operational maintenance in Nulang than
in idiomatic Go or Rust?

Do not confuse "Nulang runtime written in Rust" with a program authored in
Nulang. A Rust integration test of `DurableEffectCoordinator` is useful
runtime evidence but does not demonstrate source-level `.nula` ergonomics.

## Fixed environment and interfaces

Implement independently maintained applications in Nulang, Rust, and Go. Pin
all compilers, framework/library versions, storage backends, database schema,
OS/container image, and source revisions. Use one durable SQL store (or clearly
justify comparable substitutes) and the same **independent fake payment
provider**, which persists an operation-id -> result mapping across app crashes.
The provider must track requests separately from actual mutations.

The application exposes the same logical operations to the harness:

- `submit(order_id, cents, customer_id)` accepts a payment order; repeated
  identical requests are idempotent. Invalid or conflicting payloads fail
  without new provider mutations.
- `inspect(order_id)` reports durable order state and payment receipt.
- `restart` exits and relaunches the application without deleting durable
  storage or resetting the independent provider.
- A deterministic, explicit failpoint kills the application at each recovery
  boundary below; tests must not rely on lucky timing or sleeps.

Implementation-specific endpoint protocols may differ **only** through a small,
versioned test adapter. Publish those adapters and count their maintenance cost
separately; do not hide large amounts of application logic inside them.

## Baseline and mutation

1. **Baseline:** accept an order, validate amounts and customers, write a
   durable order record, and report a terminal state with a normal successful
   payment. Make the baseline pass the same valid-input tests in all three
   languages. Record each independent baseline SHA.
2. **Maintenance task:** add recovery-safe external payment using a stable,
   per-logical-operation idempotency key and persisted intent/receipt. A
   provider response may be lost after the provider has committed. The
   application must recover without a second logical payment. Record each
   changed candidate SHA and separately logged engineer time.
3. **Required regression matrix:**

   | Fault injected | Required externally observable behavior |
   |---|---|
   | Before intent persistence | No provider mutation; retry may begin a new attempt |
   | After intent, before provider dispatch | Same operation key on recovery; one provider mutation |
   | After provider commit, before receipt | Retry uses same operation key; provider may receive more than one call but records only one mutation |
   | After receipt, before resume | Replay saved receipt without redispatch |
   | Duplicate identical submit | Stable outcome; no second logical payment |
   | Same order ID with changed amount/customer | Fail closed without provider mutation |
   | Corrupt/missing stored receipt | Explicit error or validated recovery; never silently report success |

No program may claim to ensure exactly-once remote execution without an
external deduplication or equivalent query protocol. Measure actual provider
**mutation count** separately from **call count**.

## Acceptance and reporting

Before evaluating maintenance metrics, collect reproducible CI/test evidence
for every fault case, inspect persisted state after restart, and perform a
human review of differences in semantics. Record failure cases explicitly;
do not drop cases that one runtime cannot support. For each implementation,
run the study twice with separately named maintainers if possible; do not
assume time measurements from one author generalize to other developers.

Only then populate `scripts/maintenance_bench.py`'s JSON manifest, with a
stable `scenario` of `durable-order-payment` and `task_id` of
`idempotent-provider-recovery`. Commit hashes must name a real ancestor ->
descendant change in each repo, not broad release-to-release differences.

Include in the final study report (outside the diff-stat collector):
- first successful test time and total modification/debugging time;
- changed application code and configurations, excluding generated files;
- production deploy steps, sidecar services, and ongoing upgrade obligations;
- test failures/defects discovered after the change;
- operator recovery steps and state inspection/debugging effort;
- total cost of ownership, including any hosted workflow/runtime components.

**Nulang-specific release gate:** current `main` labels `workflow` source
semantics and external durable adapters experimental. Until the `.nula` path
passes these exact crash tests (not only the Rust coordinator integration
tests), mark the Nulang comparison **blocked/incomplete**, not superior or
inferior. Promotion requires source-to-runtime semantic evidence and the
production host adapter's proven idempotency integration.
