# Nulang Changelog

> This changelog is organized by **stability tier** (see `GOVERNANCE.md` §2),
> not by release. The tier determines what may change and how. The crate
> version in `Cargo.toml` is the implementation version; the
> **language version** (`[package.metadata] language-version`, and
> `LANGUAGE_VERSION` in `src/format/constants.rs`) is what this changelog
> tracks — it moves only on RFC-ratified change.

**Language version:** `1.0.0-frozen` (since 2026-07-19; RFCs 0001, 0002).

---

## Frozen tier

*Will never break. A change here is a new language and requires a new major
version + migration.*

### Language version 1.0.0-frozen — 2026-07-19

- **RFC 0001 — Format Stability.** Established versioned, frozen binary
  formats for durable artifacts and the wire protocol.
  - `.nbc` bytecode artifact format version 1 (magic `NLBC`, header with
    `format_version`, `language_version`, BLAKE3 `source_hash`). Codec:
    `CodeModule::to_nbc` / `from_nbc` in `src/format/nbc.rs`.
  - NUL0 wire protocol handshake version 1 (16-byte
    `{magic "NUL0", version u32, node_id u64}`). Unknown versions are refused,
    never reinterpreted. `src/runtime/network.rs`.
  - Value layout version 1 (`src/value_layout.rs`, i64-tagged).
  - Migration registry `src/format/migrate.rs` as the sole legal home for
    format upgrades. v1→v1 identity.
  - `FormatError` enum: `Truncated`, `BadMagic`, `UnsupportedVersion`,
    `IncompatibleLanguage`, `LengthMismatch`, `UnknownOpcode`, `BodyDecode`,
    `BadConstant`.
- **RFC 0002 — Frozen Core.** Defined Nulang Core, the minimal frozen subset:
  `fn`/`let`/`if`/`match`/closures, `Int`/`Bool`/`String`/`Unit`/`Nil`/
  `Vec`/`Map`/tuples/records/`enum`, HM inference over this subset, `IO.print`
  and `IO.read` only, `val` capability only. Every Core program valid today is
  valid in every future version.
- Stability contract published as `SPEC2.md` §"Format Stability" and
  `GOVERNANCE.md`.

## Stable tier

*Breaking changes require an accepted RFC and a deprecation cycle of at least
two major versions.*

### Added since 1.0.0-frozen — 2026-08-22 (vscode extension)
- **AOT backend error parity** (`src/aot/mod.rs`): the native AOT run path
  now surfaces interpreter-parity runtime errors (48-bit overflow, type
  errors from `pow`/`neg` helpers) instead of silently returning a value.

- **VS Code extension 0.2.0 — language server client** (`editors/vscode/`).
  The extension now activates `nulang --lsp` over stdio and exposes the full
  server surface: diagnostics, hover, go-to-definition, references, document
  symbols, rename, signature help, formatting, semantic tokens, code actions,
  inlay hints, completion, code lens, and document links. New commands:
  **Nulang: Compile** (`--emit-nbc`), **Run**, **Type Check** (`--check`),
  **Restart Language Server**. New `nulang.path` setting (explicit setting >
  `NULANG_PATH` > `PATH`). Integration test suite drives a real VS Code
  instance against the real server (language registration, diagnostics on
  open, hover).
- **TextMate grammar extracted to `nulang-org/nulang-syntax`** — the grammar
  (`source.nulang`) now lives in its own repo (tagged `v0.1.0`) and is
  consumed by the extension as an npm dependency; single source of truth for
  GitHub linguist submission and other TextMate-compatible editors.
- **Publishing pipeline** — `.github/workflows/vscode-extension.yml` builds
  and packages the `.vsix` on PR/main, runs the integration tests, and
  publishes to the VS Code Marketplace and Open VSX on `ext-v*` tags
  (secrets: `VSCE_PAT`, `OVSX_TOKEN`).
- **LSP server stdout purity** (`src/main.rs`, `src/observability.rs`) —
  tracing now writes to stderr, never stdout. `nulang --lsp` previously
  interleaved tower-lsp's error logs into the JSON-RPC stdout stream,
  corrupting framing for every LSP client (any unimplemented request
  produced a non-framed `ERROR ...` line on stdout).
- **LSP: removed false `diagnosticProvider` advertisement**
  (`src/lsp/mod.rs`) — pull diagnostics (`textDocument/diagnostic`) are not
  implemented; advertising them made clients (VS Code's
  vscode-languageclient) send requests that failed with MethodNotFound and
  triggered the stdout corruption above. Diagnostics remain push-only via
  `publishDiagnostics`.

### Added since 1.0.0-frozen — 2026-08-22

- **RFC 0015 phase 1 — structured deprecation warnings**
  (`src/diagnostic.rs`, `src/types.rs`, `src/main.rs`). `catch`/`fail` emit
  W01xx warnings (ariadne-rendered, plain-text fallback) instead of being
  silently accepted; warnings never fail compilation unless `--deny-warnings`
  is passed. `docs/MIGRATION_RFC_0015.md` and `docs/ERROR_CODES.md` document
  the codes and migration path.

- **`--json` structured diagnostics** (experimental; `src/json_diagnostics.rs`,
  schema v1). `nulang --check --json`, `nula build --json`, and
  `nula test --json` emit machine-readable diagnostics (errors, warnings,
  spans with line/column) on stdout while progress stays on stderr; without
  the flag, human output is byte-identical. `tests/cli_json.rs` covers the
  schema.
- **Registry worker version ordering** (tooling): the registry worker now
  sorts package versions with proper semver comparison instead of
  lexicographic string sort, so `1.0.10` lists after `1.0.2`
  (`registry-worker/src/index.ts`).
- **Registry worker semver + quota hardening** (tooling): semver
  comparison moved to a dedicated `semver.ts` (numeric prerelease
  identifiers, ASCII alphanumeric, numeric < alphanumeric, fewer fields <
  more — fixes `0.10.0-alpha.10` < `alpha.9`); chunked PUTs are rejected
  with `411` when `QUOTA_HOOK_URL` is configured so `size_bytes` is always
  the real byte count; the Rust registry server sorts versions
  semver-aware via `package::resolver::parse_semver` (invalid keys last,
  deterministic). 15 new vitest unit/integration tests.
- **Registry seed packages** (experimental; `src/registry`, `src/package`,
  `packages/`). The `nula` package manager ships a set of seed packages
  (`packages/*`) that can be published to a registry and used as
  dependencies:
  - `nula registry seed` installs/registers the seed packages with the
    configured registry (`src/registry/seed.rs`).
  - Bare module imports (`import lib`) resolve against the package's own
    `src/` directory when not found next to the importing file
    (`src/resolver.rs`), so `tests/*.nula` can import the package's modules
    under `nula test`.
  - `--with` capability grants are honored in the `--emit-nbc` path
    (`src/main.rs`), matching `--check`/`--eval`.
  - Seed packages declare no capabilities and run under default-deny.

### Added since 1.0.0-frozen — 2026-08-21

- **Full-stack web framework** (experimental; `src/web`, `src/runtime`,
  `src/package`, `src/stdlib/web`). Adds language support, compiler pipeline,
  and runtime for web applications:
  - `signal name: Type = init` declarations for reactive state
    (`src/parser.rs`, `src/ast.rs`, `src/web/reactivity.rs`).
  - JSX/HTML expression parsing (`<tag attr={expr}>...</tag>`), desugared to
    `el("tag", attrs, children)` with `text("...")` nodes; reserved keywords
    may be used as tag/attribute names.
  - `@nulang/*` package namespace and module graph resolution
    (`src/web/modules.rs`, `src/web/ir.rs`, `src/package/manifest.rs`).
  - `nula build --web` IR generation with capability, middleware, route
    placement, and cloud-config extraction (`src/package/commands.rs`).
  - Server runtime with SSR, HTML host routing, redirects, and reactive signal
    hydration (`src/runtime/http_server.rs`, `src/runtime/callbacks.rs`).
  - Adaptive VM optimizer: cached frame/constant references and inlined hot
    frame opcodes (`Call`, `TailCall`, `Ret`, `RetVal`, `ClosureCall`) in
    `src/vm.rs`.
  - Web standard library modules (`src/stdlib/web/{host,html,realtime,types}.nula`),
    example apps (`examples/{docs-web,hello-web,chat-web}`), shared packages
    (`packages/nulang-auth`), and 18 conformance tests
    (`conformance/behavior/web_*`).
- **Windows x86_64 release binary** (tooling): the release workflow now
  builds `nulang-windows-x86_64.tar.gz` on `windows-latest`
  (`x86_64-pc-windows-msvc`). `build.rs` gates its libpython symlink
  workaround to `cfg(unix)` (the API does not exist on Windows; pyo3
  links the Python import lib directly there). No OpenSSL needed on
  Windows — `native-tls` uses SChannel. `src/main.rs` gates the
  `--bench` stdout/stderr fd redirection (`dup`/`dup2`/`/dev/null`) to
  `cfg(unix)` with a no-op fallback on Windows.
- **VM call-path performance work** (interpreter + JIT tiering):
  - Per-function register counts (`CodeModule.function_local_counts`,
    `src/bytecode.rs`, `src/mir_codegen.rs`): parallel to `function_table`,
    populated with `LOCAL_BASE + locals.len()`; `#[serde(default)]` keeps old
    `.nbc` artifacts deserializing unchanged.
  - `ClosureCall` now copies only the callee's register range instead of all
    256 caller registers (`src/vm.rs`); `Call` already copied only `argc`.
  - Step-limit check batched to every 64 steps instead of every step
    (`src/vm.rs`) — safety limit semantics unchanged (overshoot ≤63 steps).
  - JIT probe cache (`last_compiled_probe`, `src/jit/mod.rs`): skips the
    compiled-region HashMap lookup for sequential execution in hot loops.
  - Measured (release, x86_64): fib(30) 16.81s → 0.56s, call-chain
    (100k × 10-deep) 2.21s → 0.27s.

### Added since 1.0.0-frozen — 2026-08-19
- **Iso arena allocation** (experimental; `src/iso_arena.rs`): arena-backed
  allocation for iso data with qualifying-site analysis over the bytecode
  (may-pointer register sets), plus `alloc_arena`/`reset_arena`/
  `is_arena_ptr` hooks on `ActorVmCallbacks`. Arena objects are reclaimed
  wholesale at activation end.

- **RFC 0016 — Virtual Actor Auto-Hydration and Immutable Shared Object Store**
  (Experimental). Orleans-style virtual actors plus a Ray-style immutable
  object store for large `val` payloads:
  - `virtual entity Name(key: Type) { ... }` declares a grain type; messages
    to `Grain("Name", key)` hydrate the actor on demand
    (`src/parser.rs`, `src/ast.rs`, `src/typechecker.rs`,
    `src/runtime/grain.rs`, `src/runtime/mod.rs`).
  - `Runtime::resolve_or_hydrate_grain` loads snapshots, replays journals, and
    enqueues the grain; `Runtime::dehydrate_idle_grains` persists and
    hibernates idle grains; `Runtime::evict_hibernated_grains` reclaims
    memory while keeping grains addressable.
  - Built-in `Grain.ref`, `Grain.prewarm`, `Grain.pin`, `Grain.unpin` effects
    (`src/runtime/mod.rs`, `src/runtime/callbacks.rs`).
  - Cross-shard grain routing (`stable_id % shard_count`) with identity carried
    in `CrossShardMsg::DeliverMessage` so owner shards hydrate on first
    delivery (`src/runtime/mod.rs`, `src/runtime/distributed.rs`).
  - Per-shard immutable object store (`src/runtime/object_store.rs`) with
    `TAG_OBJECT` value representation and wire-protocol support for `ObjectRef`
    handles (`src/value_layout.rs`, `src/runtime/network.rs`).

### Added since 1.0.0-frozen — 2026-08-17 (perf/optimizations branch)

- **`StrBuilder` builtin effect** — mutable growable string buffer
  (`StrBuilder.new/push/to_string/len/reset`). Appends are amortized O(1) with
  capacity doubling, converting O(n²) text assembly (the `+` concat path) to
  O(n). Wrapped in `stdlib::string` (`builder`/`builder_push`/…). Measured
  ~6× faster than `+` at 30 KB, widening super-linearly. (Experimental)
- **`Map` builtin effect** — mutable open-addressed hash map
  (`Map.new/insert/get/remove/contains/size`). String keys compare by content;
  capacity doubles at 0.5 load; keys/values participate in ORCA reclamation.
  Replaces the O(n) linear-scan `std.map` for keyed workloads. (Experimental)
- **Per-send allocation removal** — `behavior_id_for` and distributed content-
  hash lookup no longer build a `format!(".{name}")` string per send.
- **Actor heap density** — default per-actor bump block 64 KiB → 16 KiB
  (~4× actor density, ≈64k actors/GB). Growth chaining unchanged.
- **Int `**` overflow consistency** — interpreter, JIT, and AOT now all wrap
  on 48-bit int-pow overflow (previously the interpreter wrapped while the
  JIT/AOT compiled helper `nulang_pow` returned `nil` + recorded an arithmetic
  error). The compiled helper now mirrors the interpreter's `step_ipow`.
  Also fixed the AOT unboxed-compilation hazard: `Pow` was missing from
  `is_all_int`'s nil-producing exclusion, so an all-`Int` function compiled
  unboxed, fell through the unboxed binop match to the tagged helper with raw
  operands, and returned `0` for any non-overflow pow (`3 ** 3` → 0). Pow is
  now excluded from unboxed mode; `3 ** 3` → 27 and `3 ** -1` → nil on all
  backends.
- **JIT-compiled direct calls** — hot regions now fold direct calls to
  provably-non-suspending, non-recursive callees and run them via a
  re-entrant helper on the interpreter frame stack, keeping the caller's
  compiled region resident (no per-call region re-entry). Acyclic call-heavy
  loops measured ~46% faster (debug). Recursive and effect-performing callees
  stay on the interpreter by static analysis (`may_suspend` +
  recursion-cycle gates). Foundation: `find_compilable_region_with_calls`,
  `compute_may_suspend`, `compute_recursive` in `src/jit/`.


### Added since 1.0.0-frozen — 2026-08-15
- **E0208 FFI boundary diagnostic** (`src/types.rs`, `src/diagnostic.rs`):
  capability-qualified types at the FFI boundary now report the new
  `E013`/`E0208` error code (`FfiBoundaryViolation`) with `--explain`
  support, replacing the generic error path (`docs/ERROR_CODES.md`).

- **Aether borrow-semantics features (P0–P5).** Six borrows from the
  Aether→Nulang comparison, landed together:
  - **Savina-style benchmark harness** (`src/benchmarks.rs`): counting,
    ping-pong, thread-ring, fork-join, and skynet patterns run on the real
    bytecode VM + actor `Runtime`, asserting correctness while reporting
    throughput and per-message latency.
  - **Resource-capability gate** (`--with=fs,net,os`, `src/effect_checker.rs`,
    `src/main.rs`): grants resource categories checked against the inferred
    effect row (`FS`→fs, `Net`→net, `Env`/`Process`/`System`/`FFI`/`DB`/
    `Python`→os); ungranted resource effects are rejected at compile time.
  - **`hide` / `seal except` scope directives** (`src/lexer.rs`,
    `src/parser.rs`, `src/types.rs`): `hide a, b { body }` and
    `seal except a, b { body }` deny name resolution inside `body`.
  - **`requires` / `ensures` contracts** (`src/parser.rs`,
    `src/typechecker.rs`, `hir`/`mir`): pre/postconditions desugar to a
    runtime `OpCode::Panic` on violation (new `Expr::Panic` /
    `hir::RValue::Panic` / `mir::RValue::Panic`), with `result` bound to the
    return value in `ensures`.
  - **`@derive(eq)` structural equality** (`src/parser.rs`): desugars to a
    `name_eq(a, b) -> Bool` field-comparison function (bare `==` on records
    remains pointer equality).
  - **spawn-near co-location** (`src/runtime/mod.rs`, `src/runtime/spawn.rs`):
    `Runtime::spawn_actor_near(near_id, init)` places a child on the same
    shard as `near_id`, preserving the `actor_id % shard_count` invariant.


### Added since 1.0.0-frozen — 2026-08-14 (docs-truth sweep)

- **RFC 0011 — split-brain resolver (`static-quorum`)** (Experimental,
  `src/runtime/cluster.rs`). `SplitBrainResolver` trait +
  `StaticQuorumResolver` down a node that falls below configured quorum;
  `Failed`-peer probing self-heals a clean partition without external
  rejoin. Down-self + probe re-join covered by DST/chaos scenarios.
  Commits `5a0b641`, `1498cc7` (2026-08-13).
- **RFC 0012 — cross-node link/monitor supervision** (Stable,
  `src/runtime/supervision.rs`). `RemoteLinkRegistry`/
  `RemoteMonitorRegistry` track cross-node watchers; `Packet::Link`/
  `Monitor`/`Down` wire types propagate link/`DOWN` across nodes.
  Commit `0ab2c42` (2026-08-04).
- **Tombstone GC for `ORSet`/`AWORSet`/`RGA`** (Stable,
  `src/runtime/crdt_manager.rs`). Causal-stability watermark
  (`gc_stable_tombstones`) reclaims `removed` sets and RGA tombstones once
  every healthy replica has observed them. Commit `492cd72` (2026-08-08).
- **OpenTelemetry observability + `--metrics-port`** (Experimental,
  `src/observability.rs`, `src/main.rs`). `otel` cargo feature +
  `init_tracing`; `--metrics-port` Prometheus-format server exports
  `GcStats`/`SchedulerStats`/`ResolverStats`/mailbox depths. Commits
  `5d15857` (2026-08-08), `7592b72` (2026-08-13).
- **Runtime observability dashboard demo** (Experimental,
  `deploy/observability/`). Off-the-shelf Grafana pointed at the
  `--metrics-port` Prometheus exporter — `docker-compose.yml` (Prometheus
  + Grafana), a scrape config, provisioned datasource + dashboard
  provider, and a default 9-panel `nulang-runtime.json` dashboard
  (live-actor/DLQ/mailbox gauges + scheduler/GC/resolver counters). No
  bespoke backend, per PLAN.md D18 (2026-08-15).
- **Op-based CRDT replication** (Stable, `src/runtime/crdt_manager.rs`,
  `network.rs`). `Packet::CrdtOp` ships individual `CrdtOp`s alongside
  `CrdtDeltaSync` (lowest-bandwidth sync path); `CrdtManager::apply_op`
  merges inbound ops. Commit `f97b28d` (2026-08-04).
- **`Crdt.*` effect module + per-type operation-set enforcement** (Stable,
  `src/runtime/crdt_manager.rs`, `src/runtime/mod.rs`, `src/stdlib.rs`).
  `perform Crdt.increment/decrement/add/remove/set/read` is the `.nula`-level
  mutation path for `state crdt` fields, validated per type by
  `CrdtManager::apply_field_op` (e.g. `decrement` on a `gcounter` is a
  nil no-op); a raw `self.field = expr` assignment to a crdt field is
  ignored so it cannot orphan `state_data` from the replicated entry.
  The standalone runtime initializes `crdt_manager` eagerly, so `state
  crdt` fields register without distribution. Conformance cases in
  `conformance/behavior/crdt_*.nula` (2026-08-15).
- **Durable-store hardening** (Stable, `src/runtime/persistence.rs`).
  `LibsqlStore` applies `PRAGMA journal_mode=WAL` + operator-configurable
  `PRAGMA synchronous`; `crdt_snapshot` column round-trips save/load;
  `JsonFileStore` fsyncs journal/workflow appends. Commit `42d879d`
  (2026-08-03).
- **Supervisor restart fixes** (Stable, `src/runtime/supervisor.rs`).
  `rebuild_child` recreates the `Supervisor` struct so a nested supervisor
  keeps supervising after restart; `restart_all`/`restart_from` check each
  sibling's `should_restart` (per-sibling rate limit). Commit `22a56c7`
  (2026-08-03).

### Added since 1.0.0-frozen — 2026-08-14

- **`let rec f(x) = ... in ...` works at module level.** Recursive local
  bindings already parsed in expression position; module-level entry
  failed because `parse_module_let` hit the parameter list ("Expected =")
  with the parser already past the `let` token, blocking the expression
  fallback. `parse_module_let` now rewinds to `let` when the name is
  followed by `(`. Pinned by parser + integration tests (PLAN doc-pass
  gap 3 closed).
- **`type X = <full type>` accepts any alias body.** `type Buffer =
  [Int]`, `type T = Int`, `type F = (Int) -> Int`, `type R = &ref Int`
  now parse as aliases (previously "Expected variant name, found [");
  variants (`Some(T) | None`) and records are unchanged. Primitive type
  names lex as `UpperIdent`, so they are routed to the alias path too
  (PLAN doc-pass gap 5 closed). SPEC2 §4.5.1's stale bare-row shorthand
  prose removed — effect rows require braces; `! Type` is the typed-error
  surface (PLAN doc-pass gap 4 closed, doc side).

- **Parameter-level LinearIso must-use verified end-to-end.** 5 new
  conformance cases (cap_30–34) prove exactly-once enforcement for
  `lineariso` function and behavior parameters through the compiled
  binary: single use ok, double use rejected, never used rejected,
  explicit `consume x` discharge ok, behavior-param consume ok.
  Conformance suite: 305/305.

- **Parser fix: `Nil`-led sum types.** The gap-5 type-declaration
  routing sent `Nil` (a primitive type name) to the alias path,
  breaking `type Stream[T] = Nil | Cons(...)` — `Nil` is the canonical
  empty variant of a sum type, not a degenerate alias body. New
  `type_decl_body_is_alias` exempts it; pinned by a parser unit test
  and the generics_07/typeclass_08 conformance cases.

- **Conformance contract updates:** generics_08's stderr assertion no
  longer depends on internal fresh-type-var numbering; workflow_09/11
  expect the deliberate post-2d56e33 contract (failing saga steps
  surface a diagnostic and exit nonzero, compensation trace unchanged).

- **LSP protocol-level integration tests.** 6 tests drive the full
  JSON-RPC dispatch path (`tower_lsp::LspService` with real `Request`
  objects — the same service the stdio server runs), closing the
  "no protocol-level integration tests" gap: initialize capability
  round-trip, publishDiagnostics pushed on didOpen/didChange (empty for
  well-formed docs, parse-error diagnostics for broken ones), hover
  signature, completion keywords, documentSymbol outline, and the
  shutdown/exit lifecycle (requests after exit fail with ExitedError).
  New direct deps `futures`/`tower-service` (both already in the
  lockfile) behind the `lsp` feature.

- **Message-reorder DST scenario.** `NetworkTransport` gains
  `set_reorder`/`flush_held` (default no-ops); the deterministic
  transport delivers consecutive packets to a peer swapped (bounded
  adjacent reorder — nothing lost or duplicated). New 25-seed sweep:
  three nodes form the cluster under reordered heartbeats/gossip/acks,
  a 30-message remote burst delivers exactly 30 (AtMostOnce), and
  GCounter replicas converge under reordered delta sync.

- **GC-during-send DST scenario.** The deterministic scheduler now
  pumps GC on the production cadence (deferred frees mid-run,
  foreign-ref decrements + deferred retry at quiescence) — the DST path
  previously never applied `process_gc_ops`, so heap-churn scenarios
  could not run. New 60-seed sweep: nested heap-array trees sent across
  actors with in-flight foreign bumps, receiver holds, deferred frees,
  and seed-permuted GC interleavings; every seed delivers intact
  contents with exactly the held set of live objects (no premature
  free, no leak).

- **Node-crash DST scenario.** `DeterministicCluster::crash_node`/
  `restart_node` model a hard crash + fresh-node restart (skipped from
  the pump, links cut, Runtime replaced with the same node id). New
  20-seed sweep: survivors mark the crashed node `Failed` through the
  real virtual-clock failure detector, the restarted node rejoins
  through a survivor, the cluster reconverges, and a remote message
  delivers to an actor on the restarted node — the seed-sweepable,
  sleep-free counterpart of the real-TCP crash/rejoin test.

- **CRDT-sync-race DST scenario.** `DeterministicCluster` now drives
  `rt.sync_crdts()` per round (the harness models a Rust embedder — CRDT
  replication stays an embedder API per SPEC2 §12.5, deliberately not
  auto-driven by the production loop). New 40-seed sweep: a GCounter
  minted on node A must appear on node B via the round-1 full-state
  sync, both nodes increment local replicas under seed-permuted
  interleavings, and both replicas converge to the summed total on
  every seed (no lost update).

- **DST seed sweeps are env-scalable; nightly 10⁴-seed job wired.**
  `src/dst.rs::dst_seed_count` reads `NULANG_DST_SEEDS` (defaults:
  2000 single-node, 50 cluster, 30 cross-shard in-suite).
  `.github/workflows/dst-nightly.yml` runs the sweeps at 10⁴ seeds on a
  nightly schedule + manual dispatch, failing loudly on any invariant
  violation (quiescence, AtMostOnce delivery, cluster convergence) —
  the PLAN.md Phase 1 bullet 2 "10⁴-seeds-per-commit" deliverable.

- **Cluster/network determinism (DST).** The deterministic harness now
  drives multi-node clusters of real `Runtime`s with no wall-clock reads
  affecting state: `Runtime::enable_distribution_with_transport` accepts
  any transport (the in-memory `DeterministicNetworkTransport` for tests);
  `ClusterState::set_rng` seeds gossip/repair picks; the deterministic
  scheduler drains cross-shard channels and takes a caller-owned RNG
  (`run_scheduler_deterministic_with_rng`); heartbeat wire timestamps
  come from the virtual clock when one is installed. `DeterministicCluster`
  (test-gated `src/runtime/cluster_dst.rs`) pumps N nodes with lockstep
  virtual clocks and one seed-permuted node order. New tests: same-seed
  bit-reproducible evolution, 50-seed remote AtMostOnce delivery sweep,
  3-node partition→Failed→heal→deliver through the real failure detector,
  30-seed cross-shard delivery sweep. PLAN.md Phase 1 bullet 2 (DST)
  cluster/network determinism closed.

- **Value-level capability constructors for every reference capability.**
  `&cap expr` now constructs a reference with the requested capability for
  all eight capabilities (`&iso`, `&trn`, `&val`, `&box`, `&tag`,
  `&ref`, `&lineariso`, `&linear`); bare `&expr` remains `&ref`
  (backward compatible). Previously `&expr` always produced a `ref`
  reference while `&iso T`/`&val T`/`&trn T`/`&box T` were accepted in
  annotations only — the capability system's biggest missing surface
  (SPEC2 §3.9, PLAN.md "Gaps found by the doc pass" item 1). Semantics:
  the unique constructors (`&lineariso`, `&linear`, `&iso`, `&trn`) move a
  bare-variable operand exactly like `consume x` — a second `&iso x` on the
  same binding is a capability error — while the shared constructors
  (`&ref`, `&val`, `&box`, `&tag`) alias without consuming. Capabilities
  are compile-time only, so every constructor erases to a plain value move
  at runtime (`OpCode::Move`). The formatter now prints `&cap` (previously
  every `&`-expression formatted as `ref`, breaking round-trips). Pinned by
  parser/analyzer/integration tests and differential-corpus entries.

### Added since 1.0.0-frozen — 2026-08-13

- **`lineariso`/`linear` capability annotations now parse.** The lexer emits
  dedicated `LinearIso`/`Linear` tokens, but `parse_capability` (the `:cap`
  annotation path, `src/parser.rs`) only matched them as identifiers, so
  `:cap lineariso` and `:cap linear` always failed to parse. Fixed by
  matching the dedicated tokens directly; pinned by a Rust regression test
  and 8 conformance cases. (The parameter-capability path had the correct
  match all along.)
- **Conformance suite reached 300 behavior cases** (Phase 1 acceptance
  criterion). `conformance/run.py` is green 300/300, including two new
  cases pinning the capability downgrade lattice (`cap_22` iso→trn→ref,
  `cap_23` trn→val).
- **WASM effect-dispatch ABI: `nulang_dispatch` returns the effect-result
  length.** The host import is now `(i32,i32,i32,i32) -> i64` (bytes of the
  effect result written to the ring buffer; 0 = no result), mirroring
  `io_read`'s length-return contract so a guest lowering can read the
  result back from linear memory. Mirrors the pool host side in
  `wasmtime-actor-pool` and the parallel `wasmfx` backend. No compiler
  lowering emits the call yet — effects other than IO.print/println/read
  and `Array.length` are still rejected at compile time.

- **Single-argument `perform Timer.sleep(ms)` in a workflow step no longer
  hangs.** The step used to suspend forever (only the two-argument durable
  form `Timer.sleep(name, ms)` worked). The timer-wheel wake now resumes
  the suspended `PerformAsync` with a full VM resume, and the completion
  bookkeeping (step_index advance, `StepCompleted` event, checkpoint)
  runs exactly like the signal-wait/LLM resume paths. The resume
  distinguishes completion from re-suspension by the VM result, not
  `take_suspended_state` — that accessor returns the completed frame
  state after a normal finish, so a blind re-capture re-stalled the
  actor (the residual hang this fix closes). Pinned by
  `test_workflow_timer_sleep_single_arg_resumes`.
- **Constrained generic functions with typeclass bounds work on
  type-variable receivers.** `fn eq_check[T: Eq](a: T, b: T) -> Bool { a.eq(b) }`
  used to type-check and then crash at runtime ("Not a function: nil") —
  the dictionary transform only resolved literal receivers. The HIR now
  resolves the dictionary for `DictKind::Param` receivers and the call
  site passes the concrete dictionary argument. Pinned by
  `conformance/behavior/typeclass_06_constrained_generic_runtime_crash.nula`.
- **Recursive generic ADTs construct correctly, and generic type
  parameters are skolemized in the function body.** §7.8's
  `type Tree[T] = Leaf | Node((Tree[T], T, Tree[T]))` (and a second
  recursive shape) type-check their own constructor calls, and a body
  that pins its declared type parameter to a concrete type
  (`fn fresh[T]() -> T { 0 - 1 }`) is now rejected at the definition
  instead of failing later at a mismatched call site. Pinned by
  `conformance/behavior/generics_03/07/08_*.nula`.
- **`event_sourced` fields with non-trivial `apply` handlers survive
  crash + recovery.** `emit_event` persists the field's post-apply
  value (apply runs inline before the snapshot) and `recover_actor`
  restores it — recovery no longer reconstructs a bare event count that
  silently drops `apply`'s contributions. Pinned by
  `test_event_sourced_apply_handler_recovery`.
- **Prelude types are now usable in type annotations.** `Ok(42)` and
  `Some(x)` type-checked in every module, but `fn f(x: Option[Int])`
  failed to parse with "Unknown type name" — the prelude's type
  declarations are prepended to the AST only after the user module
  parses, so the parser never saw them. Every `Parser` now seeds the
  prelude's resolved `Option[T]`/`Result[Ok, Err]` into its
  imported-type cache (the same path `import stdlib::*` uses); local
  `type Option[T]` declarations still shadow. Pinned by
  `test_prelude_types_resolve_in_annotations` and
  `test_local_type_shadows_prelude_in_annotation`.
- **Doc-example verification is fully green and covers `///` doc
  comments.** The default `verify_doc_examples.sh` CI invocation was
  red: 16 docs-site blocks taught invalid syntax (pre-`then` `if`
  blocks; `Err(e)` on a prelude whose constructor is `Error(e)`;
  recursive ADT payloads written as bare `List[T]` variant args where
  the parser requires a tuple payload `Cons((T, List[T]))`; an
  unclosed fence in `index.mdx`; and `send x get(self)`, which is
  untypeable because a `ref` capability is not sendable — rewritten as
  `send self` from inside a behavior). All rewritten against the
  current compiler. The script now also verifies every ```` ```nulang
  ```` block inside `///` doc comments of `.nula` sources, pinned with
  a runnable round-trip example in `src/stdlib/json.nula`'s `parse`
  docs. Default run: 54 passed / 0 failed / 0 skipped.
- **A failing workflow step is no longer silent.** Previously a step
  error (e.g. a non-exhaustive match) produced no diagnostic at all —
  no stderr, exit 0 — only a difference in which compensations ran
  revealed it. The runtime now records a durable
  `WorkflowEvent::StepFailed` (with the step name and error message)
  alongside saga compensation, and the CLI prints
  `workflow step '<name>' failed: <error>` to stderr and exits nonzero.
  Pinned by `test_workflow_step_failure_is_recorded_and_surfaced`.
- **Saga compensation and workflow step dispatch no longer shift when a
  plain `actor` is declared before a `workflow` in the same module.**
  Compensation pairs now carry the step's absolute (whole-module)
  behavior index, and a workflow actor's `bytecode_offsets` are
  compressed to its own steps (local ids 0..step_count-1, matching
  `layout_workflow_behavior_table`) instead of the module's full list —
  previously the first step ran the preceding actor's first behavior and
  its compensation was patched onto the wrong behavior, silently. Pinned
  by `test_saga_compensation_ignores_non_workflow_actors`.
- **`spawn@node` references route cross-node by bare actor-ref value.**
  Actor-ref Values carry only a 48-bit id (no node), so a remote-spawn
  handle used to fall into the local mailbox path — messages were
  silently misdelivered/dropped and `ask remote` hardcoded the local
  node. The runtime now keeps a bare id → node reverse index
  (populated at remote spawn, on SpawnResponse, on wire sends, and on
  inbound messages for reply-by-ref), and `send`/`ask` on any known
  remote ref routes over the wire. Messages sent to a spawn@node
  placeholder before its SpawnResponse arrives are queued in wire form
  and flushed to the real actor id; the placeholder value keeps routing
  even after `take_spawn_response` consumes the response. Local actors
  win on id collision (`fresh_actor_id` starts at 1 on every node), and
  `RAsk` now accepts actor-ref targets and stages behavior args like the
  local `Ask` opcode (both were broken). Pinned by three cross-node TCP
  tests plus a strengthened RAsk unit test.
- **`send remote`/`ask remote` now fall back to local delivery
  single-node instead of silently dropping messages, and `ask remote`
  returns the callback's value.** The distribution wrapper resolves a
  remote address to local delivery when the node is local or the
  transport is unwired, and `RAsk` uses the same result-register
  convention as the local `Ask` opcode (previously a register-write
  mismatch returned the wrong value). Pinned by
  `test_distributed_remote_address_local_fallback` and the strengthened
  `test_distributed_callbacks_invoked` (which now asserts the RAsk
  result value). Cross-node routing of `spawn@node` references is the
  companion change above (2026-08-13); the node id is not carried in
  actor-ref values, so routing goes through the runtime's reverse
  index.

### Unchanged at 1.0.0-frozen

The following are classified Stable as of 1.0.0-frozen. They have not changed
in this version; they are recorded here to establish their tier.

- The full HM type system and inference rules (`src/typechecker.rs`).
- The effect-row system: closed/open rows, regions (`src/effect_checker.rs`).
- The capability lattice (`iso`/`trn`/`ref`/`val`/`box`/`tag`/`lineariso`)
  and subtyping (`src/effect_checker.rs`).
- The actor surface: `spawn`, `send`, `receive`, supervision
  (`src/runtime/`, `src/vm.rs`).
- ~~CRDT operations and merge semantics~~ — **correction (2026-08-02):**
  this was misclassified. `src/runtime/crdt.rs`/`crdt_reg.rs` implement a
  real, Rust-level-tested delta-sync CRDT protocol (`CrdtManager`, 8
  types), but it has no `.nula`-observable surface: no type-selector
  syntax, no `Crdt.*` effect module, and the `state crdt` field tag
  (`SPEC2.md` §9.10/§12.5) does not route through it — see those sections'
  implementation-status notes. Nothing Stable-tier is broken by this
  correction because there was never a language-level contract to keep;
  the Rust API itself remains internally stable, it just isn't a
  GOVERNANCE.md-tiered *language* surface until an RFC wires it up.

### Added since 1.0.0-frozen — 2026-07-29

- **RFC 0005/0007 — `entity` keyword and event sourcing.** `entity` desugars
  to `persistent actor` with `event_sourced` default state model. `events`
  and `apply` blocks for typed event declarations and automatic state
  mutation. `emit EventName(args)` type-checked against entity event
  declarations. `after ms => expr` standalone sugar. Entity events validated
  at compile time; unknown events produce type errors.
- **RFC 0008 — Migration contracts.** `version: N` and
  `migration from N to M { ... }` blocks parsed inside entity declarations.
  AST/HIR/bytecode metadata wired through pipeline. Migration state bodies
  and event-migration handlers are now type-checked.
- **RFC 0009 — Organization primitives.** `organization` keyword
  parsed and desugared to `entity` with durable defaults. `is_organization`
  flag tracked through AST → HIR → bytecode.
- **RFC 0003 Item 6 — Backend trait boundary.** `JitBackend`, `WasmBackend`,
  `CryptoProvider`, `ForeignInterop`, `HttpProvider` traits defined in
  `src/backends/mod.rs`. JIT and WASM wired behind traits.
- **MIR register spilling.** Functions with more locals than fit in the
  register file (238 usable registers) now spill excess locals into a
  frame-local `Vec<Value>` via `SpillLoad`/`SpillStore` opcodes (0xF5/0xF6).
  Fix (2026-07-24): replaced post-processing spill rewrite with inline
  SpillLoad/SpillStore emission during codegen, removing the 17-slot
  capacity limit entirely.  Round-robin temp register allocation (r12/r13/r14)
  prevents clobbering in multi-operand spilled reads.  Net -112 lines.
  Unblocks the self-hosting bootstrap compiler (RFC 0003 Item 3).
- **Self-hosting bootstrap: Stage 5 (closures with env capture).** The
  `bootstrap/compiler_core.nula` Pratt evaluator now supports `fn(x) => body`
  lambdas, function application `f(arg)`, and environment capture
  (`let a = 3 in (fn(x) => a + x)(5)` → 8).  Closure encoding: 30-bit flag
  with packed param-hash, body-start, and captured binding.  Out-of-band
  sentinel `1 << 40` distinguishes "no left operand" from value 0.
- ~~Formal semantics: all three Core theorems proved.~~ **correction
  (2026-08-02):** false as of today. This was true at this commit, but the
  very next commit (`ac9ef5d`, 2026-07-26 — Lean 4.16.0 compatibility fix)
  honestly disclosed in its own message that it reverted 9 theorem bodies
  to `sorry` (a custom recursor `weakening` depended on broke under the
  newer toolchain); no doc was ever updated to match. Current state: only
  `canonical_forms` is proved in `types.lean` — `progress`, `preservation`,
  and `type_soundness` (the three headline claims) are all `sorry`. The
  capability lattice proofs (`capabilities.lean`) genuinely are proved (5
  theorems); only `linear_at_most_once` there is `sorry`. `effects.lean`'s
  two theorems are vacuous `True` stubs, not proofs. See
  `spec/formal/README.md` for the corrected, per-theorem scope table.
- **Formal semantics: Core soundness chain re-proved (2026-08-14).**
  `types.lean`'s `progress`/`preservation`/`type_soundness` are machine-checked
  again (the 8 Core `sorry`s regressed by `ac9ef5d` were replaced with real
  proofs). The capability lattice laws, `cap_sendable`, and
  `discharge_sendable` are proved. Two items remain open:
  `linear_at_most_once` (`capabilities.lean`), which requires the split-context
  (input/output) refinement of `HasTypeCap` — the single-context statement is
  false (counterexample documented in-file) — and `effects.lean`'s two
  `effect_safety` theorems, which remain vacuous `True` stubs, not proofs.
  CI sorry-ratchet baseline lowered 9 → 1. See `spec/formal/README.md`.

- **RFC 0013 — Authenticated, encrypted transport (2026-08-05).**
  `TlsConfig` enum (`MutualTls`/`SelfSigned`/`PlaintextInsecure`) replaces
  the opt-in `Option<TlsConfig>`. MutualTLS nodes present certificates
  signed by a cluster CA, verify peer certificates, and derive node
  identity from the certificate's BLAKE3 fingerprint instead of the
  spoofable socket-address hash. `server_name` field for configurable
  TLS SNI. Short read timeout (50ms) with `WouldBlock` retry enables
  concurrent read/write over TLS connections, so heartbeats and gossip
  flow. Integration tests cover connection, cert mismatch rejection,
  plaintext-mTLS interop rejection, and two-node cluster convergence.
  NUL0 wire protocol unchanged (version 1). `src/runtime/network.rs`,
  `src/runtime/cluster.rs`, `src/runtime/tests.rs`.

- **Error handling syntax.** `catch expr => body`, `fail expr`
  (structured short-circuit return), and `T ! E` return-type syntax
  (`fn div(a: Int, b: Int) -> Int ! String`). Errors propagate through
  `?` operator — `expr?` is sugar for `catch expr => |e| fail e`.
  Desugaring, type inference, and codegen wired in `src/parser.rs`,
  `src/typechecker.rs`, `src/hir_lower.rs`, `src/mir_lower.rs`, and
  `src/mir_codegen.rs`.
- **Transport resilience.** `send remote` and `ask remote` keywords
  enforce network-sendable (`val`/`tag`) capability constraints at the
  call site. `ask remote actor behavior(args) timeout N` accepts an
  optional `timeout` clause for request-response with deadline semantics.
  Capability enforcement lives in `src/effect_checker.rs`; transport
  modifiers parsed in `src/parser.rs`.
- **RFC 0010 — 100-Year Language Architecture.** Documented design rationale
  for multi-century relevance. Deliverables implemented:
  - **LLM→Inference effect alias:** `perform LLM.ask(p)` and
    `perform Inference.ask(p)` are synonyms; both resolve to
    `Effect::Inference`. The `LLM.ask` surface is a deprecated alias
    (`src/effect_checker.rs`, `src/mir_lower.rs`, `src/runtime/mod.rs`,
    `src/stdlib.rs`).
  - **Keyword lifecycle governance:** `GOVERNANCE.md` §2a defines keyword
    introduction, reservation, deprecation, and removal rules.
  - **Keyword namespace cleanup:** Five formerly-reserved keywords
    (`where`, `priv`, `loop`, `node`, `subworkflow`) removed from the
    lexer and now lex as plain identifiers. `await` re-reserved (July
    2026) for future async/await support (`src/lexer.rs`).
  - **Keyword inventory documented** in `SPEC2.md` §Implementation Status
    and verified against the implementation.

- **AI façade removal (2026-08-02).** Deleted the `src/ai/` façade module
  (`mod.rs` re-exports + `runtime_impls.rs`) so the crate boundary is
  visible at every callsite. Core now imports directly through
  `use nulang_ai::…;`, never through `crate::ai::`. `AiRuntimeRegistry`
  (pipelines + debates) and `SupervisorTeamRegistry` moved from
  `src/runtime/{ai_registry,supervisor_registry}.rs` to
  `crates/nulang-ai/src/registry.rs`; `SupervisorTeamRegistry::run` gains
  a trait-generic signature (`R: SupervisorRuntime`) matching
  `AiRuntimeRegistry::run_pipeline`. Trait impls for `Runtime` move to
  `src/runtime/ai_impls.rs` where the orphan rule requires them. `LlmState`
  stays in `src/runtime/llm.rs` because it is executor infrastructure
  (persistent worker thread + channels polled by the scheduler), not a
  library type. Net effect: `src/ai/` no longer exists; core imports
  `nulang_ai::` directly; the two-crate split is explicit at every
  callsite. No behavior change.

- `ai-runtime` feature: the AI runtime — **pure types live in the `nulang-ai`
  workspace crate** (`crates/nulang-ai/`) with zero dependencies on the core
  language crate. Core imports them directly via `use nulang_ai::…;` behind
  `#[cfg(feature = "ai-runtime")]` — there is no façade module. All AI
  effects dispatch through the generic `PerformAsync` opcode (`0xC6`) with
  `effect_op` strings (`"Inference.ask"`, `"Pipeline.run"`, etc.). The
  monolithic AI opcode range (0x9D–0xC5: `LlmAsk`, `PipelineNew`…`DebateRun`)
  has been removed. Runtime integration lives in `src/runtime/ai_impls.rs`
  (trait impls for `Runtime`), `src/runtime/agent.rs` (agent LLM completion
  pipeline), and `src/runtime/llm.rs` (`LlmState` worker thread). Behind
  `--features ai-runtime` (enabled by default). `LLM.ask` is a deprecated
  alias for `Inference.ask` and emits a compiler warning (RFC 0010).
- `python` feature: PyO3 interop (`src/python/`). Behind `--features python`.
- `sqlite` feature: libsql/Turso persistence. Behind `--features sqlite`.
- `lsp` feature: the tower-lsp language server (`src/lsp/`). Behind
  `--features lsp`.
- `ai-runtime` feature: the AI runtime (`crates/nulang-ai/` workspace crate,
  imported directly through `use nulang_ai::…;` — no façade module) — LLM
  providers (OpenAI, Ollama), pipelines, debates, supervisor teams, memory
  subsystems, and usage tracking. Behind `--features ai-runtime` (enabled by
  default). **Changed in 1.0.0-frozen:** all AI effects now dispatch through
  the generic `PerformAsync` opcode (`0xC6`) with `effect_op` strings
  (`"Inference.ask"`, `"Pipeline.run"`, etc.). The dedicated `LlmAsk` opcode
  and the `PipelineNew`…`DebateRun` opcode range (0x9D–0xC5) have been
  removed. AI types live in the `nulang-ai` crate with zero core
  dependencies; the core `ActorVmCallbacks` trait no longer carries
  AI-specific methods. The `LLM` effect redirects to `Provider.ask` under
  the hood.
- AOT native backend (`src/aot/`), JIT tiering (`src/jit/`).

- **Stdlib modules.** Standard library modules provide reusable generic
  data structures and operations: `stdlib::core` (base utilities),
  `stdlib::list` (map/filter/fold/reverse), `stdlib::string`
  (split/join/trim/replace), `stdlib::set` (add/remove/contains/union/
  intersect), `stdlib::map` (insert/get/remove/keys/values), and
  `stdlib::http` (get/post request builders). Modules live under
  `src/stdlib/` and are resolved via `NULANG_STDLIB`, the executable-
  relative path, or the dev-fallback `src/stdlib/`.
- **Typeclass declarations (Phase 4).** `class` and `impl`
  keyword support: `class Eq[T] { fn eq(self: T, other: T) -> Bool }`
  declares a typeclass with optional superclasses (`class Ord[T]: Eq`).
  `impl Eq Int { fn eq(self: Int, other: Int) = self == other }` registers
  a concrete instance. Class/instance tables in `TypeChecker`
  (`src/typechecker.rs`). Typechecker integration (dictionary-passing
  transform): method calls on concrete types (`1.eq(2)`) resolve through
  the instance table and type-check against the impl dictionary; missing
  instances (`"hi".eq("there")` with no `impl Eq String`) produce
  compile-time errors. HIR lowering for runtime dictionary construction
  is implemented: `Decl::Impl` lowers to `hir::Decl::Constant`, producing
  a module-level function that evaluates to a record of method closures.
  Field access routing through the dictionary at call sites is
  implemented: method calls on concrete types (`1.eq(1)`) lower to
  dict-constant calls, field accesses, and method invocations at the
  HIR level, producing correct runtime results. Full end-to-end
  verified with integration tests.
- **RFC 0003 — Content-addressed functions.** Proposal document
  (`RFC/0003-content-addressing.md`): defines a deterministic
  content-hash-based code identity scheme for distributed code
  deployment, cache invalidation, and reproducible builds across
  heterogeneous Nulang runtimes. Status: Draft. Content hashing
  infrastructure (BLAKE3 `source_hash` in `.nbc` artifacts) is
  available per RFC 0001; full code-identity registry and
  content-addressed deployment are not yet implemented.
- **`::` import resolution.** Module imports now support `::`-delimited
  paths: `import stdlib::set`, `import mypkg::utils::math`. The resolver
  (`src/resolver.rs`) maps `stdlib::*` prefixes to the standard library
  directory and general `::` paths to filesystem-relative module files.

### Added since 1.0.0-frozen — 2026-07-30
- **Triple-quoted strings and `\u{...}` escapes.** Triple-quoted multi-line strings (`\"\"\"...\"\"\"`) and `\u{...}` unicode escape sequences implemented. Triple-quoted strings support standard escapes; interpolation is unsupported. Surrogate and out-of-range code points are rejected with a `LexError`. Implementation: `src/lexer.rs`. (Stable)

- **`**` exponentiation operator.** Right-associative, precedence above `*`
  (Pratt level 13), tokenized as `Star2`. Wired through the full pipeline:
  lexer (`src/lexer.rs`), parser (Pratt `PREC_EXP`), typechecker, HIR
  lowering, and bytecode. `a ** b ** c` parses as `a ** (b ** c)`.
- **Structured error messages.** `NuError` enum in `src/types.rs` with
  per-variant `expected`/`found` fields, `ErrorCode` classification,
  automatic fix suggestions (`suggestion()`), and `format_rich()` for
  colorized multi-line diagnostics with source excerpts and carets.
  Constructor helpers (`type_mismatch`, `missing_effect`, etc.) produce
  rich errors with minimal boilerplate at each call site.
- **Language correctness fixes** (all Stable, `src/`):
  - *Let-chain stack overflow:* long chains of consecutive `let` bindings are
    now flattened iteratively in the parser (sequential `let`-statement
    peeling) and HIR lowering (`lower_let_chain`), eliminating deep-recursion
    overflow on blocks with 40+ lets (`src/parser.rs`, `src/hir_lower.rs`).
  - *Spawn field-initializer overrides:* `spawn A { f = v }` now correctly
    overrides the actor's declared default for field `f`. Overrides are
    encoded in bytecode (`spawn_init_overrides` in `CodeModule`) and applied
    at VM spawn time, replacing any matching default (`src/vm.rs`,
    `src/mir_codegen.rs`, `src/bytecode.rs`). Backward-compatible: older
    `.nbc` artifacts missing the field deserialize with an empty vec via
    `serde(default)` (`src/format/nbc.rs`).
  - *Clearer immutable-binding error:* the type error for reassigning a
    `let` binding (`"cannot assign to immutable binding 'x'; mutable locals
    (var) are not yet supported. Use 'let x = <new value> in ...' to shadow
    the binding."`) now explains the constraint and suggests the shadowing
    workaround (`src/typechecker.rs`).
  - *Prefix `catch` syntax:* `catch expr fallback` is now accepted in
    addition to the postfix form `expr catch fallback`; desugars identically
    (`src/parser.rs`).
- **Package manager subcommands** (Experimental, `src/package/commands.rs`):
  `nula init` (scaffold a package with `Nulang.toml`, `src/main.nula`,
  `.gitignore`), `nula list` (print locked dependencies), `nula clean`
  (remove `.nbc` build artifacts), `nula add <name> [--path|--git|--version]`
  (add/update a dependency and re-resolve the lockfile), `nula remove <name>`
  (remove a dependency and update the lockfile), `nula run --watch` /
  `nula watch` (build, run, and re-run on source changes via mtime polling),
  and `nula doc [--open]` (generate Markdown API docs from doc comments and
  declarations).
- **REPL enhancements** (Experimental, `src/repl.rs`): `:help <topic>`
  (topics: syntax, types, actors, effects, commands), `:load <file>` (load
  and evaluate a `.nula` file), `:type <expr>` (show the inferred type
  without evaluating), tab completion (identifiers, keywords, REPL
  commands, stdlib modules), and automatic multi-line input when
  braces/parens/brackets are unclosed (prompt changes to `.... `).
- **New stdlib modules** (Experimental, `src/stdlib/`):
  - `result`: Result combinators (`unwrap`, `map`, `flat_map`). The `Result`
    type (`Ok(T) | Error(E)`) is defined in `stdlib::core` (auto-loaded).
  - `option`: Option combinators. The `Option` type (`Some(T) | None`) is
    defined in `stdlib::core`.
  - `datetime`: `DateTime` record type with calendar fields.
  - `math`: trigonometry (`sin`, `cos`, `tan`, `asin`, `acos`, `atan`,
    `atan2`), logarithms (`ln`, `log2`, `log10`), power/root (`pow`, `sqrt`),
    rounding (`ceil`, `floor`, `round`, `trunc`), constants (`PI`, `E`).
  - `fs`: wrapper functions around the `FS` built-in effect (see below).
  - `test`: assertion helpers powered by the `Test` built-in effect (see below).
- **`FS` filesystem effect** (Experimental). Built-in effect wired into the
  standalone VM: `perform FS.read(path) -> String`, `perform FS.write(path,
  content) -> Unit`, `perform FS.append(path, content) -> Unit`,
  `perform FS.exists(path) -> Bool`. Effect-aware type signatures (`!
  {FS}`) are enforced. Declared in `src/stdlib.rs`; wrapper functions in
  `src/stdlib/fs.nula`.
- **`Test` assertion effect + `nula test` runner** (Experimental).
  `perform Test.assert(cond, msg)`, `perform Test.assert_eq(a, b)`,
  `perform Test.assert_true(cond)`, and `fail_with(message)`. The test runner
  (`nula test [--filter <substr>]`) discovers `.nula` test files under the
  package's `tests/` directory, executes each, and reports pass/fail counts
  with optional name filtering (`src/stdlib/test.nula`,
  `src/package/commands.rs`).
- **LSP enhancements** (Experimental, `src/lsp/mod.rs`): `.` and `::`
  completion trigger characters for automatic invocation, field-access
  completion (on `self.` fields, record fields, and actor state),
  `textDocument/didSave` handler that re-checks the file on save, and
  completion items sorted by category (locals > functions > types > variants
  > keywords > effects) via `sort_text` prefixes.
- **Example programs.** 15 verified, runnable example programs under
  `examples/` with `examples/README.md`: from basic IO and arithmetic
  through functions, pattern matching, records, higher-order functions,
  algebraic effects, actors, loops, the pipe operator, arrays, JSON
  parsing, HTTP requests, Option/Result combinators, and range expressions.

- **`var` bindings** (Experimental). Mutable local variables via `var x = 0`
  (declaration) and `x = x + 1` (reassignment). `var` bindings are tracked
  separately from `let` in the typechecker and codegen, producing `Store`
  and `Load` bytecode ops for mutation — `src/parser.rs`,
  `src/typechecker.rs`, `src/mir_codegen.rs`.
- **Record-update syntax** (Experimental). `{ base .. field = value }`
  creates a new record with overridden fields. The `..` is parsed with
  `PREC_RANGE` precedence; the parser disambiguates record-update from
  range-in-block by checking for `=` after the right operand —
  `src/parser.rs`.
- **Tuple field access** (Stable). Numeric indices on tuples: `t.0`, `t.1`.
  Chained access (`t.0.1`) works directly on nested tuples without
  parenthesization — `src/parser.rs`, `src/hir_lower.rs`.
- **Range expressions** (Experimental). `a .. b` produces an inclusive-
  exclusive range at `PREC_RANGE` precedence (level 3, between pipe and
  logical-or). Ranges work in `for` loops (`for i in 0 .. 5 { … }`) and
  can appear bare in blocks (`{ a .. b }`) — `src/parser.rs`.
- **Language correctness fixes** (all Stable, `src/`):
  - *`else`-on-newline:* an `else` keyword following a newline after `}` is
    now accepted in `if`/`else` chains — `src/parser.rs`.
  - *`String.+` fix for variables:* `a + b` where both operands are
    `let`-bound string variables now correctly concatenates instead of
    returning `0` — `src/vm.rs`.
  - *`let..in` scoping fix:* block-level `let x = V in BODY` now correctly
    scopes `x` to `BODY` only, not to the remainder of the enclosing block
    — `src/hir_lower.rs`.
- **`String.from_char`** (Stable). `perform String.from_char(code)` creates
  a single-character string from a Unicode code point; returns `nil` for
  invalid code points (surrogates, out of range) — `src/stdlib.rs`,
  `src/vm.rs`.

- **`Http` builtin effect** (Experimental). `perform Http.get(url)` and
  `perform Http.post(url, body)` wired into the standalone VM via `ureq`.
  Returns the response body as a `String` on success, `nil` on error —
  `src/stdlib.rs`, `src/vm.rs`.
- **`Array` builtin effect** (Experimental). `perform Array.length(arr)`,
  `perform Array.push(arr, elem)`, `perform Array.new(n, init)`,
  `perform Array.set(arr, idx, val)`, and `perform Array.slice(arr, start, end)`
  wired into the standalone VM with value semantics (all return new arrays) —
  `src/stdlib.rs`, `src/vm.rs`.
- **Numeric conversion primitives** (Experimental). `Int.to_float`,
  `Float.to_int` (truncates toward zero), `Float.to_string`, `String.to_int`
  (returns 0 for invalid input), and `String.to_float` (returns 0.0 for
  invalid input) — `src/stdlib.rs`, `src/vm.rs`.

- **JSON parser** (Experimental). Pure-Nulang recursive-descent JSON parser
  in `stdlib::json`: `parse(json: String) -> JsonValue` handles all JSON
  value types with proper escape processing, and `stringify(value: JsonValue)
  -> String` produces valid JSON output. Uses `String.to_float`,
  `Float.to_string`, `String.from_char`, and `Array.*` primitives —
  `src/stdlib/json.nula`.
- **All 13 stdlib modules functional** (Experimental). `core`, `list`,
  `string`, `set`, `map`, `test`, `fs`, `option`, `result`, `datetime`,
  `math`, `json`, and `http` all parse, import, and resolve correctly with
  all VM primitives available — `src/stdlib/`.

- **LSP: code lenses, document links, enriched hover** (Experimental,
  `src/lsp/mod.rs`): `textDocument/codeLens` shows reference counts above
  function/actor declarations; `textDocument/documentLink` creates
  clickable links from `import` statements to resolved module files;
  `textDocument/hover` now includes doc comments (extracted from preceding
  `///` lines), effects, and formatted type signatures.
- **LSP: completion documentation** (Experimental, `src/lsp/mod.rs`):
  keyword and built-in effect completion items now carry markdown
  documentation strings with code examples in their `documentation` field.
- **Bootstrap: curried closure capture** (Experimental, `bootstrap/compile_hex.nula`):
  The bootstrap bytecode compiler now correctly compiles curried functions with
  closure capture — `(fn(a) => fn(b) => a + b)(1)(2)` → 3. Fixed swapped
  CapStore/CapLoad opcodes at body start and fn_end, added missing Move for
  captured parameter at definition time, and corrected the environment register
  mapping from the raw capture register to r11.

### Added since 1.0.0-frozen — 2026-08-09

- **`Http.serve` works in the standalone (actor-free) VM** (Stable, `Http`
  effect): previously only the runtime-backed callbacks handled `serve`,
  so an actor-free program (`nulang file.nula` with no actor decl) got
  "Unhandled effect". `StandaloneVmCallbacks` now dispatches `Http.serve`
  with the handler's module + function-table index, binding
  `HttpServerState` directly; the server is leaked so it keeps serving for
  the process lifetime. Regression test `test_http_serve_standalone`
  proves an actor-free program binds a port and serves a request
  end-to-end. Note: a pure standalone `Http.serve` program still exits
  when `main` returns (the process dies), so run it from a runtime-backed
  or blocking program.

- **`nula new --template` library grows to 7 templates** (Experimental,
  package manager): adds `distributed` (spawn + message-passing worker
  actors), `ai-agent` (actor backed by `perform Inference.ask`), and
  `web` (HTTP client via `Http.get`/`Http.post` with JSON). Each
  validated end-to-end via `nula new` → `nula run`. The planned
  `Http.serve`-based server template is deferred pending a CLI dispatch
  fix (see PLAN.md Phase 4 D6).

- **RFC 0014 — durable-actor re-spawn on node failure (Stable)**:
  implemented 2026-08-15. The confirmed-gone gate (`Removed` membership
  state promoted from `Failed` past `removal_confirmation_timeout` under
  quorum, or immediately on a positive `Packet::NodeGoodbye`), a
  gossip-replicated durable-actor location directory
  (`DurableDirectoryEntry`, highest-epoch-wins) with epoch-based
  self-demote (no two live copies), shadow-node snapshot replication at
  `checkpoint_actor` (new `Packet::ShadowReplicate`, re-spawned through the
  existing `receive_migrated_actor`), the `RestartPolicy::RespawnOnNodeLoss`
  supervisor policy (`.nula` `Otp.supervise_child` policy `3`), and the
  goodbye path (checkpoint + terminate before declaring dead). Re-spawn is
  opt-in per supervision edge; default supervision is unchanged. Deliberately
  not included: Raft/consensus (standing deferral) and silent automatic
  re-spawn without an explicit policy.

- **Node-death recovery (Stable, distributed runtime)**: when the failure
  detector declares a peer node `Failed`, the local runtime now invalidates
  that node's `RemoteActorCache` entries (sends fail fast instead of
  stale-resolving) and delivers `DOWN`-with-`noconnection` system messages
  to every local actor that had linked or monitored an actor on the dead
  node. New `ExitReason::NoConnection` (wire tag `noconnection`, DOWN
  payload code 6) distinguishes node loss from a crash. Inbound
  `Packet::Link`/`Monitor` now register remote watchers and inbound
  `Packet::Down` delivers DOWN to local watchers (previously dropped).
  Supervisor-policy re-spawn of durable actors on another node remains
  intentionally unimplemented pending the old-node-confirmed-gone gate.

- **Formatter completeness** (Experimental, `src/fmt.rs`): `nulang fmt` now
  formats every `Decl`/`Expr` construct instead of refusing files containing
  `workflow`, `agent`, `class`, `impl`, `let`-binding, `given`, `effect`,
  `module`, `import`, `extern`, `database`, `crdt`, `state_machine`, named
  handler, or `record` declarations, or `spawn`/`handle`/`receive`/`emit`/
  `migrate`/`cap-annotate`/`type-annotate` expressions. Output is canonical
  and idempotent (reformatting is a no-op). Class/impl method params and
  returns use the parser's `Unit`/bare-`Type::Var` omitted-annotation
  sentinel and are skipped rather than emitted as spurious `: Unit`.
  Added `CrdtType::keyword()` (inverse of `from_keyword`). 9 new unit tests;
  all 33 `examples/*.nula` format without errors and re-parse.

- **RFC 0003 Item 14 — transport hygiene complete.** `quinn` removed
  entirely (no `quinn` dep, `quic_transport.rs` deleted). `reqwest` and
  `rustls` are confined to their composition-root trait impls:
  `ReqwestHttpProvider` (the `HttpProvider` impl) in `src/backends/mod.rs`
  and `rustls` inside `src/runtime/network.rs` (the `NetworkTransport`
  impl). `Runtime` holds `http: Box<dyn HttpProvider>` (default
  `ReqwestHttpProvider`) delegating through `http_post_json`/`http_get`;
  `Transport` blanket-impls over `NetworkTransport` (already
  `Box<dyn NetworkTransport>`). No core-language file imports
  quinn/rustls/reqwest directly — a 2125 runtime can swap the transport
  and HTTP client without touching the language.

- **Bootstrap Stage 2: multi-fn programs + recursion through the
  self-hosting pipeline** (Experimental, `bootstrap/`): `verify.sh` check 6
  proves whole-program compilation — `desugar_fns.py` lowers top-level `fn`
  definitions into a let-binding chain, `compile_hex.nula` compiles it to
  hex, and the VM runs the resulting `.nbc` (multi-fn `add(double(3))` → 7).
  Recursion also works through the pipeline (`let fib = fn(n) => ... in
  fib(10)` → 55). 11/11 checks pass. Documented remaining blocker: the
  3-argument `nperform` path (`String.charAt`, 2 args) corrupts its
  effect-name constant due to the host compiler's MIR register-spill bug
  (`compile_hex.nula`'s `comp` has 178 locals; `src/mir_codegen.rs`).
  `String.length` (1-arg) and `IO.print` work.

- **Bootstrap self-hosting pipeline verified end-to-end** (Experimental,
  `bootstrap/`): `compile_hex.nula` (a Nulang Core program) compiles Core
  source → hex bytecode; `fixup_hex.py` patches jump/constant/closure
  offsets; `hex2nbc.py` emits a runnable `.nbc`; the VM executes it — a
  Nulang program compiling Nulang Core with no Rust compiler in the loop
  (RFC 0003 Item 3, Stage 1→2 bridge). `bootstrap/verify.sh` gained a
  pipeline check (5 expressions: arithmetic, `let`, `if`, `not`, closure
  application) and now supports `NULANG_BIN=` to skip the `cargo run`
  rebuild. Fixed the `false` keyword hash bug in `compiler_core.nula` and
  `compile_hex.nula` (`read_ident` returns the low-16 hash; `false` = 13715,
  not 79251, so the literal was never recognized — bare `false` → nil,
  `not false` → false). Verified: 20-expression oracle comparison against
  the Rust compiler, all matching; `verify.sh` 9/9 checks pass.

- **Debug Adapter Protocol server** (Experimental, `src/dap/`, `--dap`).
  `nulang --dap` speaks DAP over stdio (the same `Content-Length` framing as
  the LSP) so editors such as VS Code can debug `.nula` programs: source
  breakpoints, continue/step-in/step-over/step-out, pause, stack traces
  (frames + source lines), scopes, local-variable inspection, and
  `evaluate` (local lookup + literals). Architecture: a reader thread
  parses framed requests; a server loop dispatches them; a dedicated
  debuggee thread owns the VM with a `DebugHook` invoked before every
  interpreted instruction (JIT disabled while attached) that returns the
  `DebugPause` sentinel to stop. The v1 debuggee runs on the **standalone
  VM** — top-level code, functions, closures, and effect handlers
  (`IO.print`/`IO.read`) are fully debuggable; actor `spawn`/`send`/
  `receive` are no-ops, matching the standalone VM's outside-an-actor
  contract. Program stdout is captured and forwarded as DAP `output`
  events so it never corrupts the DAP stream. In-process test harness:
  `run_dap_server_io` over arbitrary buffers.
- **Debugger line table & per-function debug info** (Experimental). The MIR
  pipeline now records a source-line map (`CodeModule.line_table`: bytecode
  pc → 1-indexed line, one entry per source statement) and per-function
  metadata (`CodeModule.debug_functions`: name, code range, named locals
  with their registers). `mir_lower` threads each `hir::Stmt` span into the
  `FunctionBuilder`; `mir_codegen` translates statement indices to bytecode
  PCs. Both fields are additive `serde(default)`, so pre-existing `.nbc`
  artifacts deserialize unchanged.
- **`par { ... }` independence annotation** (Experimental). `par { e1; e2;
  ... }` declares that the sub-expressions have no data dependencies on
  each other. Semantics are identical to a sequential `Block` (evaluated in
  order, last expression wins); the distinct `Expr::Par` node is preserved
  through the frontend so later passes can exploit the independence (e.g.
  parallel lowering/codegen), mirroring nanolang's `par` block. Wired
  through lexer, parser, typechecker, effect checker, capability analyzer,
  HIR lowering, formatter, and LSP.

### Added since 1.0.0-frozen — 2026-08-09 (infra session)

- **`.nbc` export table** (Stable, `src/bytecode.rs`). `ExportTableEntry`
  struct (name/kind/index/type_sig) added to `CodeModule` with
  `#[serde(default)]` for backward compatibility. `add_export()` convenience
  method. Consumers can link against library exports with full type
  signatures. (RFC 0003 Item 17)
- **`CodeModule::from_bootstrap_json()`** (Stable, `src/bytecode.rs`).
  Parses the bootstrap emitter's JSON format into a runnable `CodeModule`.
  Accepts hex instruction strings, typed constants (Int/Float/Bool/String),
  and export table entries. (RFC 0003 Item 3)
- **Bootstrap self-hosting pipeline** (Experimental, `bootstrap/`).
  Stage 1 emitter (`emitter.nula`) outputs structured JSON for 3 Core
  programs (literal, add, conditional). Host converter roundtrips through
  `.nbc`. End-to-end integration test verifies the full pipeline.
  (RFC 0003 Item 3)
- **WASM Component Model WIT generator** (Experimental, `src/witgen.rs`).
  Maps 5 built-in Nulang effects (IO, Timer, Random, Signal, Provider) to
  WASI 0.2+ WIT interfaces. `extract_effects_from_source()` scans for
  `perform Effect.op(...)` patterns. `--backend wasm-component` CLI flag
  writes `.wit` alongside `.wasm`. (RFC 0003 Item 16)
- **Formal semantics in Lean 4** (Experimental, `spec/formal/`).
  6 modules formalize the Nulang Core type system: `Types.lean` (type
  language, substitution, mgu), `Capabilities.lean` (capability lattice,
  subtyping, join, sendability), `Effects.lean` (effect rows, subsumption,
  union), `Syntax.lean` (Core expression AST, free vars, capture-avoiding
  substitution), `Typing.lean` (typing context, judgment Γ ⊢ e : τ).
  Soundness theorems (Substitution Lemma, Preservation, Progress, Type
  Soundness) are machine-checked in the top-level `types.lean` (2026-08-14).
  `lake build` passes.
  (RFC 0003 Item 2)
- **`.nbc` dependency type in `nula` package manager** (Experimental,
  `src/package/`). `nbc` field in `Nulang.toml` `[dependencies]`.
  `PackageSource::Nbc` variant with full resolver pipeline (lockfile,
  content hash, dedup). (RFC 0003 Item 17)
- **Distributed trace context propagation** (Stable, `src/runtime/`).
  `trace_id: Option<String>` on `Message` struct, propagated from wire
  (`Packet::ActorMessage`) through cross-shard delivery to local send.
  (RFC 0003 Item 15)
- **Backend trait wiring** (Stable, `src/backends/`). 8 backend traits
  (JitBackend, WasmBackend, ForeignInterop, StorageBackend, Transport,
  CryptoProvider, HttpProvider, TlsProvider) fully trait-erased from the
  core language. `create_default_jit()` factory in `src/backends/mod.rs`.
---

## Pre-1.0 (crate version 0.13.0-alpha.1 and earlier)

No stability promise. The 0.x series is the alpha development track. Language
version 1.0.0-frozen is the first version with a published stability contract;
everything before it is implicitly Experimental.
