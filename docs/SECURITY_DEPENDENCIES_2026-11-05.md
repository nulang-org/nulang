# November 2026 dependency audit remediation

Date: 2026-11-05. Baseline: Nulang `main` (`448cfa8c`).

## Decision and compatibility impact

The RustSec scan originally reported ten vulnerabilities, including
`RUSTSEC-2026-0382` in two distinct rustls-webpki versions. Patch updates
address the latest rustls-webpki version, ctutils, libz-sys, Wasmtime, and
webpki-roots. A subsequent lockfile update also removes an obsolete h2
advisory and addresses unsound tokio and yanked packages where compatible
patches are available.

The remaining rustls-webpki 0.102.x is pulled only by the **remote** libSQL
feature (`hyper-rustls 0.25` → `rustls 0.22`). The latest published libSQL
0.10.0 still pins this obsolete TLS stack; no compatible patch release
exists. Nulang currently builds libSQL with only `core`, retaining
**local file/in-memory SQLite persistence**. The public
`LibsqlStore::new_remote(url, auth_token)` method remains available as
an API shape but returns `io::ErrorKind::Unsupported` deterministically.

**Breaking change:** Connecting to a Turso/libSQL remote database through
this constructor is deliberately disabled. Host applications depending on
remote libSQL must continue using an external adapter with an independently
patched TLS stack until Nulang implements and tests a secure remote client.
Never fall back to HTTP/plaintext, weaken certificate validation, or enable
`libsql/remote` and `libsql/tls` just to restore the old behavior.

## Remaining audit warnings

The local audit uses `cargo audit` without any advisory `ignore` entries.
It reports **zero vulnerabilities** and still surfaces maintenance warnings:

- `RUSTSEC-2026-0186`: `memmap2 0.2.3`, pulled through
  `cranelift-jit 0.132.x`. The audited unsound operations are not exercised
  in known Nulang JIT paths, but this is not equivalent to an upstream fix.
  Reassess when Cranelift migrates to a patched memmap2.
- `RUSTSEC-2025-0134`: `rustls-pemfile 2.2.0` is unmaintained and still
  used by Nulang's own TLS material loader. Migrate to the maintained
  rustls-pki-types PEM API with certificate and key parsing regression tests.

The security gate intentionally does not use `--deny warnings`. Do not
claim that warning-only findings have been remediated. Keep monitoring new
RustSec advisories and the upstream libSQL TLS dependency graph.

## Verification requirements

- `cargo audit` must exit zero with **no** advisory ignores.
- `cargo test --no-default-features --features sqlite --test libsql_secure_transport`
  verifies the remote constructor fails explicitly and local persistence works.
- `cargo test --no-default-features --features sqlite --lib` should preserve
  SQLite atomic transition and persistence regressions.
- `cargo check --no-default-features --features wasm-backend` verifies that
  updated Wasmtime 48.0.8 remains buildable.
- Full-feature/release builds must run in a CI environment with OpenSSL
  development headers and `pkg-config` installed; Gertrude's local
  environment currently lacks the `pkg-config` executable.

The security-first remote disablement is a **draft review decision**, not a
permission to merge past hosted CI or without acknowledging that affected
Turso users must migrate their connection path.
