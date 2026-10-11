# Bounded actor polling for Rust embedders (experimental)

`Runtime::pump_ready(max_turns)` lets a Rust host advance an existing Nulang
actor runtime from its own event loop, without calling the standalone
`run_scheduler()` idle wait loop. It does **not** create a new scheduler or
change the language/bytecode formats.

```rust
use nulang::runtime::Runtime;
use nulang::vm::Value;

let mut rt = Runtime::new();
let actor = rt.spawn_actor(Box::new(|| {
    vec![("count".to_string(), Value::int(0))]
}));
rt.actors.get_mut(&actor).unwrap().register_behavior("inc", |actor, _args| {
    let count = actor.get_state_field("count").and_then(|v| v.as_int()).unwrap_or(0);
    actor.set_state_field("count", Value::int(count + 1));
});

rt.send_message(actor, "inc", &[]);
// Each poll executes at most 8 *ready actor turns*, not 8 wall-clock ms.
let turns_processed = rt.pump_ready(8);
assert_eq!(turns_processed, 1);
```

## Host-owned local actor lifecycle (stacked proposal)

`runtime::embedded_host::EmbeddedActorHost` provides a small Rust facade over
`Runtime::pump_ready`, with an explicit actor-creation quota, integer-only
initial state and message arguments, trusted native function-pointer behaviors,
and idempotent shutdown.

```rust
use nulang::runtime::embedded_host::EmbeddedActorHost;
use nulang::runtime::Actor;
use nulang::vm::Value;

fn increment(actor: &mut Actor, args: &[Value]) {
    let by = args.first().and_then(Value::as_int).unwrap_or(0);
    let current = actor.get_state_field("count").and_then(|v| v.as_int()).unwrap_or(0);
    actor.set_state_field("count", Value::int(current + by));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut host = EmbeddedActorHost::new(32);
    let actor = host.spawn(vec![("count".to_string(), 0)])?;
    host.register_behavior(actor, "increment", increment)?;
    host.send_int(actor, "increment", &[5])?;
    assert_eq!(host.poll(1)?, 1);
    assert_eq!(host.get_int_field(actor, "count")?, Some(5));
    host.shutdown();
    Ok(())
}
```

The facade rejects invalid actor IDs, unregistered behaviors, duplicate
registrations, exceeding the configured actor quota, integer truncation, and
operations after shutdown. `send_int` explicitly returns `HostError::MailboxFull`
if local mailbox admission fails due to backpressure. An accepted message is
**not** proof of behavior execution or durable delivery. This is not a
persistent-storage API, a C ABI, or an async executor.

## Behavior and limitations

- **No idle wait:** if no actors are ready, the method returns 0 even with
  future timers scheduled. It does not call the standalone scheduler's idle
  callback or block waiting for an LLM completion.
- **Bounded turns:** up to `max_turns` ready actor messages are processed.
  Unprocessed work remains in mailboxes. `pump_ready(0)` does nothing.
- **Timers:** each nonzero poll checks currently due timers once. The host
  must keep scheduling polls, including after a return of 0, if it needs
  future timer execution.
- **GC:** the method processes pending GC and deferred operations at the end
  of its poll. It is not intended as an alternative to the distributed
  runtime's full long-lived maintenance loop.
- **Not real-time:** behavior callbacks or effects may perform blocking work.
  The limit bounds actor turns, *not* CPU time, IO time, or message-delivery
  latency.
- **Not a security sandbox:** host-supplied behaviors and native extensions
  still execute with host-process privileges.
- **No C lifecycle yet:** this low-level Rust primitive is the prerequisite
  for adding per-runtime actor lifecycle and polling to the existing C and
  Rust SDK interfaces after their current embedding PR stack is verified.

Do not replace `run_scheduler()` in a standalone distributed runtime with
`pump_ready()` without separately providing its network and maintenance
integration.
