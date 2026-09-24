# Nulang Implementation Status

**Updated:** 2026-09-23  
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
- Actor turns adapt their batch size to runnable-peer pressure while retaining
  a hard reduction/preemption boundary.
- Each actor's first 16 KiB bump-heap block is allocated lazily on first
  small-object allocation rather than at actor spawn.

Actors support links, monitors, supervision, process groups, priorities,
selective receive, timed receive, and actor-local ORCA memory management.

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
silently emulate it with a sequence of individually visible writes. PostgreSQL
implements the atomic transition/tail-fencing contract. Local durable outbox
delivery can atomically accept a committed message into a workflow receiver
(inbox identity + command journal) and redeliver across the acceptance/mailbox
crash window.

Important boundaries:

- durable outbox delivery is currently proven only for local workflow
  receivers; ordinary actor sends are not yet automatically staged into the
  sender transition, generic persistent actors are deferred, and cross-node
  durable delivery remains incomplete;

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

This is not yet a claim of production-grade transparent clustering. In
particular, automatic virtual-actor activation/placement and the broader target
cluster architecture remain separate work.

## WASM and component-model boundary

Plain WASM is the portable execution target and is hosted with Wasmtime. The
repository also contains a Wasmtime Component Model host runtime and a
Borsh-based component boundary for serializable Nulang values.

Treat Component Model support as an evolving integration boundary, not as proof
that every bytecode/runtime feature is component-portable today.

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
