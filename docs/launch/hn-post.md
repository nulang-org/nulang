# Show HN — Nulang

Post type: **Show HN**. Submit as a text-light link post to the GitHub repo,
then immediately post the author comment below.

---

## Title options

1. **Show HN: Nulang – A typed, durable BEAM for the rest of us (actors, algebraic effects, HM types)**
2. **Show HN: Nulang – Erlang-style actors with Hindley-Milner types and event-sourced persistence**
3. **Show HN: Nulang – ML-flavored actor language with Pony-style capabilities and durable, event-sourced actors**
4. **Show HN: Nulang – Supervision trees and durable actors, with a real type system**

Recommended: option 1. It states the positioning honestly, signals the
audience (people who admire BEAM but want static types), and avoids hype words.

---

## Author comment (post immediately after submitting)

Hi HN, I'm David. Nulang is an actor-based language I've been building: take
Erlang's supervision trees and message-passing actors, give them a
Hindley-Milner type system with full inference, add Pony-style reference
capabilities (`iso`/`trn`/`ref`/`val`/`box`/`tag`) for compile-time data-race
freedom, and make actors *durable* — persistent actors checkpoint and journal
their state after behaviors, and `entity` declarations are event-sourced by
default. Durable-store selection is now wired through `--store` /
`NULANG_STORE_PATH`, and persistent supervised children hydrate saved state
during restart. This is still alpha: crash-safety depends on the selected
store and exact failure boundary, so destructive recovery tests remain part of
the stabilization gate rather than a blanket durability claim.

What's real today: the compiler (Rust; AST → HIR → MIR) targeting a
register-based bytecode VM with a Cranelift JIT, supervision, links/monitors,
an effect system (`perform`/`handle` with resume semantics — effects are
checked in function signatures), persistent actors behind a pluggable
`PersistenceStore` (memory, JSON-file, libSQL/SQLite, plus optional RocksDB
and PostgreSQL), a package manager (`nula`), an LSP server, and a large Rust
test/conformance suite. There are also formalization efforts in the repo;
current CI is the source of truth for test counts.

What's experimental or unfinished, honestly: multi-node distribution (works
over TCP, marked experimental), the WASM/WasmFX and native AOT execution paths (experimental and semantically
restricted), CRDT state (implemented at the Rust level only, not yet wired to
source syntax), Fabric/cache surfaces, typed actor-protocol hardening, and the
AI-agent runtime. There is
no production user base. This is alpha — I'm launching to get brutal feedback,
not to claim it's done.

The design bet: durability and fault tolerance shouldn't be frameworks you
bolt on — they should be how the language works. Ask me anything.

---

## Anticipated top-10 skeptical questions, with honest answers

**1. "Another actor language? What does this do that Erlang/Elixir doesn't?"**
The BEAM is a 40-year-validated runtime with a dynamic type system and no
durable actor state. Nulang's deltas: static HM typing with inference (no
annotations needed), reference capabilities that make data races a compile
error, algebraic effects so side effects are visible in types, and
event-sourced durable actors (journaling works today; automatic state rebuild
on restart is the top pre-1.0 milestone — see Q7). If you're happy
on the BEAM, stay — Nulang is for people who want BEAM's model with static
guarantees, on a small native runtime instead of a VM the size of OTP.

**2. "Why not just use Gleam?"**
Gleam is excellent and brings types to the BEAM — but it runs *on* the BEAM,
inherits its distribution model, and doesn't have durable/event-sourced actors
or capability types. Nulang is a separate runtime with persistence as a
first-class state model (`local`/`durable`/`event_sourced`/`crdt`) and
compile-time data-race freedom via capabilities, which neither Erlang nor
Gleam attempts.

**3. "Is this AI-generated?"**
The repo is public — judge the commit history. LLM tools were used as
assistants during development (as disclosed in the repo's RFC/status docs),
but the design, architecture decisions, and this launch post are human. The test suite, conformance suite under `conformance/`, and bootstrap
verification are there so you don't have to take anyone's word for anything.
Test counts move quickly, so CI is more useful than a copied number here.

**4. "Production-ready?"**
No. It's alpha, explicitly. The README says so, the stability tiers in
GOVERNANCE.md say so, and the pre-1.0 disclaimer says breaking changes are
expected. What's offered today is a real, test-covered implementation you can
build and run — not a production commitment.

**5. "Performance?"**
Register-based bytecode VM with a Cranelift JIT, sharded multi-threaded
execution, and ORCA garbage collection. Each live shard has one cooperative
scheduler owner; multiple shards run in parallel. Chase-Lev stealing exists in
the scheduler implementation but is not used by the current live per-shard
loop. Benchmarks live in `benches/` and `docs/PERFORMANCE_ANALYSIS.md`.
We have not published competitive BEAM/Pony results and won't claim wins we
haven't measured.

**6. "Why not Pony? You even took its capabilities."**
Pony is a major influence (credit due: `iso/trn/ref/val/box/tag` are Pony's).
Nulang differs in having HM inference (Pony is nominally typed with more
annotation), algebraic effects, durable/event-sourced actors, and
Erlang-style supervision with links and monitors. Pony's actor persistence is
not a language-level feature.

**7. "Durable actors that survive kill -9 — really?"**
The old launch answer is obsolete: durable-store selection is now exposed by
the CLI (`--store` / `NULANG_STORE_PATH`), `Runtime::recover_actor` restores
snapshot/journal state, and persistent supervised children hydrate a saved
snapshot in `Supervisor::rebuild_child`. The runtime includes memory and
JSON stores, default-feature libSQL/SQLite, and optional RocksDB/PostgreSQL
backends.

That is not the same as claiming production-grade zero-loss recovery from
every `kill -9`. Store durability settings, crash ordering, artifact/schema
compatibility, and stale-writer fencing still matter. The active requirement is
to prove recovery with destructive fault-injection tests before strengthening
the claim.

**8. "Algebraic effects and actors and capabilities and durability — isn't this too much?"**
Fair. The mitigations: the effect system is how all I/O is expressed (there's
one way to do side effects, not four); capabilities are mostly inferred and
erased at runtime; and the stability tiers keep experimental surfaces distinguishable from
current Stable semantics. But yes — the feature surface is broad for an alpha,
and the project now deliberately delays permanent source freezing until there
is external adoption evidence.

**9. "Who is this for? What's the use case?"**
Long-lived stateful services that hate losing state: chat/team servers,
workflow engines, game backends, IoT coordinators, agents with memory.
Anywhere you'd reach for Erlang/OTP or an event-sourcing framework plus a
supervision library, and would rather have the compiler check it.

**10. "1.0.0-frozen but alpha? Windows? Editor support?"**
`1.0.0-frozen` is historical artifact metadata. RFC 0021 reclassified
pre-adoption source semantics so the current source language is not permanently
frozen; published versioned compatibility contracts remain obligations.
Tagged release CI now validates Windows x86_64 alongside Linux x86_64/aarch64
and macOS aarch64. There's a VS Code extension in `editors/vscode/`, and
`nulang --lsp` provides diagnostics, hover, navigation, rename, completion,
formatting, code actions, inlay hints, semantic tokens, and more.
