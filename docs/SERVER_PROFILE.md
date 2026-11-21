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

## Next integration slice

Once the release gates above are green, provide a **single-host service
template** using existing HTTP and SQLite APIs, followed by a crash-tested
SQLite job dispatcher built on the canonical atomic durable-transition
coordinator. Do not create a second workflow engine or promise implicit
exactly-once execution of arbitrary external effects.
