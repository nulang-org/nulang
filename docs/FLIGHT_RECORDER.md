# Flight recorder: local mailbox diagnostics

Nulang's per-actor `FlightRecorder` is a bounded, opt-in **diagnostic breadcrumb buffer**, not a deterministic replay log or a substitute for durable workflow history.

## Enable it

Set `NULANG_FLIGHT_RECORDER=1` before starting a native runtime. Runtime-created actors have recording disabled by default; when disabled, they allocate no recorder ring and format no payload values. Rust embedders can construct an explicitly enabled recorder with `FlightRecorder::new(capacity)`. A zero-capacity recorder is disabled.

## Events currently captured

| Event | Meaning |
| --- | --- |
| `FlightEventKind::MessageAdmitted` | A local actor mailbox accepted a message |
| `FlightEventKind::MailboxBackpressure` | A bounded local actor mailbox rejected a message because it was full |

Entries contain a per-actor sequence, sender ID, target behavior ID, payload argument count, and a **redacted** summary marker for nonempty payloads. No message values, string contents, provider credentials, external effects, or heap addresses are captured.

Use `FlightRecorder::ordered_entries()` for chronological order. The buffer overwrites old entries at capacity. The ring is scoped to an actor and does **not** provide an atomic cluster-wide ordering.

## Limits

- Only the local `deliver_local_message` path currently emits these events. Cross-shard channel backpressure, remote delivery rejection, scheduler pauses, GC, workflow recovery, and JIT decisions are **not** currently covered by this recorder.
- The mailbox backpressure event means **admission failed**, not that an application behavior ran or a durable message was persisted.
- A trace is incomplete once events are overwritten, after process exit, or when recording is disabled.
- Recorded event metadata is insufficient for deterministic replay. Recovery must use the authoritative durable workflow/event history and verified semantic identities.
- `NULANG_FLIGHT_RECORDER=1` enables recording on every runtime-created actor; high actor counts and message rates increase memory and CPU usage. Do not enable globally without benchmarking your workload.

## Verification

Relevant focused tests:

```sh
cargo test --locked flight_recorder
cargo test --locked runtime_reports_local_mailbox_backpressure_without_payload_values
```

Production validation should additionally measure disabled/enabled actor throughput, allocation overhead, bounded-ring behavior, and the privacy of exported diagnostics before adding external trace exporters.
