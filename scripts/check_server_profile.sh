#!/usr/bin/env bash
# Validate the small single-host server profile without changing the default CLI.
set -euo pipefail

echo "==> Build bytecode server CLI without optional tooling"
cargo check --locked --no-default-features --features server --bin nulang

echo "==> Check the profile contract"
cargo test --locked --no-default-features --features server --test server_feature_profile

echo "==> Verify SQLite activation epochs and durable-effect recovery"
cargo test --locked --no-default-features --features server --test durable_activation_epoch
cargo test --locked --no-default-features --features server --test durable_effect_recovery_store

echo "==> Audit production/build dependencies of server profile"
cargo tree --locked --no-default-features --features server --edges normal,build --prefix none --format '{p}' |
    python3 -c '
import sys
names = {line.split()[0] for line in sys.stdin if line.strip()}
forbidden = {
    "pyo3", "pyo3-ffi", "nulang-ai", "tower-lsp", "wasmtime",
    "wasm-encoder", "libloading", "libffi", "libffi-sys",
    "rocksdb", "librocksdb-sys", "postgres", "tokio-postgres",
    "mio",
}
unexpected = sorted(name for name in names if name in forbidden or name.startswith("cranelift"))
if unexpected:
    sys.exit("server profile includes optional tooling: " + ", ".join(unexpected))
print("server dependency graph excludes JIT, Python, AI, LSP, FFI, external storage")
'

echo "server profile: OK"
