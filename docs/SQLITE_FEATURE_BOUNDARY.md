# Local SQLite and optional remote libSQL transport

**Compatibility notice:** This is a staged, review-only change. Historical
`sqlite` / `full` / `release-cli` builds implicitly enabled the remote libSQL
transport. They now provide **local persistence only**. Code using
`LibsqlStore::new_remote` still compiles but returns `io::ErrorKind::Unsupported`
unless the explicit `sqlite-remote` feature is enabled. This runtime behavior
change requires a consumer inventory and approval before release.

| Build profile | Command | Remote SQL transport |
| --- | --- | --- |
| Local SQLite | `cargo build --locked --no-default-features --features sqlite` | Disabled |
| Default CLI | `cargo build --locked` | Disabled |
| Remote libSQL compatibility | `cargo build --locked --no-default-features --features sqlite-remote` | Enabled, **legacy** |
| Everything (review only) | `cargo check --locked --all-features` | Enabled, **legacy** |

The local profile uses `libsql/core` only; it does not link the legacy
`hyper 0.14` / `h2 0.3.27` dependency graph. The remote compatibility profile
explicitly adds `libsql/remote` and the verified native TLS connector. It
continues to enforce HTTPS, certificate chains, hostname verification and
fail-closed URL admission in the existing connector.

**Security limitation:** `sqlite-remote` **still includes** the vulnerable
`h2 0.3.27` package (RUSTSEC-2026-0258). `Cargo.lock` intentionally retains
that package because the remote feature remains resolvable. `cargo audit` scans
the entire lockfile, *not* just a chosen build's feature graph. The repository
has an existing documented RustSec exception for this legacy h2 branch; a
green configured audit is **not** an unsuppressed clean audit. Do not expose
unreviewed production workloads to the legacy remote transport merely because
the local/default profile excludes it.

## Qualification and promotion

- `python3 -m unittest scripts.tests.test_sqlite_feature_boundary` proves
  that local/default feature graphs exclude legacy h2, while remote opt-in
  retains it and therefore remains subject to separate security review.
- `cargo test --locked --no-default-features --features sqlite --test sqlite_feature_boundary`
  proves the public remote constructor fails closed without the opt-in feature.
- `cargo test --locked --no-default-features --features sqlite-remote --lib runtime::libsql_tls::tests`
  preserves the remote transport's HTTPS and URL-rejection tests.
- Full default and all-features suites, release artifacts, and exact-head hosted
  CI must succeed on the proposed final integration tree.
- Before any production merge, inventory consumers of `LibsqlStore::new_remote`,
  validate disposable authenticated nonproduction Turso SQL operations and
  obtain independent security approval. Prefer migrating the remote transport
  to a maintained HTTP client behind an external capability boundary rather
  than permanently retaining legacy Hyper 0.14 in the core language runtime.
