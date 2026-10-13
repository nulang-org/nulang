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

## HTTP service template (stacked preview)

The new `service` package template provides an HTTP homepage and a liveness
endpoint through the existing routed-web development server:

```bash
nulang nula new my-service --template service
cd my-service
nulang nula dev
# In a second shell:
curl --fail http://127.0.0.1:8787/health
```

This template is currently **stateless**. The web development server's
`Web.kv_*` effects are **not** SQLite-backed and do not persist across
restarts; the `server` Cargo feature simply makes the separate SQLite
persistence adapter available in the binary. Do not expose `nula dev` as a
production-ready public server.

CI's `scripts/test_service_template.py` will scaffold the template,
connect through loopback to an ephemeral dev-server port, and check both HTTP
routes using **both** the release CLI and the lean `server` feature profile.
The current development listener may bind all interfaces; run this smoke in
isolated CI. It is not a production security or durability test.

## Next integration slice

Design an explicit **runtime-owned request-to-persistence boundary** with
structured errors and tenant/capability enforcement before connecting web
operations to SQLite. The later durable-job dispatcher must reuse the canonical
atomic durable-transition coordinator and pass crash/restart testing. Do not
create a second workflow engine or promise exactly-once execution of arbitrary
external effects.
