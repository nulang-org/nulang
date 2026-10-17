# Typed durable actors, execution modes, AI authority: implementation proof plan

Status: **design and validation plan, not production-complete implementation**.
Date: 2026-10-14.
Preserve [semantic stabilization](SEMANTIC_STABILIZATION_CONTRACT.md), [atomic durable transitions](../RFC/0022-atomic-durable-transitions.md), [orthogonal execution](../RFC/0024-orthogonal-execution-model.md), and the [effect guarantees](DURABILITY_GUARANTEES.md).

## Core design: one compiler-owned semantic contract

Do not invent a second actor-state identity, durability state machine, authorization model, or Cloud source parser.

A typed turn conceptually maps (State, Message) to (NextState, Events, Outbox, EffectIntents, Timers). This is a *logical contract*, **not new syntax**. The compiler already owns protocol/state/effect/authority and semantic/artifact identities. The host must enforce their emitted canonical manifests, not reinterpret source.

### Typed durable actor turns

- Typecheck declared actor state and incoming message/protocol with effect rows, sendability, and authority requirements at compile time.
- Before invoking the handler, reject unknown protocol revisions and mismatched durable identities without acknowledging inbox messages as successfully processed.
- For durable turns, commit inbox deduplication, sequence, state/event changes, effect intents, outbox, timers, and activation fence in the existing atomic-transition store. No provider calls before persisted intent.
- Derive external effect IDs from compiler-owned effect site plus stable execution turn and occurrence; bind a request digest, receipt and explicit delivery semantics.
- Recover with the exact retained artifact or a complete pure versioned migration chain. An unknown artifact, ambiguous schema, missing event, gap, stale fence or impure migration must fail closed *without mutation or external dispatch*.
- Never claim external exactly-once execution for a provider without deduplication/transactional participation.

**Acceptance:** Counter v1 to v2 with two effects per turn, duplicate typed messages, mismatched protocol revisions and restarts at pre-intent, post-intent, post-provider, post-receipt and pre-ack crash points. Assert state/sequence continuity, no stale commits, and no duplicate completed logical effects. Prove on an atomic disk store and on Cloud's real crash/adoption boundary.

### Unified transient and durable execution

Follow RFC 0024 execution/persistence orthogonality. Ordinary transient actors continue through in-memory mailboxes and the fast scheduler with **zero persistence-store calls, WAL, fsync or schema-migration work**. Durable actors opt into commit/replay adapters at the turn boundary. Workflows remain durable structured computation that may be actor-hosted rather than a new actor species.

For cross-mode sends, document delivery precisely: a transient sender offers no durable handoff guarantee; a committed durable sender uses an outbox for guaranteed eventual delivery. If no atomic store is available, reject durable admission instead of silently degrading to snapshots.

**Acceptance:** identical actor calculation in both modes produces the same result; transient path performs zero persistence calls, including under high contention; durable path survives destructive restarts; measure dispatch latency and allocation separately.

### Capability-secure AI agent boundaries

Pony-inspired reference capabilities determine sharing/aliasing; they do **not** grant external authority. Compiler manifests declare effects/authority requirements; Cloud intersects requested authorities with deployment/tenant policy, denies ungranted effects, and checks the actor/execution/resource scope **at every actual host effect and tool dispatch**. Keep credentials in the trusted host, not actor state or model output. Enforce token/cost/tool-count ceilings with in-flight reservations and revalidate after crash, resume or revocation. Avoid restart storms when an agent is out of budget. Tool use must be audit-linked to semantic effect IDs and receipts.

**Acceptance:** forged model tool, expired/revoked authority after suspension, cross-tenant operation, manifest drift, budget racing, process crash after provider mutation, and stale activation tests all prove no unauthorized host dispatch across bytecode and WASM.

### Evidence-based performance

The existing [cross-runtime harness](../benchmarks/cross_runtime/README.md) covers Nulang, Rust std channels, Go and Erlang/BEAM. It currently **does not include Pony**, durable recovery, or peak RSS. This branch first prevents accepting malformed/duplicate/wrong-message-count/zero-timing records.

Follow-ons: add opt-in pinned Pony fixtures for the same four workloads; report p50/p95/p99, peak and steady RSS, allocations, actor density and raw samples alongside throughput. Keep durable recovery as a separate benchmark: effect intent, provider commit, crash, restart, reconciliation, exactly one observable logical result, and measured recovery latency. Use same-host, pinned-CPU, alternating paired runs; disclose differences in producer concurrency and scheduler semantics. No blanket speed rankings from shared CI runners.

### Developer experience

Borrow Gleam's coherent new -> check -> run -> test -> format -> package -> deploy path, strict syntax predictability and precise diagnostics. Test CLI/LSP/playground equivalence of error code, range, explanation and suggested fix. Prioritize record-field go-to-definition, references and rename, import rewrite, missing-pattern code actions and migration/effect/authority explain views. Build from pinned lockfiles and make offline failures diagnostic.

## Competitor lessons

| Source | Borrow | Avoid |
|---|---|---|
| [Gleam LSP 1.18](https://gleam.run/news/a-field-day-for-gleams-language-server/) | Record-field rename/references, refactor code actions, concise errors and predictable pipe semantics | Another target backend added primarily for parity |
| [Pony reference capabilities](https://tutorial.ponylang.io/reference-capabilities/reference-capabilities) and [ORCA](https://tutorial.ponylang.io/runtime-basics/garbage-collection.html) | Sendability as compile-time invariant, per-actor isolation, actor-specific GC/tracing and latency tests | Confusing reference qualifiers with permission to invoke external tools |
| [Unison identity model](https://www.unison-lang.org/docs/language-reference/hashes/) and [Cloud](https://www.unison-lang.org/blog/unison-cloud-open-source/) | Stable definition identity, retained historical executable compatibility, local/host semantics parity, typed dependency inspection, small distributed service packaging | Replacing Git/text editing with a second proprietary-like codebase manager |

Unison Cloud's Nimbus worker, API server, client interpreter and UI were open-sourced under MIT on October 8, 2026. Review them as reference implementations, not mandatory platform dependencies.

## Recommended PR / gate order

1. Restore reliable exact-head executable CI. Land existing P0 nominal protocol, schema identity, migration and authority stacks before creating new abstractions.
2. Add a **single end-to-end typed durable turn** with atomic state/inbox/effect/outbox commit, deterministic crash injection and persisted schema migration.
3. Enforce canonical manifest authority and recovery semantics at real Nulang Cloud WASM/KVM/effect-provider boundaries.
4. Prove no persistence overhead on transient actors and release matched performance results.
5. Add Pony fixture, RSS and durable-recovery datasets; ship the CLI/LSP/package/deploy DX vertical slice.

**Claim boundary:** This file specifies unfinished proof gates. Existing unit contracts and benchmark parser safeguards do not certify general production durability, agent isolation or a performance win.
