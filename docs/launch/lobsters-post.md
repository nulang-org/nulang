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
checkpointed and journaled after every behavior. `entity` declarations are
event-sourced by default. (Caveat up front: journal-based state rebuild is
implemented and integration-tested at the runtime level, but not yet wired to
supervised restarts on the CLI path — a restarted actor currently starts
fresh. Details in docs/launch/demo-script.md, which documents an executed
crash-containment demo.)

Pipeline: AST → HIR → MIR → register-based bytecode VM, with a Cranelift JIT
and experimental AOT/WASM backends. The runtime is a multi-threaded
work-stealing executor with ORCA GC, links/monitors, process groups, and
supervision strategies (one-for-one, one-for-all, rest-for-one,
simple-one-for-one).

Things I expect lobste.rs to poke at, preemptively:

- **Maturity**: alpha, no external users, expect breaking changes pre-1.0.
  GOVERNANCE.md defines frozen/stable/experimental tiers and what each
  guarantees.
- **Tests/proofs**: ~1,680 Rust tests, a `.nula` conformance suite
  (`conformance/`), a self-hosting bootstrap verified through stage 2, and
  partial Lean 4 / Coq formalization under `formal/` (the shift_compose lemma
  is 12/13 proved — i.e., not finished).
- **Known sharp edges are documented in-tree**: e.g. `send` to a nonexistent
  behavior currently runs behavior 0 instead of erroring (documented in SPEC2
  §8.5 with conformance evidence), and `state crdt` fields parse but behave as
  `durable` because the CRDT wiring is Rust-level only so far. SPEC2 has dated
  "implementation status" notes throughout rather than aspiration-as-fact.
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
