# Universal UI benchmark contract

This benchmark suite exists to measure Nulang's universal application framework against equivalent implementations without changing methodology to favor a particular framework.

## Workloads

### 1. Static page

A route with no client interactivity. Record HTML bytes, transferred JavaScript, total transferred bytes, server render time, and build time.

Expected Nulang invariant: no framework JavaScript is required when no interactive binding exists.

### 2. Targeted reactive updates

Render 1,000 independently addressable text values. Update one value per iteration for 1,000 iterations.

Record:

- median and p95 update latency
- client CPU time
- peak and settled memory
- DOM nodes touched per update when measurable

Expected Nulang invariant: a leaf signal update does not require a component-tree walk.

### 3. Typed navigation

Navigate list -> detail -> list across 1,000 generated records. Record route transition latency, bytes transferred, and client CPU.

### 4. Resource revalidation

Warm-start from local resource state, render immediately, then revalidate remotely. Record time to local paint, revalidation latency, and number of network requests.

### 5. Mutation and idempotency

Submit the same logical mutation twice with the same idempotency key. Record action latency and verify one committed domain mutation.

### 6. Offline synchronization

Start online, disconnect, mutate locally, restart the process, reconnect, synchronize, and verify convergence. Record queued bytes, recovery latency, synchronization latency, and conflict outcome.

### 7. Mobile startup and idle

For iOS and Android reference hosts, record cold startup, warm startup, idle resident memory, package/framework size, and memory after the reactive-update workload.

## Comparators

Maintain equivalent task applications for:

- Nulang
- React
- SvelteKit

Comparator applications should implement the same data, UI count, navigation behavior, and workload. Avoid framework-specific optimizations that materially change application semantics unless an equivalent optimization is available to every implementation.

## Environment capture

Every benchmark result must include:

- git commit SHA
- OS and version
- CPU architecture/model
- memory
- browser and version
- Rust toolchain
- Node/package manager versions for comparator apps
- build mode
- iteration count
- warmup count

Do not compare results gathered on materially different machines.

## Results format

Store machine-readable output in `results/<commit>/<platform>.json` with this shape:

```json
{
  "schema": "nulang-benchmark/1",
  "commit": "<sha>",
  "environment": {},
  "workloads": {
    "static": {},
    "reactive_updates": {},
    "navigation": {},
    "resource_revalidation": {},
    "mutation_idempotency": {},
    "offline_sync": {},
    "mobile_startup": {}
  }
}
```

## Reporting rules

- Report median, p95, sample count, and dispersion where meaningful.
- Preserve raw measurements when practical.
- Failed correctness assertions invalidate the performance run.
- Do not publish a single composite score.
- Prefer per-workload tradeoffs over claims that one framework is universally faster.
- Performance regressions above the accepted threshold should be explainable before release.

## Initial regression thresholds

Until stable baselines exist, flag rather than fail on:

- >10% regression in median latency
- >15% regression in p95 latency
- >10% increase in transferred framework bytes
- >10% increase in idle memory
- any increase from zero framework JS on a static Nulang page

Once variance is understood, convert stable thresholds into CI gates.