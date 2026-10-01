# Nulang Product Requirements Document
## Strategic Repositioning: A Durable Computation Language for Long-Lived Software Entities

**Version:** 0.1 — Draft  
**Date:** 2026-07-21  
**Status:** Historical strategic draft — superseded for current semantics  
**Author:** AI assistant review based on the July 2026 `nulang` repository and proposed strategic updates  

> **Historical document.** This PRD records the July 2026 repositioning proposal
> and is not the source of truth for current language semantics. RFC 0021
> superseded the original permanent Frozen-Core policy for pre-adoption source
> semantics, and RFC 0024 superseded the "entity as the universal unit of
> computation" framing with an orthogonal execution model: ordinary local
> computation, scoped tasks, and independently addressable actors are distinct
> execution forms, while durability, identity, placement, effects, reference
> capabilities, and external authority compose with those forms. Use
> `SPEC2.md`, `GOVERNANCE.md`, accepted RFCs, and
> `docs/IMPLEMENTATION_STATUS.md` for current behavior and status.

---

# 1. Vision and Philosophy

Nulang is a **durable computation language** for long-lived, distributed, stateful software entities. Its core purpose is to let programmers describe software that keeps running across crashes, restarts, node migrations, and organizational change. The unit of thought is not a function or a service, but an **entity**: a named identity that carries state, responds to messages, evolves over time, and persists by default.

This document proposes a strategic repositioning of Nulang. The current implementation has drifted toward "AI language" as its public identity: `agent` and `workflow` are first-class declarations, the AST has a dedicated `LlmAsk` opcode, and the README highlights "AI-native agents" as a headline feature. The proposed update is to invert that relationship. AI is a powerful **application** of durable computation, but it is not the foundation. The foundation is timeless primitives: actor, effect, state, message, time, identity, capability, and deterministic execution.

The repositioned vision:

> Nulang is the runtime language for software entities that must survive forever. AI agents are one kind of such entity. So are workflows, databases, ledgers, organizations, simulations, and contracts.

This shift matters for a 50–100+ year horizon because AI model APIs, GPU providers, cloud vendors, and orchestration platforms will change many times. A language frozen around today's AI surface will look dated and become hard to migrate. A language frozen around durable computation primitives can outlast any of those technologies.

---

# 2. Long-Term Goals

1. **Survivability by default.** Any Nulang entity that is not explicitly ephemeral must be able to resume after process, host, or datacenter failure without data loss.
2. **Semantic parity across environments.** The same source code has the same observable behavior in a single-process REPL, a local CLI binary, and the hosted cloud runtime.
3. **Timelessness over trend.** Prefer primitives that were meaningful in 1986 and will be meaningful in 2086: identity, state, message, causality, effect, capability.
4. **Local-first development.** A programmer can build durable entities on a laptop without cloud credentials, then deploy unchanged to a hosted platform.
5. **Frozen core, evolvable layers.** The language core is small and versioned; higher layers (AI, billing, multi-tenancy, infrastructure providers) evolve as libraries and cloud services.
6. **Verifiable semantics.** The language should be specifiable enough that multiple independent runtimes (Rust reference, WASM, future bootstrap compiler) can pass the same conformance suite.

---

# 3. Language Principles

1. **Entities, not services.** The primary abstraction is a long-lived entity with identity, state, and a message interface. Services are compositions of entities.
2. **Explicit effects.** Every side effect (I/O, storage, time, messaging, AI inference) is declared in the type system. Pure code can be reasoned about locally.
3. **Capabilities as permissions.** Every reference carries a capability that constrains how it may be read, written, aliased, or sent across a message boundary.
4. **Deterministic replay.** Entity state can be reconstructed from an event journal plus the entity's code. The runtime may optimize with snapshots, but the semantics are event-first.
5. **Composition over frameworks.** There is no hidden framework runtime. Databases, AI models, workflow engines, and billing systems are user-space libraries or cloud services built on the same primitives.
6. **Failure as a first-class concern.** Crashes are normal. Entities are supervised, messages are monitored, and compensation is a language construct, not an afterthought.

---

# 4. Frozen Core Specification

The Frozen Core is the minimal subset of Nulang that must remain unchanged across major revisions. It is intentionally tiny. It is defined by [RFC 0002: Frozen Core](./../RFC/0002-frozen-core.md) and already implemented in the current repository.

## 4.1 What is in the Frozen Core

- Expressions: `fn`, `let` / `let rec`, `if`/`then`/`else`, `match`, closures, `return`, blocks.
- Values: `Int`, `Bool`, `String`, `Unit`, `Nil`; tuples; records; arrays; enum variants.
- Types: the same set as values, plus function types, tuples, records, variants, and `Vec<T>` / `Map<K,V>`.
- Effects: `IO.print` and `IO.read` only.
- Capabilities: `val` only (immutable, sendable).
- Declarations: top-level `fn`, `let`, `type`, `effect`.

## 4.2 What is intentionally NOT in the Frozen Core

- Actors, spawn, send, receive.
- Persistence, event sourcing, snapshots, replay.
- Effects other than terminal I/O.
- Capabilities other than `val`.
- Distribution, networking, CRDTs.
- FFI, Python interop, WASM.
- AI/LLM, agents, workflows, tools.
- JIT, native/AOT compilation, the bytecode VM itself.

This is the correct design. Core programs are pure, sequential, and immortal in semantics. Everything concurrent, distributed, durable, or AI-related belongs in higher tiers where it can evolve without destabilizing the language kernel.

## 4.3 Proposed change: keep Core frozen, do not expand it

A temptation of this repositioning is to elevate actors, effects, and state into the Frozen Core because they feel timeless. That would be a mistake. The Frozen Core must remain small enough that a bootstrap compiler, a formal proof, and a hostile runtime can all implement it in a single file. Actors and durability are **Stable-tier** features: their semantics are versioned, frozen between major versions, and governed by RFC, but they are not part of the Core.

---

# 5. Stable Language Features

Stable features are versioned semantics that the language guarantees for a major version. They may change only through an RFC and a deprecation cycle per [GOVERNANCE.md](./../GOVERNANCE.md). The following features should be Stable in the repositioned Nulang.

## 5.1 Actor and entity model

- `actor` and `persistent actor` declarations.
- `behavior` definitions inside actors.
- `spawn`, `send`, `ask`, `receive`.
- State models: `local`, `durable`, `event_sourced`, `crdt`.
- Entity-oriented extensions: `entity` as a durable actor alias; `event` for domain events; `goal` for intent-driven computation.

## 5.2 Effect system

- `effect` declarations, `perform`, `handle`, `resume`.
- Effect rows in function types (`!{IO, Timer}`).
- Built-in Stable effects: `IO`, `Timer`, `Signal`, `Actor` (lifecycle), `Storage`.
- AI effects (`LLM`, `Vector`, `Tool`) are **not** Stable language effects; they are Cloud SDK libraries.

## 5.3 Capability system

- Capabilities: `iso`, `trn`, `ref`, `val`, `box`, `tag`, `lineariso`.
- Sendability rules.
- Capability annotations on function parameters and actor state.

## 5.4 Temporal primitives

- `Timer.sleep(name, ms)` as a language-level effect.
- `Signal.wait(name)` as a runtime effect.
- Proposed: `after ms => expr`, `until condition => expr`, `sleep_until(deadline)` as Stable syntactic sugar over `Timer`.

## 5.5 Identity and migration

- Stable actor identity (actor id / address) across restarts.
- Proposed: `migration` contracts for entity schema evolution.
- Proposed: `organization` primitives for composite multi-actor identities.

## 5.6 What moves from Stable to Experimental

Currently the language surface treats the following as Stable or implemented:

- `agent` declarations.
- `workflow` declarations.
- `database` declarations.
- `LlmAsk` bytecode opcode (`0x9C`).
- Agent memory/pricing/retry/fallback structs.
- `perform LLM.ask(...)` as a built-in effect.

These should be reclassified as **Experimental** and eventually removed from the language surface, reimplemented as Cloud SDK / standard-library packages (`nlc.ai`, `nlc.workflow`, `nlc.vector`, etc.). The AST contains `Decl::Agent`, `Decl::Workflow`, `Decl::Database`, and the bytecode has `LlmAsk`; those are evidence of the drift this PRD intends to reverse.

---

# 6. Experimental Feature Process

Experimental features are not covered by backwards-compatibility guarantees. They live behind explicit opt-in (language version, feature flag, or import) and may be removed without a deprecation cycle.

## 6.1 How Experimental features enter

1. An RFC describes the problem, proposed syntax/semantics, and a path to Stable or removal.
2. The feature is implemented behind a flag or in an optional library.
3. A conformance test suite is added.
4. After real-world use, the RFC is amended to either promote the feature to Stable or schedule removal.

## 6.2 Current Experimental candidates

- `agent`, `workflow`, `database` language keywords.
- `LLM.ask` opcode and stdlib effect.
- AI-specific memory, pricing, tool schemas.
- `Pipeline`, `Supervisor`, `Debate` orchestration builtins.
- Proposed new primitives (`entity`, `event`, `goal`, `after`/`until`, `organization`, `migration`) start as Experimental RFCs.

---

# 7. Type System

Nulang uses Hindley-Milner type inference (Algorithm W) with extensions for effect rows and reference capabilities. The current implementation (`src/typechecker.rs`) already supports:

- Tuples, records, variants, arrays.
- Function types carrying effect rows.
- `&cap T` reference types.
- Row-polymorphic records: `fn(r) r.x + r.y` accepts any record with fields `x` and `y`.
- Closed record annotations require exact field matches.

## 7.1 Requirements for the repositioned language

1. Keep HM inference as the Core type system.
2. Make effect rows explicit in function signatures.
3. Preserve row polymorphism for records; add similar row polymorphism for effect rows.
4. Add Stable types for entity identity, event journals, and migration descriptors.
5. Do not add AI-specific types to the language. Types for prompts, models, embeddings, and tools belong in `nlc.ai`.

---

# 8. Ownership and Capability Model

Nulang's capability system (`iso`, `trn`, `ref`, `val`, `box`, `tag`, `lineariso`) governs how references may be read, written, aliased, and sent across actor boundaries. Capabilities are checked at compile time and erased at runtime.

## 8.1 Current state

The capability analyzer lives in `src/effect_checker.rs`. `lineariso` enforces at-most-once consumption. Sendable capabilities (`lineariso`, `iso`, `val`, `tag`) are required for message arguments.

## 8.2 Repositioning requirements

1. Keep the capability lattice Stable.
2. Use capabilities to enforce that durable state is serializable and migration-safe.
3. Introduce an `entity` capability or authority pattern so an entity can prove its own identity and delegate access.
4. Keep AI/LLM capabilities (model access, token budgets) out of the language surface; they are library/cloud concerns.

---

# 9. Effect System

Algebraic effects are Nulang's mechanism for declaring, composing, and handling side effects. The current runtime resolves effects via a handler stack (`Handle`, `Perform`, `Resume`, `Unwind`).

## 9.1 Stable effect tiers

- **Core:** `IO.print`, `IO.read`.
- **Stable:** `Timer.sleep`, `Signal.wait`, `Actor.link`, `Actor.monitor`, `Actor.spawn`, `Actor.send`, `Actor.receive`, `Storage.read`, `Storage.write`.
- **Library / Cloud SDK:** `LLM.ask`, `Vector.search`, `Workflow.step`, `Billing.meter`, `Identity.authenticate`.

## 9.2 Effect system requirements

1. Effects must remain deep resumable via continuations.
2. Effect rows must remain explicit in function types.
3. AI effects must leave the language; they can be implemented as ordinary effect handlers in `nlc.ai`.
4. Temporal effects must be first-class Stable.

---

# 10. Actor Model

Actors are the fundamental unit of concurrent and durable computation in Nulang. The current runtime (`src/runtime/`) already implements spawn, send, ask, receive, links, monitors, supervision, process groups, and actor priority.

## 10.1 Current state

- Actors are lightweight, each with a mailbox and heap.
- Messages are delivered asynchronously and processed sequentially per actor.
- Selective `receive` with `after` timeouts is implemented.
- Supervision trees and OTP-style fault tolerance exist.
- Distribution is location-transparent.

## 10.2 Requirements

1. Keep actors as the Stable concurrency primitive.
2. Add `entity` as a durable actor alias with stronger identity and migration semantics.
3. Support event-sourced actors where state is fully reconstructible from an event journal.
4. Allow actors to expose typed message interfaces (behavior signatures) that can be verified at compile time.
5. Remove `agent` as a special actor kind; an agent is just an actor that imports `nlc.ai`.

---

# 11. Concurrency Model

Nulang uses cooperative, single-threaded-per-actor concurrency. The runtime scheduler (`src/runtime/scheduler.rs`) is a work-stealing deque system with per-actor heaps and a global injector split by priority (High, Normal, Low).

## 11.1 Requirements

1. Preserve the single-threaded illusion within each actor.
2. Preserve FIFO mailbox semantics.
3. Preserve priority scheduling but ensure it cannot starve lower-priority actors indefinitely.
4. Keep all heap/GC access on the scheduler thread; no cross-thread heap access without restored atomics.
5. Allow local/cloud semantic parity: the same `spawn`/`send`/`receive` code runs in a single process and across a cluster.

---

# 12. Durable State Semantics

Durability means that an entity's state survives crashes and restarts. The current runtime supports four state models (`local`, `durable`, `event_sourced`, `crdt`) via persistence backends (memory, JSON file, SQLite).

## 12.1 State models

| Model | Semantics |
|-------|-----------|
| `local` | Ephemeral state, lost on restart. |
| `durable` | State is snapshotted and restored. |
| `event_sourced` | State is a left fold over an append-only event journal. |
| `crdt` | State converges under partition and merge. |

## 12.2 Requirements

1. Make `durable` and `event_sourced` the default for long-lived entities; require explicit `local` to opt out.
2. Define deterministic replay semantics: the same journal plus the same code must always produce the same state.
3. Add a `migration` construct for schema evolution that preserves journal compatibility.
4. Keep CRDTs as a Stable primitive for distributed shared state.
5. Do not bake database schemas or SQL into the language; those belong in `nlc.storage` or user libraries.

---

# 13. Determinism Guarantees

Determinism is essential for replay, testing, and verification.

## 13.1 What must be deterministic

- Pure Core computation.
- Actor behavior execution in response to a given message and state.
- Event-sourced state reconstruction from a fixed journal.
- Effect handler dispatch order within a single actor turn.

## 13.2 What is intentionally non-deterministic

- Message arrival order across actors.
- Network latency and partition behavior.
- External AI model outputs.
- Clock readings outside explicit `Timer` effects.

## 13.3 Requirements

1. Provide a deterministic simulation mode for testing concurrent entities.
2. Randomness, if introduced later, must be explicit and seeded (`Random` effect, not a global RNG).
3. AI outputs must be modeled as non-deterministic effects so tests can mock them deterministically.

---

# 14. Error Handling

Nulang uses a single `NuError` enum for compile-time errors and runtime errors. Compile-time variants carry spans; runtime variants carry strings.

## 14.1 Requirements

1. Keep compile-time errors span-aware and fail-fast.
2. Add typed failure channels for actor behaviors (e.g., `Result[T, E]` returns from `ask`).
3. Make workflow compensation a first-class error-handling mechanism.
4. Ensure AI failures (model unavailable, token limit, tool error) are handled in library code, not language-level exceptions.

---

# 15. Modules and Packages

Nulang supports `module` and `import` declarations. The package manager (`src/package/`) is an MVP supporting local-path and git dependencies.

## 15.1 Requirements

1. Keep modules and packages Stable.
2. Define a clear boundary between language-owned namespaces and Cloud SDK namespaces:
   - Language/Stdlib: `Core`, `List`, `Map`, `Set`, `String`, `Json`, `Time`, `Concurrent`, `Actor`, `Storage`, `Crypto`.
   - Cloud SDK: `nlc.ai`, `nlc.workflow`, `nlc.vector`, `nlc.billing`, `nlc.identity`, `nlc.marketplace`.
3. Allow the package manager to resolve optional cloud features without requiring them in the language core.
4. Support workspace-local and git dependencies for now; a registry is a future Cloud Platform concern.

---

# 16. Standard Library Design

The standard library should contain timeless utilities and runtime-facing modules. It should not contain AI, billing, or cloud-specific APIs.

## 16.1 Proposed Stable stdlib modules

- `Core` — language primitives (`Option`, `Result`, `Tuple`, `Unit`).
- `List`, `Map`, `Set`, `String` — persistent collections.
- `Json` — JSON parsing and serialization.
- `Time` — timestamps, durations, monotonic clocks, deadlines.
- `Concurrent` — actor-local concurrency primitives if any (most concurrency is actor-based).
- `Storage` — durable key/value and journal effects.
- `Crypto` — hashing, signatures, deterministic identifiers.
- `Network` — message passing abstractions over actors.

## 16.2 What must leave stdlib

- `LLM.ask` and AI builtins.
- `Pipeline`, `Supervisor`, `Debate` orchestration builtins.
- Any pricing, model, or memory-config types.

These become Cloud SDK packages, not stdlib modules.

---

# 17. Compiler Architecture

The compiler pipeline is MIR-exclusive: source → lexer → parser → AST → typechecker → effect checker → capability analyzer → HIR → MIR → backend.

## 17.1 Current state

- Lexer (`src/lexer.rs`) and parser (`src/parser.rs`) produce the AST.
- Typechecker (`src/typechecker.rs`) does HM inference.
- Effect checker / capability analyzer (`src/effect_checker.rs`) validates effects and capabilities.
- HIR lower (`src/hir_lower.rs`) and MIR lower (`src/mir_lower.rs`) produce MIR.
- Backends: bytecode (`src/mir_codegen.rs`), native AOT (`src/aot/codegen.rs`), WASM (`src/mir_wasm.rs`).

## 17.2 Requirements

1. Keep the pipeline Stable.
2. Remove AI-specific AST nodes (`Decl::Agent`, `Decl::Workflow`, `Decl::Database`) and the `LlmAsk` opcode from the compiler frontends and bytecode.
3. Replace them with library implementations that use Stable actor/effect primitives.
4. Keep the bootstrap compiler path (`bootstrap/compiler_core.nula`) targeted at the Frozen Core only.
5. Add a conformance test mode that compiles the same source through multiple backends and compares observable output.

---

# 18. VM Architecture

The reference VM (`src/vm.rs`) is a register-based interpreter with NaN-tagged 64-bit values, 256-register frames, and an effect handler stack. It supports JIT tiering via Cranelift.

## 18.1 Requirements

1. Keep the VM architecture Stable.
2. Remove the `LlmAsk` opcode from the bytecode ISA; replace it with ordinary effect dispatch to a runtime-provided handler.
3. Ensure that bytecode compiled today can run on future runtimes via the `.nbc` format and migration chain defined in [RFC 0001: Format Stability](./../RFC/0001-format-stability.md).
4. Keep per-actor heaps and ORCA GC.

---

# 19. WASM Backend

The WASM backend (`src/mir_wasm.rs`, `src/wasm_runtime.rs`) compiles MIR to WebAssembly and runs it with Wasmtime. It is gated behind `--features wasm-backend`.

## 19.1 Requirements

1. Keep the WASM backend Stable and the primary Cloud deployment target.
2. Ensure that AI/LLM/cloud effects are implemented as Wasmtime host imports, not as language opcodes.
3. Use the component model for Cloud SDK libraries so they can be linked dynamically.
4. Maintain the same durable actor semantics in WASM as in the native bytecode runtime.

---

# 20. Native Backend Roadmap

The native/AOT backend (`src/aot/`) compiles MIR to Cranelift CLIF and then to native object code.

## 20.1 Requirements

1. Keep the native backend as an opt-in, performance-oriented target.
2. Do not make native code the only way to access durable semantics; Cloud deployments should prefer WASM.
3. Ensure type metadata and unboxed operations remain compatible between JIT and AOT paths.

---

# 21. Runtime API

The runtime API is the interface between compiled code and the host. It is defined by effect declarations and runtime callbacks, not by language keywords.

## 21.1 Requirements

1. Define a Stable runtime API for actor lifecycle, messaging, timers, signals, storage, and CRDTs.
2. Define an optional Cloud SDK runtime API for AI, vector search, billing, identity, and workflows.
3. Keep the VM→runtime callback traits (`ActorVmCallbacks`, `DistributedVmCallbacks`) as the abstraction boundary.
4. Ensure the Cloud SDK API can be implemented by both the in-process local emulator and the hosted cloud runtime without language changes.

---

# 22. Testing Strategy

Nulang already has 1362 tests (1368 with `wasm-backend`). The repositioning adds a new requirement: conformance across implementations.

## 22.1 Requirements

1. Keep the existing unit, integration, and stress test suites passing.
2. Add a conformance suite that runs the same durable-entity programs on the native VM, the WASM backend, and the local cloud emulator.
3. Add deterministic simulation tests for concurrent entities.
4. Add migration tests that verify event journals remain readable after entity schema changes.
5. Remove or re-home tests that depend on `agent` / `workflow` / `LLM.ask` as language features.

---

# 23. Package Manager

The package manager (`nula`) currently supports `new`, `build`, `build-wasm`, `test`, and `run`, with local-path and git dependencies.

## 23.1 Requirements

1. Keep the package manager Stable.
2. Add support for optional Cloud SDK dependencies that are only resolved when `--features cloud` or similar is enabled.
3. Ensure a package can declare that it does not depend on AI/cloud features and therefore builds with the language core only.
4. Support lockfiles that pin optional feature dependencies independently.

---

# 24. LSP and Tooling

The LSP server (`src/lsp/mod.rs`) currently provides hover, goto definition, references, document symbols, rename, signature help, formatting, semantic tokens, code actions, inlay hints, completion, and diagnostics.

## 24.1 Requirements

1. Keep all 12 LSP features Stable.
2. Update diagnostics, hover, and completion so that AI/cloud symbols are shown as coming from `nlc.*` packages, not as language builtins.
3. Add inlay hints for entity identity, state model, and effect row annotations.
4. Add a code action to convert a deprecated `agent` declaration into an actor that imports `nlc.ai`.

---

# 25. Formatting and Linting

Nulang does not currently ship a standalone formatter. This is an opportunity.

## 25.1 Requirements

1. Define a canonical formatter as a Stable tool.
2. Add lints for:
   - Non-durable state in long-lived entities without explicit `local` annotation.
   - AI/cloud effects performed without importing the relevant Cloud SDK package.
   - Deprecated `agent` / `workflow` / `database` keywords.
3. Keep formatting and linting decoupled from compilation.

---

# 26. Versioning

Nulang uses language versions distinct from crate versions. The `.nbc` artifact records a `language_version` that is checked by the runtime.

## 26.1 Requirements

1. Keep the language version frozen for Core; it moves only on RFC-ratified change.
2. Use major-version bumps for Stable-tier breaking changes (actor semantics, effect names, state models).
3. Use minor-version bumps for additive Stable changes (new stdlib modules, new non-breaking syntax).
4. Experimental features do not participate in semver guarantees.

---

# 27. Governance

Nulang governance is described in [GOVERNANCE.md](./../GOVERNANCE.md). It uses Frozen/Stable/Experimental tiers and an RFC process.

## 27.1 Requirements

1. Route all language-surface changes through the RFC process.
2. Require a new major language version for any Frozen Core change.
3. Require an RFC and deprecation cycle for any Stable change that removes or alters behavior.
4. Allow Experimental features to be added and removed with lightweight RFCs.
5. Establish a "Nulang Constitution" document that records the timeless principles (this PRD's Vision, Principles, and Core) and is harder to amend than ordinary RFCs.

---

# 28. Performance Goals

## 28.1 Entity lifecycle

- Entity spawn in <1 ms in local bytecode VM.
- Entity spawn in <50 ms from cold in WASM cloud runtime.

## 28.2 Message latency

- In-process message send/receive: <1 µs.
- Cross-node message: <1 ms on same datacenter, <5 ms on most paths.

## 28.3 Durability

- Durable write acknowledged in <10 ms for local SQLite backend.
- Snapshot creation overhead <5% of actor turn time.

## 28.4 AI workloads

- AI inference latency is dominated by model providers, not Nulang overhead.
- Nulang runtime overhead per LLM request must be <1 ms.

---

# 29. Security Model

## 29.1 Capabilities

The capability system is the primary compile-time security boundary. It prevents mutable references from escaping actors and ensures only sendable values cross message boundaries.

## 29.2 Sandboxing

- The WASM runtime runs inside Wasmtime with guard pages and capability-based host imports.
- The cloud runtime isolates tenants in microVMs with a minimal syscall whitelist.

## 29.3 Identity and audit

- Every entity has a stable identity.
- Every durable action can be logged to a tamper-evident audit trail (Cloud SDK / Cloud Platform feature).

## 29.4 AI safety

- AI model access is gated by a capability and a policy layer in `nlc.ai`, not by the language.
- Tool execution is mediated by typed effect handlers.
- Token budgets and cost circuit breakers are platform policies, not language semantics.

---

# 30. Backwards Compatibility Guarantees

1. **Frozen Core:** Every Core program valid today is valid in every future version.
2. **Stable features:** Backwards-compatible within a major version; breaking changes require a new major version and a deprecation cycle.
3. **Experimental features:** No compatibility guarantee.
4. **Bytecode artifacts:** `.nbc` format is versioned and migratable per RFC 0001.
5. **AI/Agent/Workflow surface:** The current language-level `agent` / `workflow` / `database` declarations and `LlmAsk` opcode are deprecated as of the next major version and will be removed in the following major version. Migration path: rewrite as actors importing `nlc.ai` / `nlc.workflow`.

---

# 31. Roadmap

## Phase 1: Repositioning documentation (0–4 weeks)

- Adopt this PRD.
- Update `README.md` and `SPEC2.md` forward/introduction to position Nulang as a durable computation language, with AI as one application.
- Add a `docs/PHILOSOPHY.md` or amend `SPEC2.md` §1.2 to de-emphasize "Language-integrated AI" and emphasize durable computation.

## Phase 2: Deprecation RFCs (4–12 weeks)

- RFC to deprecate `agent` language keyword.
- RFC to deprecate `workflow` language keyword.
- RFC to deprecate `database` language keyword.
- RFC to remove `LlmAsk` opcode and `LLM.ask` stdlib effect.
- Implement deprecation warnings in compiler, LSP, and linting.

## Phase 3: New Stable primitives (12–24 weeks)

- RFC and implementation for `entity` as a durable actor alias.
- RFC and implementation for `event` and event-sourced entities.
- RFC and implementation for temporal syntax: `after`, `until`, `sleep_until`.
- RFC and implementation for `migration` contracts.
- RFC and implementation for `organization` primitives.

## Phase 4: Cloud SDK extraction (24–36 weeks)

- Create `nlc.ai` package with `agent`, memory, tools, pipelines, supervisors, debates.
- Create `nlc.workflow` package with durable workflow engine.
- Create `nlc.vector`, `nlc.billing`, `nlc.identity` packages.
- Reimplement current `Decl::Agent`, `Decl::Workflow`, `Decl::Database` lowering as macros or code-generation against the new Cloud SDK.

## Phase 5: Conformance and multi-implementation (36–48 weeks)

- Conformance suite across native VM, WASM backend, and local cloud emulator.
- Bootstrap compiler milestone: Core self-hosts.
- Documentation, examples, and migration guide.

---

# 32. Non-Goals

This PRD explicitly does **not** commit to the following:

1. **AI as the core identity.** AI remains an important use case, implemented through libraries and cloud services, not as a language primitive.
2. **Kubernetes replacement.** Nulang Cloud may run on Kubernetes or bare metal; that is a platform decision, not a language feature.
3. **General-purpose language competition.** Nulang is not trying to be the best language for every program. It targets durable, stateful, long-lived entities.
4. **GPU-native language semantics.** GPUs are hardware accelerators accessed through cloud libraries; they do not appear in the type system or Frozen Core.
5. **Multi-tenant billing in the language.** Billing, metering, and cost controls are Cloud Platform policies.
6. **Specific cloud vendor lock-in.** The language and runtime must be deployable on any infrastructure that provides WASM and durable storage.
7. **Workflow as a language keyword.** Workflows are compositions of durable actors and effects; they live in `nlc.workflow`.
8. **Database schema as a language keyword.** Schemas, SQL, and indexing are library concerns.

---

# 33. Appendix: Layer Separation

This appendix restates the five-layer architecture that the repositioned Nulang should expose to users.

## 33.1 Nulang Language

- Frozen Core: pure sequential computation.
- Stable surface: actors, effects, capabilities, durable state, time, identity, modules, types.
- Governance: RFC process, Frozen/Stable/Experimental tiers.

## 33.2 Nulang Runtime

- Bytecode VM with register frames and NaN-tagged values.
- JIT tiering via Cranelift.
- Actor scheduler, per-actor heap, ORCA GC, cycle detector.
- Persistence backends (memory, JSON, SQLite, future libsql/Turso).
- Distribution layer: NUL0 wire protocol, gossip membership, CRDTs, remote spawn.

## 33.3 Nulang Standard Library

- `Core`, `List`, `Map`, `Set`, `String`, `Json`, `Time`, `Concurrent`, `Storage`, `Crypto`, `Network`.
- No AI, billing, cloud, or workflow-specific modules.

## 33.4 Nulang Cloud SDK

Optional packages that build on the language and runtime:

- `nlc.ai` — LLM clients, memory, tools, pipelines, supervisors, debates.
- `nlc.workflow` — durable workflow engine, saga compensation, human-in-the-loop.
- `nlc.vector` — vector search and embeddings.
- `nlc.billing` — usage metering and pricing helpers.
- `nlc.identity` — authentication, authorization, RBAC.
- `nlc.marketplace` — agent and workflow templates.

These packages are versioned independently and may include Cloud Platform-specific extensions.

## 33.5 Nulang Cloud Platform

The hosted service that runs Nulang entities at scale. Public architecture claims only:

- Compiles Nulang to WebAssembly and executes it in sandboxed runtimes.
- Provides durable actors, timers, workflows, and messaging across nodes.
- Offers AI, vector, billing, identity, and observability services as host effects.
- Uses standard protocols (WebSocket, NATS, WireGuard) and isolates tenants.

Implementation details beyond these public claims (crate names, internal protocols, pricing formulas, infrastructure providers) are not part of this PRD.

---

# Conclusion

Nulang already has the right ingredients: a small Frozen Core, a sound effect and capability system, a mature actor runtime, durable state, distribution, and multiple backends. The strategic risk is that these ingredients are currently presented as an "AI language," with AI-specific surface baked into the AST, bytecode, and standard library.

This PRD recommends a clear separation:

- **Core stays frozen and tiny.**
- **Actors, effects, state, time, identity, and capabilities become the Stable surface.**
- **AI, workflows, databases, billing, identity, vector search, and multi-tenancy move to the Cloud SDK and Cloud Platform.**

The result is a language that can credibly claim to be the runtime for software entities that must survive forever — whether those entities are AI agents, financial ledgers, workflow orchestrators, organizations, simulations, or things we have not named yet.
