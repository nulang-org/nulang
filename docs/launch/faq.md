# Nulang FAQ (launch edition)

Every answer here is grounded in the repo. Where the honest answer is "not
yet," it says so. When a fact changes, update this file — stale FAQs are
worse than no FAQ.

## What is Nulang, in one sentence?

An actor-based language with Hindley-Milner type inference, Pony-style
reference capabilities, algebraic effects, and durable event-sourced actors —
"a typed, durable BEAM for the rest of us." (README.md, SPEC2.md)

## Is it production-ready?

No. It is alpha software with no external users, and says so in the README's
Project Status section. The current policy distinguishes Frozen published
compatibility contracts (for example versioned artifact/wire/value-layout
formats), Stable pre-adoption source semantics, and Experimental surfaces.
RFC 0021 explicitly moved away from treating all pre-adoption source semantics
as permanently Frozen. The pre-1.0 disclaimer still applies: expect breaking
changes before external validation closes the freeze gate.

## What's actually implemented vs. planned?

Implemented and tested: the full compiler pipeline (AST → HIR → MIR),
register-based bytecode VM, Cranelift JIT, supervision (one-for-one,
one-for-all, rest-for-one, simple-one-for-one), links/monitors/process
groups, algebraic effects with resume, HM inference, capabilities, persistent
actors with checkpointing + journaling through `PersistenceStore` (memory,
JSON-file, default-feature libSQL/SQLite, plus optional RocksDB and PostgreSQL
backends), `entity`/`events`/`apply`/`emit` event sourcing, the `nula`
package manager, LSP server, REPL, test runner, an extensive Rust test suite,
and a `.nula` conformance suite under `conformance/`.

Experimental (feature-gated or marked): multi-node distribution over TCP,
WASM/WASM Component Model/WasmFX backends, the secondary native AOT backend
(restricted semantics), AI runtime, Fabric, RESP cache serving, typed
actor-protocol hardening, the Experimental `agent`/`workflow`/`database`
declaration surfaces, and source-level CRDT state/replication.
`.nula` CRDT fields and `Crdt.*` operations are implemented; checkpoints
persist CRDT state plus field mappings, and recovery restores/re-registers
those mappings.

Actor protocol checking now rejects unknown behaviors and wrong arity when the
receiver is statically known, and explicit `ActorRef[P]` values enforce their
structural protocol. Dynamic or opaque actor references intentionally retain the
legacy permissive compatibility path for now, so protocol/runtime admission is
still an experimental hardening area rather than a universal guarantee.
Snapshot compaction is planned; backend-specific AOT gaps remain documented in
the status/conformance material.

## Why not just use Erlang/Elixir?

If the BEAM works for you, use it — it's battle-tested in ways Nulang won't
be for years. Nulang's differences: static typing with full inference,
compile-time data-race freedom via capabilities, effects tracked in types,
and journaled, event-sourced actor state. Durable-store selection and
supervisor snapshot hydration are now wired, while stronger crash-safety
claims still require store-specific destructive validation. Nulang is a small native runtime,
not a VM with OTP's operational tooling — that trade cuts both ways.

## Why not Gleam?

Gleam brings static types to the BEAM and is a good language. It doesn't
have durable/event-sourced actors, capability types, or algebraic effects —
it deliberately reuses the BEAM runtime. Nulang is a separate runtime making
different bets (durability, capabilities). They solve overlapping but
different problems.

## Why not Pony?

Pony originated the capability system Nulang uses (`iso/trn/ref/val/box/tag`,
plus `lineariso`), and credit is due. Nulang adds HM type inference (Pony
requires more annotation), algebraic effects, Erlang-style supervision with
links/monitors, and first-class durable/event-sourced actors.

## Why not Akka / Orleans / other actor frameworks?

Those are frameworks on general-purpose languages: the compiler doesn't know
you're writing actors, so it can't check message sendability, effect
handling, or capability discipline. In Nulang those are language semantics.
The corresponding cost: a young language and ecosystem versus decades of
JVM/.NET libraries.

## Was this written by AI?

LLM tools were used as development assistants; the repo's status docs and
RFC trail are public. The design decisions and the implementation are the
maintainer's. The verification story doesn't depend on trust: the repository carries a
large Rust test suite, a conformance suite with expected-output files, a
self-hosting bootstrap path, and dated implementation-status notes that
separate verified behavior from plans. Test counts change quickly, so the
current CI result is more meaningful than a number copied into this FAQ.

## Do durable actors really survive `kill -9`?

The persistence path is now wired far enough that this is no longer the same
gap described by the original launch draft. Durable programs can select a
disk-backed store with `--store <uri>` or `NULANG_STORE_PATH`; the CLI
supports JSON-file storage and feature-gated libSQL/SQLite, RocksDB, and
PostgreSQL forms. Persistent supervised children hydrate their saved snapshot
during `Supervisor::rebuild_child`, and `Runtime::recover_actor` restores
snapshot/journal state for runtime/process recovery paths.

That still does **not** justify a blanket production-grade "survives any
kill -9 with zero data loss" claim. Durability depends on the selected store,
its fsync/transaction guarantees, the exact crash boundary, semantic/artifact
compatibility, and the remaining stabilization work documented in
`docs/SEMANTIC_STABILIZATION_CONTRACT.md`. Treat destructive crash/recovery
tests as release evidence, not as an assumption.

## What's the performance story?

Register-based bytecode VM with a Cranelift JIT tier, sharded multi-threaded
execution, and ORCA garbage collection. Each live Runtime shard has one owning
cooperative scheduler thread; `NULANG_SHARDS>1` runs shards in parallel over
bounded cross-shard channels. Chase-Lev work-stealing APIs exist in the
scheduler, but the current live per-shard `run_scheduler()` path uses one
worker slot rather than peer stealing. Benchmarks and analysis are in
`benches/` and `docs/PERFORMANCE_ANALYSIS.md`. There are no published
head-to-head numbers against BEAM or Pony, and none should be claimed until
measured.

## What platforms are supported?

The tagged release matrix currently validates Linux x86_64/aarch64, macOS
aarch64, and Windows x86_64. Other platform/architecture combinations are not
release-tested. Rust 1.95.0 is pinned via `rust-toolchain.toml`.

## What's the deal with "1.0.0-frozen" and "200-year horizon"?

`1.0.0-frozen` is historical language metadata, not a statement that the
current source language is permanently frozen. RFC 0021 keeps already-published
compatibility contracts as archival obligations while source semantics remain
pre-adoption Stable until the external-validation freeze gate is satisfied.
It is not a claim that the implementation is finished — see the alpha
disclaimer.

## Is there a hosted platform? Is this open source?

The language and runtime are Apache-2.0 and fully self-hostable. Nulang
Cloud (nulang.cloud) is an optional managed platform for running Nulang
actors — no lock-in is the stated intent.

## How do I try it?

Build from source (`cargo build --release`, Rust 1.95.0), or download a
checksummed archive from GitHub Releases. Run the verified programs in
`examples/`, read `docs/GETTING_STARTED.md` and `docs/TUTORIAL.md`, or use
the browser playground. The VS Code extension is in `editors/vscode/`.

## How can I contribute?

CONTRIBUTING.md and CODE_OF_CONDUCT.md cover process; issues and
discussions are on GitHub. The most useful launch-era contributions:
reproducing (or breaking) the documented claims, conformance tests for
underspecified corners, and feedback on the supervision syntax RFC.
