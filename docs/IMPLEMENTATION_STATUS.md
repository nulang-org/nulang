# Nulang Implementation Status

**Updated:** 2026-10-16  
**Scope:** current `main` implementation, not roadmap promises

This document is the compact status map for contributors who need to know what
Nulang actually implements today. It is intentionally narrower than
`ARCHITECTURE.md` and `SPEC2.md`.

For normative language semantics, use `SPEC2.md`, `GOVERNANCE.md`, and
accepted RFCs. For implementation truth, source code and executable tests win.
`CHANGELOG.md` records landed behavior. Open pull requests and roadmap
documents are not treated as shipped features here.

## Semantic model

Nulang is a typed language for concurrent, durable, and distributed software.

The current semantic direction follows RFC 0024:

- ordinary local computation is ordinary computation, not an actor;
- scoped concurrent tasks and independently addressable actors are distinct
  execution forms;
- actors own isolated mutable state and communicate through messages;
- durability, identity, placement, effects, reference capabilities, and
  external authority are orthogonal properties that may compose with those
  execution forms;
- current `entity` and `workflow` lowering may use actors internally, but
  that implementation choice does not make every durable computation
  semantically an actor.

The compiler implements Hindley-Milner inference, row-polymorphic algebraic
effects, variants/records, and Pony-inspired reference capabilities
(`iso`, `lineariso`, `trn`, `ref`, `val`, `box`, `tag`).

## Compiler and execution backends

| Backend | Current role | Status boundary |
|---|---|---|
| Register bytecode VM | Default execution path and semantic reference implementation | Primary |
| Cranelift JIT | Tiers hot bytecode regions into native code while preserving VM semantics | Enabled by `native-codegen` |
| Native/AOT | MIR-to-Cranelift native compilation for supported workloads and differential validation | Secondary / Experimental |
| Plain WASM | Canonical portable/cloud target through MIR-to-WASM and Wasmtime | Experimental; semantic coverage is narrower than bytecode |
| WasmFX | Stack-switching experiment for suspending effects | Experimental |

The frontend is MIR-exclusive:

```text
source
  -> lexer
  -> parser / AST
  -> HM type checking
  -> effect checking
  -> reference-capability analysis
  -> HIR
  -> MIR
  -> bytecode / native / WASM backend
```

Do not claim backend parity unless the relevant conformance and differential
tests establish it. Bytecode remains the semantic reference.

## Actor runtime

The live runtime is sharded and cooperatively scheduled.

- One `Runtime` owns one shard and one live cooperative scheduler thread.
- `NULANG_SHARDS>1` runs multiple shards concurrently on OS threads with
  bounded cross-shard channels.
- The current live per-shard scheduler dequeues through worker slot 0.
  Chase-Lev peer-stealing support exists for alternate/future multi-worker
  callers, but it is not the live per-shard execution topology.
- Actor ids are currently process-generated `u64` values. Nulang does not
  yet provide Orleans-style activation-on-first-message virtual actors with
  stable string identity and consistent-hash placement.
- Spawned actors default to unbounded mailboxes. Mailboxes support separate
  system/local/normal paths, transactional selective receive, and optional
  capacity. Configured normal/bulk capacity produces explicit backpressure;
  system messages bypass that capacity.
- Small messages with 0–4 values are stored inline in the message envelope;
  larger payloads use shared `Arc<Vec<Value>>` storage.
- Ready-queue ownership is deduplicated so bursts of sends do not create
  duplicate ready tokens.
- Actor turns adapt their mailbox batch size to runnable-peer pressure (up to
  16 messages with peers ready, or up to 256 without them) while retaining a
  default 1,000-message-reduction ceiling. This does **not** imply universal
  instruction-level preemption inside long-running behaviors.
- Each actor's first 16 KiB bump-heap block is allocated lazily on first
  small-object allocation rather than at actor spawn.

Actors support links, monitors, supervision, process groups, priorities,
selective receive, timed receive, and actor-local ORCA memory management.

### Delivery and mailbox guarantees

A source-level `send` is fire-and-forget, not proof of receiver admission or
processing. Bounded local mailboxes can reject normal/bulk messages with
backpressure; system messages bypass that capacity. Unknown actors/behaviors
and unsupported cross-shard payloads may also be rejected. The local runtime
can place a notification in its dead-letter mailbox, but that notification does
**not** retain the rejected message payload or supply a durable retry queue.
Do not promise lossless, exactly-once, or durable remote delivery without an
application-level acknowledgment, retry, and idempotency protocol.

Strict High > Normal > Low scheduler priority is implemented, but sustained
higher-priority traffic can starve lower-priority actors.

## Durability and persistence

Nulang exposes durable state, journaling, snapshots, workflow history, recovery,
and storage-neutral persistence through `PersistenceStore`.

Current backends:

| Backend | Availability |
|---|---|
| `MemoryStore` | Always available; test/ephemeral durable state |
| `JsonFileStore` | Always available; file-backed development/debugging |
| `LibsqlStore` | `sqlite` feature; libSQL/SQLite and remote libSQL/Turso path |
| `RocksDbStore` | Optional `rocksdb` feature |
| `PostgresStore` | Optional `postgres` feature |

The durability layer includes an atomic durable-transition contract. Backends
that cannot satisfy a required atomic operation must fail closed rather than
silently emulate it with a sequence of individually visible writes.

Important boundaries:

- persistent actors/entities/workflows are implemented and recoverable, but
  the higher-level durable semantic model is still being stabilized;
- deterministic replay requires effect results and identity to be preserved
  explicitly; arbitrary external exactly-once delivery is not claimed;
- migration purity/schema compatibility work exists, but production migration
  guarantees must be read from the current RFCs and `CHANGELOG.md`;
- the eight CRDT implementations exist at the Rust/runtime layer, while
  source-level `state crdt` integration is not yet equivalent to the full
  target semantics.

## Distribution

The distributed actor transport is an Experimental custom TCP protocol.

Each connection:

1. establishes TCP;
2. optionally upgrades to mutual TLS through rustls (or uses the explicit
   insecure plaintext development mode);
3. exchanges a frozen 16-byte NUL0 handshake:
   `[magic "NUL0"][wire_version u32][node_id u64]`;
4. exchanges length-prefixed NUL0 frames.

Each packet frame contains a 4-byte length followed by the 13-byte
NUL0/type/sequence envelope and a type-specific payload.

Implemented distributed/runtime pieces include remote actor routing, gossip
membership, remote spawn negotiation, and CRDT state synchronization.

**Wire-protocol boundary:** This frozen NUL0 v1 peer protocol is **not** the
Nulang Cloud host–guest protocol of the same name. Nulang peers exchange a
16-byte `"NUL0"`/version/node-ID handshake and frame magic/type/u64-sequence
packets. Cloud's `nats-transport/src/nul0.rs` instead uses a length/type/flags/
u16-stream header followed by a CRC32 trailer, without this peer handshake.
The formats must not be interchanged; any future bridge needs an explicit,
versioned adapter and its own conformance tests.

This is not yet a claim of production-grade transparent clustering. In
particular, automatic virtual-actor activation/placement and the broader target
cluster architecture remain separate work.

## WASM and component-model boundary

Plain WASM is the portable execution target and is hosted with Wasmtime. The
repository also contains a Wasmtime Component Model host runtime and a
Borsh-based component boundary for serializable Nulang values.

Treat Component Model support as an evolving integration boundary, not as proof
that every bytecode/runtime feature is component-portable today.

## Experimental Web deployment boundary

The HTTP/Web effect host, compiler-first web framework, browser runtime, and
in-process SSE transport are experimental. Do not treat a successful demo,
static build, or an open hardening pull request as evidence of an audited
Internet-facing server. Before public deployment, validate **the exact code
revision being shipped**, including request framing, Host/origin admission,
response header/cookie sanitization, static-file root containment, bounded SSE
resources, and form/server-action dispatch. A proposed fix is not part of
`main` until merged and validated there. The development server is not a
production-security certification.

## Performance validation

Performance work should be evidence-driven and should not use cross-machine or
unmatched-runtime numbers as language rankings.

Current `main` includes:

- `scripts/nulang_ab_bench.py` and
  `.github/workflows/nulang-ab-bench.yml` for same-runner comparison of a
  candidate against its exact base SHA;
- `benchmarks/cross_runtime/` and `scripts/cross_runtime_bench.py` for
  matched counting, ping-pong, thread-ring, and fork-join fixtures across
  Nulang, Rust standard-library channels, Go channels/goroutines, and
  Erlang/BEAM;
- single-logical-CPU comparison mode to avoid presenting unequal multicore
  scheduler budgets as equivalent measurements.

Benchmark results are workload measurements, not universal runtime rankings.

## Stability summary

Nulang is alpha software.

- **Frozen:** published compatibility contracts such as versioned artifact,
  wire-protocol, and value-layout formats.
- **Stable:** the current pre-adoption semantic surface where governance says
  changes require an RFC/versioned migration path.
- **Experimental:** evolving backends, distributed features, AI runtime,
  Fabric/cache surfaces, and other explicitly marked features.

The repository still contains historical design documents that predate RFC
0021/RFC 0024. When they conflict with accepted RFCs, `SPEC2.md`, current
source, or tests, treat those older documents as historical context rather than
current behavior.

## Contributor verification

Useful local gates:

```bash
cargo test --locked
cargo test --locked --features wasm-backend
python3 verify_implementation.py
```

For backend-specific, minimal-feature, release, and benchmark validation, use
the repository workflows and commands documented in `AGENTS.md` and
`CONTRIBUTING.md`.
