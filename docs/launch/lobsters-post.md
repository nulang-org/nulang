# Lobste.rs post — Nulang

Suggested title:
**Nulang: an actor language with HM types, Pony-style capabilities, and durable event-sourced actors**

Tags: `plt`, `compilers`, `rust`, `distributed` (pick 2–3; `plt` + `rust` is the
natural fit). Submit as a link to https://github.com/nulang-org/nulang.

Lobste.rs culture note: no marketing voice, no "excited to announce." Post the
link, then add a plain-text comment with the technical summary below. If you
are a new account, be extra careful to be technical and non-promotional;
consider asking an established member to submit.

---

## Submission text / first comment

Nulang is an actor-based language implemented in Rust. The short version:
Erlang's actors and supervision, an ML-derived type system (full Hindley-Milner
inference, row-polymorphic records and variants, effect rows), Pony's
reference capabilities (`iso/trn/ref/val/box/tag/lineariso`) checked at
compile time and erased at runtime, and persistent actors whose state is
checkpointed and journaled after behaviors. `entity` declarations are
event-sourced by default. Durable stores are selectable through `--store` /
`NULANG_STORE_PATH`, and persistent supervised children hydrate saved state
when restarted. This is still alpha; destructive recovery testing against the
chosen store/configuration is required before making stronger durability
claims.

Pipeline: AST → HIR → MIR → register-based bytecode VM, with a Cranelift JIT
and experimental native AOT/WASM backends. Actor execution is sharded: each
live shard has one cooperative scheduler owner, and multiple shards can run in
parallel. Chase-Lev peer stealing exists in the scheduler implementation but
is not used by the current live per-shard loop. ORCA GC, links/monitors,
process groups, and supervision strategies are integrated into that runtime.

Things I expect lobste.rs to poke at, preemptively:

- **Maturity**: alpha, no external users, expect breaking changes pre-1.0.
  GOVERNANCE.md defines frozen/stable/experimental tiers and what each
  guarantees.
- **Tests/proofs**: a large Rust test suite, a `.nula` conformance suite
  (`conformance/`), a self-hosting bootstrap path, and partial formalization
  under `formal/`. CI is the source of truth for current test/proof counts.
- **Known sharp edges are documented in-tree**: typed actor-protocol admission
  remains experimental. Source-level typed CRDT fields and `Crdt.*` operations
  are now wired and conformance-tested, but recovered actors still need the
  field-name→CRDT-id mapping rebuilt/re-registered before CRDT operations are
  fully usable. SPEC2 documents that recovery gap explicitly.
- **Distribution**: multi-node `send`/`ask` over TCP (NUL0 wire protocol) and
  gossip membership work but are marked experimental.

Spec: SPEC2.md (~3,800 lines, syntax/semantics/type system/runtime).
Architecture map: ARCHITECTURE.md. Feedback on the type system (capabilities
× effect rows interaction especially) very welcome.

---

## Notes for the author

- Respond to every technical comment within a few hours; lobste.rs threads
  reward depth over speed.
- If asked "why not X" (Erlang, Gleam, Pony, Akka), answer with specific
  mechanism differences, not adjectives. See faq.md for prepared answers.
- Do not mention nulang.cloud unless directly asked about monetization; if
  asked: the language/runtime is Apache-2.0 and self-hostable; the cloud is an
  optional managed platform.
