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
Project Status section. GOVERNANCE.md defines three tiers — Frozen (bytecode
format, NUL0 wire protocol, Nulang Core), Stable (type system, effects,
capabilities, actor surface — breaking changes need an RFC + deprecation
cycle), Experimental (everything else) — and the pre-1.0 disclaimer warns
that any guarantee may be revised until the language sees real-world use.

## What's actually implemented vs. planned?

Implemented and tested: the full compiler pipeline (AST → HIR → MIR),
register-based bytecode VM, Cranelift JIT, supervision (one-for-one,
one-for-all, rest-for-one, simple-one-for-one), links/monitors/process
groups, algebraic effects with resume, HM inference, capabilities, persistent
actors with checkpointing + journaling (in-memory, JSON-file, SQLite store
backends), `entity`/`events`/`apply`/`emit` event sourcing, the `nula`
package manager, LSP server, REPL, test runner, an extensive Rust test suite,
and a `.nula` conformance suite under `conformance/`.

Experimental (feature-gated or marked): multi-node distribution over TCP,
WASM/WasmFX backends, the secondary native AOT backend (restricted semantics),
AI-agent runtime, Fabric, RESP cache serving, typed actor-protocol hardening,
and CRDT state fields (implemented and tested at the Rust level; the `.nula`
surface parses but behaves as `durable` — SPEC2 §9.10).

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
and journaled, event-sourced actor state — with the caveat, verified by
execution, that automatic state rebuild on restart is not yet wired to the
CLI path (see the kill -9 answer below). Nulang is a small native runtime,
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
maintainer's. The verification story doesn't depend on trust: ~1,680 tests,
a conformance suite with expected-output files, a stage-2-verified
self-hosting bootstrap, and dated "implementation status" notes in the spec
that explicitly separate verified behavior from plans.

## Do durable actors really survive `kill -9`?

Not yet, end-to-end — verified by running it. What works today: crash
*containment* (a supervised actor that dies is handled by its supervisor,
siblings keep state, the process stays up — see the executed demo in
`docs/launch/demo-script.md`), plus journaling and checkpointing of
persistent-actor state after every behavior. State-rebuild recovery is
implemented and pinned by an integration test at the Rust runtime level
(`recover_actor` with a shared store; JSON-file and SQLite backends have
round-trip tests), but two wirings are missing: the CLI constructs an
in-memory store (`src/runtime/mod.rs`) with no flag for a file backend, and
supervised restarts currently come back with fresh state — confirmed by
execution while preparing the demo. Wiring recovery into supervisor restarts
and exposing a file-backed store from the CLI is the top pre-1.0 milestone.
We know "durable" invites the kill -9 test — the gap is disclosed here
rather than discovered by you.

## What's the performance story?

Register-based bytecode VM with a Cranelift JIT tier, a multi-threaded
work-stealing scheduler, and ORCA garbage collection. Benchmarks and
analysis are in `benches/` and PERFORMANCE_ANALYSIS.md. There are no
published head-to-head numbers against BEAM or Pony, and none should be
claimed until measured.

## What platforms are supported?

Linux and macOS (x86_64 and aarch64 release artifacts per
docs/RELEASING.md). Windows is not supported yet — WSL works. Rust 1.95.0 is
pinned via `rust-toolchain.toml`.

## What's the deal with "1.0.0-frozen" and "200-year horizon"?

`1.0.0-frozen` is the *language* version: the bytecode format, wire
protocol, and Nulang Core are designated Frozen (never break), per RFCs
0001/0002 and GOVERNANCE.md. It is not a claim that the implementation is
finished — see the alpha disclaimer. The long-horizon framing is a design
discipline (freeze what must never change, iterate on the rest), not a
prediction.

## Is there a hosted platform? Is this open source?

The language and runtime are Apache-2.0 and fully self-hostable. Nulang
Cloud (nulang.cloud) is an optional managed platform for running Nulang
actors — no lock-in is the stated intent.

## How do I try it?

Build from source (`cargo build --release`, Rust 1.95.0), or download a
release tarball once v0.1.0 is cut. Run the 17 verified programs in
`examples/`, read docs/GETTING_STARTED.md and docs/TUTORIAL.md, or run the
playground locally with `python3 playground/server.py`. The VS Code
extension is in `editors/vscode/`.

## How can I contribute?

CONTRIBUTING.md and CODE_OF_CONDUCT.md cover process; issues and
discussions are on GitHub. The most useful launch-era contributions:
reproducing (or breaking) the documented claims, conformance tests for
underspecified corners, and feedback on the supervision syntax RFC.
