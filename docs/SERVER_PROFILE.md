# Lean self-hosted server profile (experimental)

Nulang's `server` **Cargo feature profile** combines the bytecode VM, actor
runtime, TCP/HTTP, TLS, outbound HTTP and local SQLite/libSQL persistence.
It is intended for applications that want a small **dependency surface**, not
a new runtime, queue, storage engine, or source-language execution model.

The existing default and public release-CLI profiles are unchanged. The new
profile is **opt-in**; specifying `--features server` without
`--no-default-features` still enables the full default dependency graph.

## Build and smoke test

From a checkout with the pinned Rust toolchain installed:

```bash
cargo build --locked --release --no-default-features --features server --bin nulang
cargo run --locked --no-default-features --features server --bin nulang -- --eval 'perform IO.print("server profile OK")'
bash scripts/check_server_profile.sh
```

The last command builds the profile, tests the manifest feature contract,
runs focused activation-epoch and durable-effect persistence tests using
the `sqlite` feature, and audits the *normal/build* dependency graph.

The `server` profile deliberately excludes the Cranelift JIT/native
backend, Python, the AI provider runtime, LSP/editor server, FFI, RocksDB,
PostgreSQL, the standalone RESP cache-server reactor and WASM. A separate
compile/runtime profile can opt into those capabilities when needed.

## Boundaries

- This is a *Cargo build feature*, not yet a self-contained Nulang application
  manifest or one-command production deployment system.
- Disabling native code generation means bytecode execution; it does not
  produce a tiny native standalone user-program executable or imply static
  linking. Separate draft work tracks native executable emission.
- SQLite is a useful local durable store for suitable single-host workloads.
  This does not provide automatic multi-node replication, failover, or
  high-write-concurrency guarantees.
- The profile does not claim that every external effect or source-level
  workflow dispatch is replay-safe. See
  [durability guarantees](DURABILITY_GUARANTEES.md) and the open
  [workflow recovery](https://github.com/nulang-org/nulang/issues/1548)
  and [terminal commit](https://github.com/nulang-org/nulang/issues/1389)
  issues.
- Ordinary remote actor sends are not a durable queue; see
  [delivery semantics issue](https://github.com/nulang-org/nulang/issues/1180).
- Production still requires filesystem persistence policy, backups,
  permission/capability enforcement, TLS termination, secrets, health checks
  and deployment supervision.

## Opt-in SQLite state for routed web services (stacked preview)

The `service` template now demonstrates single-host persistent Web.kv_* state
using the existing libSQL driver and a dedicated `nulang_web_kv` table.
This is **not** an actor/workflow persistence backend replacement.

```bash
nulang nula new my-service --template service
cd my-service
nulang nula dev
# In a second shell:
curl --fail http://127.0.0.1:8787/health
curl -d 'value=hello' http://127.0.0.1:8787/state
curl --fail http://127.0.0.1:8787/state
```

The generated `Nulang.toml` explicitly opts in:

```toml
[web]
port = 8787
kv_backend = "sqlite"
kv_path = "data/web-state.sqlite"
```

The existing `web` template and all manifests without `kv_backend`
continue to use ephemeral in-memory KV. A configured SQLite failure prevents
the server starting or makes the affected request fail with HTTP 500; it never
falls back to memory. The response body is intentionally generic, with detailed
storage errors confined to server logs. SQLite is opt-in and requires the
`sqlite` Cargo feature, included in `server` and `release-cli`.

The routed *development* listener now binds to `127.0.0.1` by default.
The sample POST endpoint is unauthenticated and is not a production API.

**Durability scope:** Each individual Web.kv_set or Web.kv_delete completes
only after SQLite acknowledges it, using WAL and FULL synchronous mode.
A request containing multiple writes is **not** a database transaction.
There is no automatic rollback across web effects or atomic handoff to
actor/workflow transitions. Do not infer exactly-once external effects,
replication, HA, per-tenant separation, authorization, backups or full
production hosting support from this sample.

CI's `scripts/test_service_template.py` exercises the generated service
with the release and lean server CLI profiles, asserting a successful web
write is still present after process SIGKILL/restart. It also injects a
SQLite-triggered write error and asserts HTTP 500 with no hidden memory write.
`tests/web_sqlite_state.rs` verifies adapter persistence and SQL parameters.

## Remaining work

Before production use, define capability-scoped access to persistent web
state, explicit multi-operation transactions and tenant isolation, robust
backup/restore, and a hardened deployment server. Durable-job dispatch must
continue to use Nulang's canonical RFC 0022 transition coordinator after
the existing workflow recovery P0 issues are resolved. Do not create
a second workflow engine.
