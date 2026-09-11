# Universal application vertical slice

Status: implementation gate for the Nulang universal web/mobile framework.

## Objective

Prove one complete application flow before expanding the framework surface area. The same domain flow must execute across web, iOS, and Android while preserving typed routing, resource semantics, action placement, offline behavior, and deterministic synchronization.

## Required flow

1. Compile one Nulang application with a typed route containing a path parameter and query state.
2. Resolve a resource through local state first, then revalidate against a remote source.
3. Render the resource through the semantic UI model.
4. On web, lower to DOM/CSS with leaf-level signal updates and no virtual DOM.
5. On iOS, render equivalent semantic intent using SwiftUI controls.
6. On Android, render equivalent semantic intent using Jetpack Compose controls.
7. Invoke a client action through the stable native action ABI.
8. Invoke a server action through the network/durable execution path; native hosts must not execute server actions locally.
9. Persist a local mutation while offline.
10. Reconnect, synchronize, resolve the mutation according to explicit policy, and propagate the resulting state to all active renderers.
11. Emit an end-to-end trace linking route, resource, action, sync operation, and resulting UI update.

## Architectural constraints

- Keep UI semantics outside the frozen language kernel unless a feature provably requires a language primitive.
- Web uses DOM/CSS directly. No framework-wide virtual DOM.
- iOS uses SwiftUI. Android uses Jetpack Compose.
- Do not introduce a JavaScript runtime or WebView for native mobile execution.
- The mobile runtime remains interpreter-only for this milestone; AOT is a later optimization behind the same runtime/host ABI.
- Storage, synchronization, analytics, and networking are capability interfaces, not hard dependencies on a specific database vendor.
- Client and server action placement is explicit and validated.
- Action requests carry correlation and idempotency metadata.
- Offline behavior is deterministic and testable; silent last-write-wins is not the default.

## Reference test application

Use a small task application because it exercises the framework without hiding problems behind domain complexity.

Data model:

- `Task { id, title, completed, updated_at }`
- list route
- task detail route with typed path parameter
- text filter represented as query state
- create/update/complete actions

Required scenarios:

- cold online start
- warm local start followed by background revalidation
- client-only filter update
- online server mutation
- offline mutation queued locally
- reconnect and successful synchronization
- deterministic conflict fixture
- navigation from list to detail and back
- process restart with local state recovery

## Conformance gates

### Compiler and shared model

- typed route links reject invalid path/query arguments at compile time
- illegal placement/effect combinations fail compilation
- semantic UI document is renderer-neutral
- action payload ABI round-trips form and signal data without lossy encoding

### Web

- static route can render with zero framework JavaScript when there are no interactive bindings
- a single signal change mutates only its dependent DOM leaf
- no full component-tree reconciliation occurs for leaf updates
- hydration/runtime payload is measured and recorded

### Apple

- `NulangMobile.xcframework` builds for device and simulator
- Swift host consumes the versioned UI protocol and incremental messages
- client actions execute off the main thread and UI result application returns to the main actor
- server actions are routed through application/network policy

### Android

- release AAR builds for all supported ABIs
- protocol module remains Kotlin/JVM-only
- JNI transports ordinary UTF-8 without modified-UTF-8 corruption
- runtime execution is serialized on the dedicated worker
- Compose state updates occur on the UI/main executor

### Offline and sync

- local mutation survives process termination
- retries are idempotent
- duplicate delivery cannot duplicate a mutation
- conflict behavior is covered by fixtures
- synchronization state is observable and recoverable

### Observability

Every run must be able to expose a causal chain containing:

`route -> resource -> local/remote resolution -> action -> effect/sync -> signal update -> renderer update`

## Performance budgets

These are initial guardrails, not marketing claims. Record actual measurements and revise only with evidence.

- static web framework JS: 0 bytes
- interactive runtime: <= 15 KiB compressed for the reference app, excluding application payload
- leaf signal update: no component-tree walk
- mobile runtime: no Cranelift, Wasmtime, or libloading dependency in the interpreter profile
- idle mobile memory, binary size, build time, route latency, action latency, and sync latency: recorded on every benchmark release

## Benchmark comparison

Compare equivalent task applications implemented in:

- Nulang universal UI
- React
- SvelteKit

Measure at minimum:

- transferred JavaScript
- total transferred bytes
- startup / time-to-interactive
- client CPU during 1,000 targeted updates
- memory after idle and after 1,000 updates
- production build time
- server-render latency
- native binary/AAR/XCFramework size where applicable
- mobile idle memory

Do not claim superiority unless the benchmark implementation and methodology are published and reproducible.

## Definition of done

This milestone is complete only when the vertical slice is merged into the repository, the full applied tree builds, platform conformance tests pass, benchmark results are committed, and the reference application demonstrates online/offline behavior on web, iOS, and Android.

Until this gate is complete, prioritize integration defects, runtime correctness, conformance, observability, and benchmark tooling over additional semantic UI primitives.