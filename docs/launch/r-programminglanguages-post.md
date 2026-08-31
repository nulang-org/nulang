# r/programminglanguages post — Nulang

Suggested title:
**Nulang: ML-flavored, statically-typed actor language with Pony capabilities, algebraic effects, and durable event-sourced actors (alpha, Rust impl)**

Flair: "Language" / "Project". Post as a self-post (text post) with the body
below — r/programminglanguages engages better with a substantive text post
than a bare link.

---

## Post body

I've been working on Nulang, an actor-based language that tries to combine
four lines of PL work that usually live in separate languages:

- **Actors + supervision** (Erlang): `spawn`, `send`/`ask`, selective
  `receive` with `after`, links, monitors, supervision trees with the classic
  restart strategies.
- **Hindley-Milner inference** (ML family): full Algorithm W, row-polymorphic
  records and variants, no required annotations.
- **Reference capabilities** (Pony): `iso/trn/ref/val/box/tag` plus
  `lineariso`, checked at compile time, erased at runtime, giving data-race
  freedom without a borrow checker. Sendability is derived from capabilities.
- **Algebraic effects**: `perform`/`handle` with resume semantics; effect rows
  appear in function signatures (`fn f(...) -> T ! {IO, FS}`), so unhandled
  effects are compile errors. All I/O goes through effects.

The piece I haven't seen combined with the above: **durability as a state
model**. Persistent actors declare state as `local`, `durable`,
`event_sourced`, or `crdt`; the runtime checkpoints and journals after each
behavior. `entity` declarations are event-sourced by default with
`events`/`apply`/`emit` blocks, so the aggregate-root pattern is syntax, not
a framework. (Honest status: state-rebuild recovery is implemented and
integration-tested at the runtime level but not yet wired to supervised
restarts on the CLI path — verified by execution; see
docs/launch/demo-script.md.)

```nulang
entity Counter {
    state count: Int = 0
    events
        | Incremented(by: Int)
    apply
        | Incremented(by) => self.count = self.count + by
    behavior increment(by: Int) { emit Incremented(by) }
    behavior get() { self.count }
}
```

Implementation: Rust; AST → HIR → MIR → register-based bytecode VM with a
Cranelift JIT, experimental Cranelift AOT and WASM backends, work-stealing
multi-threaded scheduler, ORCA GC. ~1,680 tests, a conformance suite, partial
Lean 4 formalization of the core semantics, and a stage-2-verified
self-hosting bootstrap.

Honest status: alpha. No users. Distribution (multi-node send/ask over TCP),
WASM/AOT backends, and the AI-agent runtime are experimental and
feature-gated. Some documented rough edges exist (e.g. misaddressed `send`
currently falls back to behavior 0 — see SPEC2 §8.5; CRDT state is Rust-level
only). The full spec (SPEC2.md) annotates what is verified vs. planned.

Repo: https://github.com/nulang-org/nulang — spec, examples (17 verified
programs), and the conformance suite are in-tree.

Design questions I'd love this subreddit's take on:

1. Capabilities × effect rows: is checking sendability via capabilities the
   right call vs. a separate effect for message sends?
2. Event sourcing as a *default* for entities — good default or foot-gun?
3. Frozen core (bytecode format + wire protocol frozen forever) as a
   stability strategy for a young language — credible or premature?

---

## Comment strategy

- Lead replies with mechanisms, cite SPEC2 chapters.
- Expect "why not Erlang/Gleam/Pony/Akka/Kalos" — faq.md has prepared,
  non-defensive answers.
- If the thread veers into "AI-written?" (the repo discloses LLM assistance),
  answer plainly per faq.md — do not get defensive.
