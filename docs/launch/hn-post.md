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
their state after every behavior, and `entity` declarations are event-sourced
by default. (Candor up front: journal-based state rebuild is implemented and
integration-tested at the runtime level, but isn't yet wired to supervised
restarts — a restarted actor currently starts fresh. That's the top pre-1.0
milestone; the demo in docs/launch/demo-script.md shows what *does* work
today, with observed output recorded.)

What's real today: the compiler (Rust; AST → HIR → MIR) targeting a
register-based bytecode VM with a Cranelift JIT, supervision, links/monitors,
an effect system (`perform`/`handle` with resume semantics — effects are
checked in function signatures), persistent actors with three storage
backends, a package manager (`nula`), an LSP server, and ~1,680 passing tests.
There are also Coq/Lean formalization efforts in the repo (partial — see
formal/).

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
but the design, architecture decisions, and this launch post are human. The
~1,680-test suite, conformance suite under `conformance/`, and bootstrap
verification are there so you don't have to take anyone's word for anything.

**4. "Production-ready?"**
No. It's alpha, explicitly. The README says so, the stability tiers in
GOVERNANCE.md say so, and the pre-1.0 disclaimer says breaking changes are
expected. What's offered today is a real, test-covered implementation you can
build and run — not a production commitment.

**5. "Performance?"**
Register-based bytecode VM with a Cranelift JIT, multi-threaded work-stealing
scheduler, ORCA garbage collection. Benchmarks live in `benches/` and
PERFORMANCE_ANALYSIS.md. We have not done competitive benchmarking against
BEAM and won't claim wins we haven't measured — perf work is ongoing and the
AOT backend is experimental.

**6. "Why not Pony? You even took its capabilities."**
Pony is a major influence (credit due: `iso/trn/ref/val/box/tag` are Pony's).
Nulang differs in having HM inference (Pony is nominally typed with more
annotation), algebraic effects, durable/event-sourced actors, and
Erlang-style supervision with links and monitors. Pony's actor persistence is
not a language-level feature.

**7. "Durable actors that survive kill -9 — really?"**
Not yet, through the CLI — and we're saying so up front. The persistence
machinery is real and tested: a `PersistenceStore` trait with in-memory,
JSON-file, and SQLite backends; checkpointing and journaling after each
behavior step; and state-rebuild recovery pinned by an integration test that
drives `recover_actor` directly (`src/integration_tests/mod.rs`). But two
wirings are missing: the CLI constructs an in-memory store (no flag for a
file backend), and a supervised restart currently comes back with fresh
state — we verified this by running it while preparing the demo
(`docs/launch/demo-script.md` documents the observed behavior). Wiring
recovery into supervisor restarts and the CLI is the top pre-1.0 milestone.
What works today, and what the demo shows: crash containment — a supervised
actor dies, siblings keep their state, the system stays up.

**8. "Algebraic effects and actors and capabilities and durability — isn't this too much?"**
Fair. The mitigations: the effect system is how all I/O is expressed (there's
one way to do side effects, not four); capabilities are mostly inferred and
erased at runtime; and the frozen/stable/experimental tiers mean the core you
learn first is small and won't break. But yes — the feature surface is broad
for an alpha, and GOVERNANCE.md exists precisely to keep it honest.

**9. "Who is this for? What's the use case?"**
Long-lived stateful services that hate losing state: chat/team servers,
workflow engines, game backends, IoT coordinators, agents with memory.
Anywhere you'd reach for Erlang/OTP or an event-sourcing framework plus a
supervision library, and would rather have the compiler check it.

**10. "1.0.0-frozen but alpha? Windows? Editor support?"**
`1.0.0-frozen` is a *language* version for the frozen core (bytecode format,
wire protocol, Nulang Core) — the implementation is alpha; see GOVERNANCE.md.
Windows isn't supported yet (use WSL); it's on the roadmap. There's a VS Code
extension in `editors/vscode/` (syntax + LSP) and `nulang --lsp` implements
hover/goto-def/rename/completion/diagnostics.
