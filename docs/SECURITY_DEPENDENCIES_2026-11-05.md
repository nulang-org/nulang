# November 2026 dependency audit remediation

Date: 2026-11-05. Baseline: Nulang `main` (`448cfa8c`).

## Decision and compatibility impact

The RustSec scan originally reported ten vulnerabilities, including
`RUSTSEC-2026-0382` in two distinct rustls-webpki versions. Patch updates
address the latest rustls-webpki version, ctutils, libz-sys, Wasmtime, and
webpki-roots. A subsequent lockfile update also removes an obsolete h2
advisory and addresses unsound tokio and yanked packages where compatible
patches are available.

The original TLS finding was pulled through the **remote** libSQL
feature (`hyper-rustls 0.25` → `rustls 0.22`). Published libSQL 0.10
dependencies pinned this obsolete TLS stack. As of the November 10 draft
SQLite-provider consolidation, Nulang removes the embedded libSQL FFI
backend entirely. Local actor and AI persistence now share one
`rusqlite 0.41.1` / `libsqlite3-sys 0.39.1` provider (bundled SQLite 3.54.1).
The historic public `LibsqlStore` type name remains for compatibility;
`LibsqlStore::new_remote(url, auth_token)` still returns
`io::ErrorKind::Unsupported` deterministically.

**Breaking change:** Connecting to a Turso/libSQL remote database through
this constructor is deliberately disabled. Host applications depending on
remote libSQL must continue using an external adapter with an independently
patched TLS stack until Nulang implements and tests a secure remote client.
Never fall back to HTTP/plaintext, weaken certificate validation, or enable
`libsql/remote` and `libsql/tls` just to restore the old behavior.

## November 10 follow-up: one verified local SQLite engine

The original `libsql-ffi 0.10.0-pre.4` also bundled SQLite 3.47.0,
predating the 3.54.0 upstream fix for the crafted database/WAL
memory-disclosure advisory (CVE-2026-119375). Adding patched `rusqlite`
alongside it produced duplicate `sqlite3_*` symbols in default and
release builds (issue #2549). The consolidation removes `libsql`
and its own FFI bundle rather than suppressing linker or security checks.

The adapter in `src/runtime/sqlite_compat.rs` maps the existing
`LibsqlStore` local operations to `rusqlite`, preserving transactional
atomicity and SQL schema compatibility while allowing the public API
to migrate separately. The adapter currently buffers each result set
into owned rows, matching the store's materialized query API but
adding transient allocation pressure for large queries. Avoid
unbounded queries in security-sensitive use.

Required release evidence: the same patched SQLite runtime version
for both stores, legacy-schema migrations, atomic rollback and
recovery tests, default-feature/feature-combination builds,
full release linker validation, RustSec audit, and terminal exact-head
CI. A successful `cargo audit` **alone is not merge authorization**.

## Remaining audit warnings

The local audit uses `cargo audit` without any advisory `ignore` entries.
The original single-provider integration reported **zero vulnerabilities** and two warnings.
The follow-up PEM-parser draft reports **zero vulnerabilities** and one warning:

- `RUSTSEC-2026-0186`: `memmap2 0.2.3`, pulled through
  `cranelift-jit 0.132.x`. The audited unsound operations are not exercised
  in known Nulang JIT paths, but this is not equivalent to an upstream fix.
  Reassess when Cranelift migrates to a patched memmap2.
The direct `rustls-pemfile 2.2.0` dependency (RUSTSEC-2025-0134,
previously an unmaintained-crate warning) is removed in the follow-up PEM
parser draft. Nulang's TLS material loader now uses the already-resolved
`rustls::pki_types::pem::PemObject` API, retaining error handling for malformed
certificate chains, recognized private-key encodings, and certificate-derived
node identities. This change requires exact-head hosted CI and mutual TLS
integration testing before promotion.

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
