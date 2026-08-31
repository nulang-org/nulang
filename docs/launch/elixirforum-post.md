# ElixirForum post — Nulang

Suggested title:
**Nulang: a statically-typed, durable take on the BEAM model (alpha — feedback wanted from BEAM people)**

Category: "Projects". ElixirForum values humility toward OTP — lead with the
comparison and be explicit that Nulang is not trying to replace the BEAM.

---

## Post body

Hi all — long-time admirer of OTP here. I've been building **Nulang**, an
actor language that asks: what would the BEAM model look like with static
types and durable process state? I'm posting here because the people who will
give the most useful criticism are the ones who know supervision trees cold.

### The BEAM mapping

| BEAM/OTP | Nulang |
|---|---|
| `spawn` / processes | `spawn Actor {}` — actors with isolated state |
| `send` / `GenServer.call` | `actor ! msg(...)` (cast) / `ask actor msg(...)` (call) |
| `receive` + `after` | selective `receive { \| Msg(x) => ... }` with `after` timeout |
| Links / monitors | `perform Actor.link(t)`, `Actor.monitor(t)`, `Actor.trap_exit(true)`, DOWN messages |
| Registered names | `perform Actor.register("name")` / `Actor.whereis("name")` |
| Supervisors (one_for_one, one_for_all, rest_for_one, simple_one_for_one) | `perform Otp.create_supervisor(name, strategy)`, `Otp.start_child`, restart policies Permanent/Temporary/Transient |
| `:pg` / process groups | process groups built in |
| `atom` registries, ETS | — (no direct analog; state lives in actors) |

The two big departures from the BEAM:

1. **Types.** Full Hindley-Milner inference (write no annotations, get
   static typing), plus Pony-style reference capabilities
   (`iso/trn/ref/val/box/tag`) so data races are compile errors, and
   algebraic effects so side effects are tracked in signatures. Messages are
   checked for sendability at compile time.

2. **Durability (in progress — honestly).** On the BEAM, a restarted process
   comes back with *fresh* state — recovery is your problem (ETS, DETS,
   Mnesia, Postgres…). Nulang's design makes recovery the runtime's job:
   persistent actors checkpoint and journal their state after every behavior,
   and `entity` declarations are event-sourced by default. Status check,
   because this crowd will test it: journaling and the storage backends
   (in-memory / JSON-file / SQLite) are implemented and tested, and
   state-rebuild recovery is pinned by integration tests at the runtime level
   — but it is **not yet wired to supervised restarts on the CLI path**, so a
   restarted actor currently starts fresh. That wiring is the top pre-1.0
   milestone. The entity surface itself works today:

   ```nulang
   entity ChatRoom {
       state messages: Int = 0
       events
           | MessagePosted(sender: String, text: String)
       apply
           | MessagePosted(sender, text) => self.messages = self.messages + 1
       behavior post(sender: String, text: String) {
           emit MessagePosted(sender, text)
       }
       behavior count() { self.messages }
   }
   ```

### What it is not

- It is not on the BEAM — it's a separate Rust runtime (bytecode VM +
  Cranelift JIT, work-stealing scheduler, ORCA GC). No interop with Erlang/Elixir.
- It is not mature. Alpha, no users, breaking changes expected pre-1.0.
  Distribution (multi-node messaging over TCP) is experimental.
- It is not claiming to beat OTP at what OTP does. OTP has 40 years of
  production scar tissue; Nulang has ~1,680 tests and a spec.

### What I'd love from this community

- Does durable-by-default actor state solve a pain you've actually had, or
  does it conflict with "let it crash" philosophy in practice?
- Typed behaviors vs. dynamic messages: what would you miss?
- The supervision API is currently runtime-driven (`Otp.*` effect calls);
  declarative in-language supervision syntax is planned. What would you want
  it to look like?

Repo: https://github.com/nulang-org/nulang (Apache-2.0). Spec in SPEC2.md,
BEAM-primitive notes in BEAM_PRIMITIVES.md, 17 verified examples in
`examples/` including a supervisor tree and a chat room.

---

## Comment strategy

- Expect (and welcome) "just use Elixir + typed_behaviour / Gleam / Mnesia."
  Answer per faq.md: acknowledge the maturity gap first, then explain the
  mechanism difference.
- Do not oversell durability: supervised restarts currently come back with
  fresh state on the CLI path (verified by execution — see
  docs/launch/demo-script.md); journal-based state rebuild is implemented and
  integration-tested at the Rust runtime level but not yet wired to
  supervisor restarts or a CLI flag. Say so if asked — before being asked,
  ideally.
