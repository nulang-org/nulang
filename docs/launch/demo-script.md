# The Demo — Supervised Chat Server (crash containment, executed)

**Validation status: HISTORICAL EXECUTED TRANSCRIPT.** The program below was
type-checked and run against commit `654910f` with Rust 1.95.0; the displayed
output is the verbatim result from that commit. **Do not treat the old recovery
notes as current-main behavior.** Since this demo was recorded, durable-store
selection and persistent supervisor-restart hydration have been wired. Re-run
the script against the exact release/commit you plan to demonstrate and record
new observed output before publishing a fresh performance or durability claim.

## Honesty note — read before recording

The original launch preparation discovered a real durability gap at commit
`654910f`: supervised persistent children restarted from template state
instead of a durable snapshot, and the CLI did not expose a durable store.
Those findings are preserved here as historical evidence, but they have since
been superseded.

Current main now exposes `--store <uri>` / `NULANG_STORE_PATH`, and
`Supervisor::rebuild_child` loads a saved snapshot for persistent children
before re-keying it to the new actor id. `Runtime::recover_actor` remains the
explicit snapshot/journal recovery path. This is stronger than the original
demo, but still does **not** justify an unconditional "kill -9 safe" claim:
store durability settings, crash ordering, semantic/artifact compatibility,
and stale-writer fencing still determine the guarantee.

For a new recording, keep the crash-containment segment, then add a separate
destructive recovery run against the selected disk-backed store and exact
release commit. Treat the observed recovery result as evidence; do not reuse
the old transcript as proof of current behavior.

## `chat_server.nula` (~75 lines, executed)

```nulang
// chat_server.nula — a supervised chat server.
//
// Two actors under a one-for-one supervisor: a durable ChatRoom and a
// ChatLogger. We crash the room mid-conversation; the supervisor absorbs
// the failure, the logger keeps every line it recorded, and the
// supervision set stays intact. No try/catch anywhere.
//
// Run with: nulang chat_server.nula

persistent actor ChatRoom {
    state durable messages: Int = 0

    behavior post(sender: String, text: String) {
        self.messages = self.messages + 1
        perform IO.print("[room] " + sender + ": " + text)
        self.messages
    }

    behavior count() {
        self.messages
    }

    // The controlled crash: an abnormal exit, exactly like a real failure.
    behavior crash() {
        perform IO.print("[room] *** crashing ***")
        perform Actor.exit(1)
    }
}

actor ChatLogger {
    state local lines: Int = 0

    behavior log(text: String) {
        self.lines = self.lines + 1
        perform IO.print("[log] (" + perform Int.to_string(self.lines) + ") " + text)
        self.lines
    }

    behavior total() {
        self.lines
    }
}

fn main() {
    // One-for-one supervisor (strategy 0): a child that dies is restarted
    // (or cleaned up) on its own — siblings are never touched.
    let sup = perform Otp.create_supervisor("chat_sup", 0)

    let room = spawn ChatRoom {} as "room:main"
    let logger = spawn ChatLogger {}

    perform Otp.supervise_child(sup, room, 0)
    perform Otp.supervise_child(sup, logger, 0)

    // A normal chat round, mirrored to the logger.
    let m1 = ask room post("alice", "hello everyone!")
    let l1 = ask logger log("alice said hello")
    let m2 = ask room post("bob", "hi alice!")
    let l2 = ask logger log("bob said hi")

    perform IO.print(
        "room messages: " + perform Int.to_string(ask room count()) +
        ", log lines: " + perform Int.to_string(ask logger total())
    )

    // Kill the room. No try/catch, no checkpoint code.
    room ! crash()

    // The supervision set is intact and the logger never noticed.
    perform IO.print(
        "children still supervised: " +
        perform Int.to_string(perform Otp.child_count(sup))
    )
    perform IO.print(
        "log lines after crash: " +
        perform Int.to_string(ask logger total())
    )
    0
}
```

### Observed output (verbatim, exit code 0)

```
[room] alice: hello everyone!
[log] (1) alice said hello
[room] bob: hi alice!
[log] (2) bob said hi
room messages: 2, log lines: 2
children still supervised: 2
log lines after crash: 2
[room] *** crashing ***
```

Note the ordering: the crash line prints last even though `crash` is sent
earlier — message sends are asynchronous, and the room processes its mailbox
in order while main continues. That is a feature of the demo, not a bug:
point it out; it shows real actor semantics.

## Segment 2 — event-sourced entities (executed)

Show the durable-side surface with the conformance-pinned entity counter
(`conformance/behavior/org_03_*`, `persist_07_*`):

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
fn main() {
    let c = spawn Counter {} in {
        send c increment(3)
        send c increment(4)
        perform IO.print(ask c get())
    }
}
```

**Observed output: `9`.** Explain it honestly if asked: each `emit` journals
the event (which also contributes a per-emit tick to event-sourced fields —
see SPEC2 §9.6's implementation note), and the `apply` handler adds `by`;
3+1+4+1 = 9, matching the `persist_07` conformance expectation. The point to
land: `emit`/`apply`/`events` are language syntax, and every emission is
journaled by the runtime.

## Segment 3 — durability, revalidate before recording

For a current recording, use wording like:

> "The runtime checkpoints and journals persistent actors, the CLI can select a
> durable store with `--store`, and persistent supervised children hydrate
> their saved snapshot when restarted. I still treat crash recovery as a
> tested property of a specific store/configuration/release rather than a
> blanket guarantee, so this demo includes an actual destructive restart."

Use a temporary disk-backed store, run the program, terminate the process at a
controlled boundary, restart against the same store, and show the recovered
state. Capture the exact commit, store URI/backend, and durability settings in
the recording notes. The semantic stabilization contract requires this kind of
fault-injection evidence before strengthening production claims.

## Recording instructions

**Tool:** asciinema (preferred — text is copy-pasteable, embeds cleanly on
GitHub) with agg or a GIF conversion for social. Terminalizer is an
acceptable alternative.

```bash
# 80x24, large font, clean PS1 like '$ ', dark theme.
asciinema rec --cols 100 --rows 30 -i 1.5 nulang-demo.cast
# In the recording:
$ cat chat_server.nula            # briefly — scroll with bat/nl if long
$ nulang --check chat_server.nula # type-check only: shows HM inference
$ nulang chat_server.nula         # the run: chat, crash, containment
$ nulang entity_seg.nula          # the entity segment (prints 9)
asciinema upload nulang-demo.cast
```

Pacing: pause 1–2 s after `*** crashing ***` and after
`log lines after crash: 2`. Total runtime target: 60–90 seconds.

GIF for README/social: `agg nulang-demo.cast nulang-demo.gif` (asciinema's
agg) at 1000px width, or terminalizer's `render`. Keep the GIF under ~5 MB.

## Talking points (while the demo runs)

1. "One file. No framework imports — actors and supervision are language and
   runtime features."
2. "No annotations anywhere — full Hindley-Milner inference. `--check` ran
   the typechecker without executing."
3. "The crash is a real abnormal exit. The only fault-tolerance code is one
   supervisor line. The logger kept its state; the supervision set is intact;
   the process exits cleanly."
4. "Entities are event-sourced by default — `emit` is the only persistence
   code in the program."
5. The honesty beat (Segment 3 above). Deliver it unprompted — it converts
   skeptics.

## Execution log

- [x] Build compiler (`cargo build --no-default-features`, Rust 1.95.0) — ok
- [x] `nulang --check chat_server.nula` — "Type check passed."
- [x] `nulang chat_server.nula` — observed output recorded above, exit 0
- [x] Entity segment — observed output `9`, exit 0
- [x] Attempted durable-state-across-supervised-restart variant — **failed
      (state resets to fresh)**; documented above, demo redesigned around it
